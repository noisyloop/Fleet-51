//! One session = one event log on disk + one supervisor task that (re)spawns the agent.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc};

use crate::accounts;
use crate::adapters::{self, Cmd, Exit, Spec};
use crate::daemon::Daemon;
use crate::events::{Ev, Event, Live, State, Status};
use crate::home::{now_ms, short_id};
use crate::manifest::{expand, expand_all, Manifest, Protocol};
use crate::sandbox;
use crate::workspace;

struct Inner {
    state: State,
    file: File,
}

pub struct Session {
    pub id: String,
    pub dir: PathBuf,
    inner: Mutex<Inner>,
    pub tx: broadcast::Sender<Event>,
    global: broadcast::Sender<Live>,
    cmd: Mutex<Option<mpsc::UnboundedSender<Cmd>>>,
    running: AtomicBool,
}

impl Session {
    pub fn open(dir: PathBuf, id: &str, global: broadcast::Sender<Live>) -> Result<Arc<Session>> {
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("events.jsonl");
        let mut state = State {
            id: id.to_string(),
            ..Default::default()
        };
        if let Ok(f) = File::open(&path) {
            for line in BufReader::new(f).lines().map_while(Result::ok) {
                if let Ok(e) = serde_json::from_str::<Event>(&line) {
                    state.apply(&e);
                }
            }
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let (tx, _) = broadcast::channel(4096);
        Ok(Arc::new(Session {
            id: id.to_string(),
            dir,
            inner: Mutex::new(Inner { state, file }),
            tx,
            global,
            cmd: Mutex::new(None),
            running: AtomicBool::new(false),
        }))
    }

    pub fn emit(&self, ev: Ev) -> Event {
        let (event, state) = {
            let mut g = self.inner.lock().unwrap();
            let e = Event {
                seq: g.state.last_seq + 1,
                ts: now_ms(),
                ev,
            };
            if let Ok(line) = serde_json::to_string(&e) {
                let _ = writeln!(g.file, "{line}");
            }
            g.state.apply(&e);
            (e, g.state.clone())
        };
        let _ = self.tx.send(event.clone());
        let _ = self.global.send(Live::Event {
            session: self.id.clone(),
            event: event.clone(),
        });
        if !matches!(event.ev, Ev::Output { .. }) {
            let _ = self.global.send(Live::State(state));
        }
        event
    }

    pub fn emit_tool(&self, name: &str, input: Value) {
        self.emit(Ev::ToolCall {
            name: name.to_string(),
            input,
        });
    }

    pub fn state(&self) -> State {
        self.inner.lock().unwrap().state.clone()
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub fn history(&self, since: u64) -> Vec<Event> {
        let Ok(f) = File::open(self.dir.join("events.jsonl")) else {
            return vec![];
        };
        BufReader::new(f)
            .lines()
            .map_while(Result::ok)
            .filter_map(|l| serde_json::from_str::<Event>(&l).ok())
            .filter(|e| e.seq > since)
            .collect()
    }

    pub fn send(&self, c: Cmd) -> bool {
        self.cmd
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|tx| tx.send(c).is_ok())
    }

    /// Queue a prompt. Holding the command lock while appending means a concurrent (re)spawn
    /// either sees it in `pending` or receives it on the channel, never both.
    pub fn queue_prompt(&self, text: &str) -> (String, bool) {
        let guard = self.cmd.lock().unwrap();
        let id = short_id("p");
        self.emit(Ev::PromptQueued {
            id: id.clone(),
            text: text.to_string(),
        });
        let live = guard.as_ref().is_some_and(|tx| {
            tx.send(Cmd::Prompt {
                id: id.clone(),
                text: text.to_string(),
            })
            .is_ok()
        });
        (id, live)
    }

    fn attach(&self, tx: mpsc::UnboundedSender<Cmd>) {
        let mut guard = self.cmd.lock().unwrap();
        let st = self.state();
        if st.pending.iter().any(|p| p.delivered) {
            self.emit(Ev::Note {
                level: "info".into(),
                message: "re-sending prompts the previous process never acknowledged".into(),
            });
        }
        for p in st.pending {
            let _ = tx.send(Cmd::Prompt {
                id: p.id,
                text: p.text,
            });
        }
        *guard = Some(tx);
    }

    fn detach(&self) {
        *self.cmd.lock().unwrap() = None;
    }
}

/// Start (or resume) the supervisor. No-op if it is already running.
pub fn start(d: Arc<Daemon>, s: Arc<Session>, resume: Option<String>) {
    if s.running.swap(true, Ordering::SeqCst) {
        return;
    }
    tokio::spawn(async move {
        supervise(&d, &s, resume).await;
        s.running.store(false, Ordering::SeqCst);
    });
}

fn requeue_interrupted(s: &Session, why: &str) {
    if let Some(text) = s.state().open_turn {
        s.emit(Ev::Note {
            level: "info".into(),
            message: format!("{why}: re-running the unfinished turn once"),
        });
        s.queue_prompt(&text);
    }
}

async fn supervise(d: &Arc<Daemon>, s: &Arc<Session>, mut resume: Option<String>) {
    if matches!(
        s.state().status,
        Status::Orphaned | Status::Suspended | Status::Failed
    ) {
        requeue_interrupted(s, "previous process ended mid-turn");
    }
    loop {
        let st = s.state();
        let Some(m) = d.manifest(&st.agent) else {
            s.emit(Ev::Note {
                level: "error".into(),
                message: format!("no agent manifest named {}", st.agent),
            });
            s.emit(Ev::Exited {
                code: None,
                clean: false,
            });
            return;
        };
        let provider = m.provider();
        let account = match st.account.as_deref() {
            Some("auto") => {
                let a = accounts::pick(&d.home, &provider, None);
                if a.is_none() {
                    s.emit(Ev::Note {
                        level: "info".into(),
                        message: format!(
                            "no {provider} accounts configured; using the agent's default login"
                        ),
                    });
                }
                a
            }
            other => other.map(str::to_string),
        };
        let spec = match build_spec(d, s, &m, &st, account.clone(), resume.clone()) {
            Ok(x) => x,
            Err(e) => {
                s.emit(Ev::Note {
                    level: "error".into(),
                    message: e.to_string(),
                });
                s.emit(Ev::Exited {
                    code: None,
                    clean: false,
                });
                return;
            }
        };
        let (tx, rx) = mpsc::unbounded_channel();
        s.attach(tx);
        let exit = adapters::run(spec, s.clone(), rx).await;
        s.detach();
        match exit {
            Exit::Stopped => {
                s.emit(Ev::Exited {
                    code: None,
                    clean: true,
                });
                return;
            }
            Exit::Exited(code) => {
                s.emit(Ev::Exited {
                    code,
                    clean: code == Some(0),
                });
                return;
            }
            Exit::Suspended => {
                s.emit(Ev::Suspended {
                    generation: d.generation,
                });
                return;
            }
            Exit::SpawnFailed(e) => {
                let hint = m
                    .install_hint
                    .as_deref()
                    .map(|h| format!(" (install: {h})"))
                    .unwrap_or_default();
                s.emit(Ev::Note {
                    level: "error".into(),
                    message: format!("could not start agent: {e}{hint}"),
                });
                s.emit(Ev::Exited {
                    code: None,
                    clean: false,
                });
                return;
            }
            Exit::RateLimited(msg) => {
                s.emit(Ev::RateLimited {
                    account: account.clone(),
                    message: msg,
                });
                if let Some(a) = &account {
                    accounts::cool_down(&d.home, &provider, a, 3600);
                }
                match account
                    .as_deref()
                    .and_then(|a| accounts::pick(&d.home, &provider, Some(a)))
                {
                    Some(next) => {
                        s.emit(Ev::AccountSwitched {
                            from: account.clone(),
                            to: next,
                        });
                        resume = if m.resume_portable {
                            s.state().provider_session
                        } else {
                            None
                        };
                        requeue_interrupted(s, "rate limited mid-turn");
                    }
                    None => {
                        s.emit(Ev::Note {
                            level: "warn".into(),
                            message:
                                "rate limited and no other account is available; session parked"
                                    .into(),
                        });
                        s.emit(Ev::Exited {
                            code: None,
                            clean: false,
                        });
                        return;
                    }
                }
            }
        }
    }
}

fn build_spec(
    d: &Daemon,
    s: &Session,
    m: &Manifest,
    st: &State,
    account: Option<String>,
    resume: Option<String>,
) -> Result<Spec> {
    let ws = workspace::get(&d.home, &st.workspace)?;
    let cwd = PathBuf::from(&st.cwd);
    let overlay = s.dir.join("overlay");
    std::fs::create_dir_all(&overlay)?;
    let exe = d.exe.display().to_string();
    let overlay_s = overlay.display().to_string();
    let model = st.model.clone().unwrap_or_default();
    let cwd_s = cwd.display().to_string();
    let resume_s = resume.clone().unwrap_or_default();
    let vars: [(&str, &str); 6] = [
        ("fleet51", &exe),
        ("id", &s.id),
        ("session", &resume_s),
        ("overlay", &overlay_s),
        ("model", &model),
        ("cwd", &cwd_s),
    ];
    // Per-session config overlay. Nothing outside ~/.fleet51 is written.
    for f in &m.overlay {
        let p = overlay.join(&f.path);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(p, expand(&f.content, &vars))?;
    }

    let mut flags = m
        .permission_args
        .get(&st.permission)
        .cloned()
        .unwrap_or_default();
    if !model.is_empty() {
        flags.extend(expand_all(&m.model_args, &vars));
    }
    let mut argv = expand_all(&m.command, &vars);
    argv.extend(expand_all(&m.args, &vars));
    if m.protocol != Protocol::CodexExecJson {
        argv.extend(flags.iter().cloned());
        if resume.is_some() {
            argv.extend(expand_all(&m.resume_args, &vars));
        }
        if !m.overlay.is_empty() {
            argv.extend(expand_all(&m.overlay_args, &vars));
        }
    }

    let mut env: Vec<(String, String)> = vec![
        ("FLEET51_SESSION".into(), s.id.clone()),
        ("FLEET51_URL".into(), d.url.clone()),
        ("FLEET51_TOKEN".into(), d.token.clone()),
    ];
    for (k, v) in &m.env {
        env.push((k.clone(), expand(v, &vars)));
    }
    let account_dir = account
        .as_ref()
        .map(|a| accounts::dir(&d.home, &m.provider(), a));
    if let (Some(var), Some(dir)) = (&m.account_env, &account_dir) {
        env.push((var.clone(), dir.display().to_string()));
    }

    let extra: Vec<PathBuf> = ws
        .checkouts
        .iter()
        .map(|c| {
            if c.kind == "git" {
                c.source.join(".git")
            } else {
                c.path.clone()
            }
        })
        .collect();
    let keys: Vec<String> = env.iter().map(|(k, _)| k.clone()).collect();
    let mounts = sandbox::Mounts {
        workspace: &ws.root,
        extra: extra.iter().map(|p| p.as_path()).collect(),
        overlay: &overlay,
        account: account_dir.as_deref(),
        exe: &d.exe,
    };
    let sandbox_prefix = sandbox::prefix(
        &st.sandbox,
        m,
        &mounts,
        &keys,
        &cwd,
        m.protocol == Protocol::Pty,
    )?;

    Ok(Spec {
        argv,
        env,
        cwd,
        manifest: m.clone(),
        resume,
        account,
        generation: d.generation,
        permission: st.permission.clone(),
        flags,
        sandbox_prefix,
    })
}
