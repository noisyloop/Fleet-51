//! One bot = one event log on disk + one supervisor task that runs its agent process
//! (`fleet51 agent`) and restarts it when the model or Think setting changes.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::{broadcast, mpsc};

use crate::daemon::Daemon;
use crate::events::{Ev, Event, Live, State, Status};
use crate::home::{now_ms, short_id};

pub enum Cmd {
    Prompt {
        id: String,
        text: String,
        /// Sent by this automation; the bot runs it as a standalone task.
        automation: Option<String>,
        attachments: Vec<String>,
    },
    /// Stop for good (the bot was deleted or the user stopped it).
    Stop,
    /// Stop because the daemon is handing over to a newer one; it resumes there.
    Suspend,
    /// Pick up a new model or Think setting, keeping the conversation.
    Restart,
}

enum Exit {
    Exited(Option<i32>),
    Stopped,
    Suspended,
    Restart,
    SpawnFailed(String),
}

struct Inner {
    state: State,
    file: File,
}

pub struct Session {
    pub id: String,
    pub dir: PathBuf,
    inner: Mutex<Inner>,
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
                // Lines from older Fleet-51 versions that no longer parse are skipped.
                if let Ok(e) = serde_json::from_str::<Event>(&line) {
                    state.apply(&e);
                }
            }
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Arc::new(Session {
            id: id.to_string(),
            dir,
            inner: Mutex::new(Inner { state, file }),
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
        let _ = self.global.send(Live::Event {
            session: self.id.clone(),
            event: event.clone(),
        });
        let _ = self.global.send(Live::State(Box::new(state)));
        event
    }

    pub fn state(&self) -> State {
        self.inner.lock().unwrap().state.clone()
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Events after `since`, leaving out anything before the chat was last cleared.
    pub fn history(&self, since: u64) -> Vec<Event> {
        let since = since.max(self.state().cleared_seq);
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

    /// Queue a message. Holding the command lock while appending means a concurrent (re)start
    /// either sees it in `pending` or receives it on the channel, never both.
    pub fn queue_prompt(
        &self,
        text: &str,
        automation: Option<&str>,
        attachments: &[String],
    ) -> (String, bool) {
        let guard = self.cmd.lock().unwrap();
        let id = short_id("p");
        self.emit(Ev::PromptQueued {
            id: id.clone(),
            text: text.to_string(),
            automation: automation.map(str::to_string),
            attachments: attachments.to_vec(),
        });
        let live = guard.as_ref().is_some_and(|tx| {
            tx.send(Cmd::Prompt {
                id: id.clone(),
                text: text.to_string(),
                automation: automation.map(str::to_string),
                attachments: attachments.to_vec(),
            })
            .is_ok()
        });
        (id, live)
    }

    fn attach(&self, tx: mpsc::UnboundedSender<Cmd>) {
        let mut guard = self.cmd.lock().unwrap();
        for p in self.state().pending {
            let _ = tx.send(Cmd::Prompt {
                id: p.id,
                text: p.text,
                automation: p.automation,
                attachments: p.attachments,
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

fn requeue_interrupted(s: &Session) {
    if let Some(text) = s.state().open_turn {
        s.emit(Ev::Note {
            level: "info".into(),
            message: "the last reply was interrupted; asking again".into(),
        });
        s.queue_prompt(&text, None, &[]);
    }
}

async fn supervise(d: &Arc<Daemon>, s: &Arc<Session>, mut resume: Option<String>) {
    if matches!(
        s.state().status,
        Status::Orphaned | Status::Suspended | Status::Failed
    ) {
        requeue_interrupted(s);
    }
    loop {
        let (tx, rx) = mpsc::unbounded_channel();
        s.attach(tx);
        let exit = run_agent(d, s, resume.clone(), rx).await;
        s.detach();
        match exit {
            Exit::Restart => {
                resume = s.state().provider_session;
                requeue_interrupted(s);
            }
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
                s.emit(Ev::Note {
                    level: "error".into(),
                    message: format!("could not start the bot: {e}"),
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

/// Run one agent process and translate its JSON lines into events.
async fn run_agent(
    d: &Daemon,
    s: &Arc<Session>,
    resume: Option<String>,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
) -> Exit {
    let st = s.state();
    let overlay = s.dir.join("agent");
    let _ = std::fs::create_dir_all(&overlay);
    let mut cmd = tokio::process::Command::new(&d.exe);
    #[cfg(windows)]
    cmd.creation_flags(crate::home::CREATE_NO_WINDOW);
    cmd.arg("agent").arg("--state-dir").arg(&overlay);
    if let Some(m) = &st.model {
        cmd.args(["--model", m]);
    }
    if st.run_code {
        cmd.arg("--run-code");
    }
    if st.think {
        cmd.arg("--think");
    }
    if let Some(r) = &resume {
        cmd.args(["--resume", r]);
    }
    if let Some(root) = crate::runtime::root() {
        cmd.env("OLLAMA_HOST", root);
    }
    cmd.current_dir(&st.cwd)
        .env("FLEET51_SESSION", &s.id)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    if let Err(e) = std::fs::create_dir_all(&st.cwd) {
        return Exit::SpawnFailed(format!("{}: {e}", st.cwd));
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Exit::SpawnFailed(e.to_string()),
    };
    s.emit(Ev::Spawned {
        pid: child.id(),
        generation: d.generation,
        resume,
    });
    let mut stdin = child.stdin.take();
    let mut lines = tokio::io::BufReader::new(child.stdout.take().expect("piped stdout")).lines();
    // Input goes through a pipe and is buffered until the agent reads it: no readiness race.
    s.emit(Ev::Ready);

    let mut inflight: VecDeque<String> = VecDeque::new();
    let mut outcome: Option<Exit> = None;
    let mut open = true;
    loop {
        tokio::select! {
            line = lines.next_line() => match line {
                Ok(Some(l)) => handle_line(s, &l, &mut inflight),
                _ => break,
            },
            cmd = rx.recv(), if open => match cmd {
                Some(Cmd::Prompt { id, text, automation, attachments }) => {
                    let msg = json!({"type": "user", "automation": automation, "attachments": attachments, "message": {"role": "user", "content": [{"type": "text", "text": text}]}});
                    let ok = match stdin.as_mut() {
                        Some(w) => w.write_all(format!("{msg}\n").as_bytes()).await.is_ok() && w.flush().await.is_ok(),
                        None => false,
                    };
                    if ok {
                        s.emit(Ev::PromptDelivered { id: id.clone() });
                        inflight.push_back(id);
                    }
                }
                Some(c) => {
                    outcome.get_or_insert(match c {
                        Cmd::Suspend => Exit::Suspended,
                        Cmd::Restart => Exit::Restart,
                        _ => Exit::Stopped,
                    });
                    stdin = None;
                    let _ = child.start_kill();
                }
                None => open = false,
            },
        }
    }
    let code = child.wait().await.ok().and_then(|st| st.code());
    outcome.unwrap_or(Exit::Exited(code))
}

fn handle_line(s: &Session, line: &str, inflight: &mut VecDeque<String>) {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return;
    };
    match v["type"].as_str() {
        Some("system") if v["subtype"] == "init" => {
            if let Some(id) = v["session_id"].as_str() {
                if s.state().provider_session.as_deref() != Some(id) {
                    s.emit(Ev::ProviderSession { id: id.to_string() });
                }
            }
        }
        Some("system") if v["subtype"] == "checkpoint" => {
            if let Some(id) = v["id"].as_str() {
                s.emit(Ev::Checkpoint { id: id.to_string() });
            }
        }
        Some("system") if v["subtype"] == "checkpoint_failed" => {
            s.emit(Ev::Note {
                level: "warn".into(),
                message: v["message"]
                    .as_str()
                    .unwrap_or("Undo is not available")
                    .into(),
            });
        }
        Some("system") if v["subtype"] == "notice" => {
            if let Some(m) = v["message"].as_str() {
                s.emit(Ev::Note {
                    level: "warn".into(),
                    message: m.into(),
                });
            }
        }
        Some("user") if v["isReplay"] == true => {
            if let Some(id) = inflight.pop_front() {
                s.emit(Ev::PromptAcked { id });
            }
        }
        Some("assistant") => {
            for b in v["message"]["content"].as_array().into_iter().flatten() {
                match b["type"].as_str() {
                    Some("thinking") => {
                        let t = b["thinking"].as_str().unwrap_or_default();
                        if !t.trim().is_empty() {
                            s.emit(Ev::Reasoning {
                                text: t.to_string(),
                            });
                        }
                    }
                    Some("text") => {
                        let t = b["text"].as_str().unwrap_or_default();
                        if !t.trim().is_empty() {
                            s.emit(Ev::Text {
                                text: t.to_string(),
                            });
                        }
                    }
                    Some("tool_use") => {
                        s.emit(Ev::ToolCall {
                            name: b["name"].as_str().unwrap_or("tool").to_string(),
                            input: b["input"].clone(),
                        });
                    }
                    _ => {}
                }
            }
        }
        Some("result") => {
            let is_err = v["is_error"].as_bool().unwrap_or(false);
            s.emit(Ev::TurnDone {
                ok: !is_err,
                summary: v["result"].as_str().map(|t| t.chars().take(600).collect()),
                input_tokens: v["usage"]["input_tokens"].as_u64().unwrap_or(0),
                output_tokens: v["usage"]["output_tokens"].as_u64().unwrap_or(0),
            });
        }
        _ => {}
    }
}
