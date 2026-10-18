//! fleet51 daemon: one per user, generation-numbered. Starting a new generation asks the old one
//! to suspend its sessions and exit, then resumes them, so upgrades never leave orphan daemons.
//! Any process left behind by a crashed generation is found through the event log and reaped.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, Notify};

use crate::adapters::Cmd;
use crate::events::{Ev, Live, Status};
use crate::home::{kill_pid, now_ms, pid_alive, pid_belongs_to_session, short_id, Home};
use crate::manifest::{self, Manifest};
use crate::review;
use crate::session::{self, Session};
use crate::workspace::{self, RepoSpec, Workspace};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub pid: u32,
    pub generation: u64,
    pub url: String,
    pub started: u64,
}

pub fn read_info(home: &Home) -> Option<DaemonInfo> {
    std::fs::read_to_string(home.daemon_file())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
}

pub struct Daemon {
    pub home: Home,
    pub generation: u64,
    pub token: String,
    pub url: String,
    pub exe: PathBuf,
    pub sessions: RwLock<HashMap<String, Arc<Session>>>,
    pub global: broadcast::Sender<Live>,
    pub stop: Notify,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct NewSession {
    pub agent: String,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub repos: Vec<RepoSpec>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub sandbox: Option<String>,
    #[serde(default)]
    pub permission: Option<String>,
    #[serde(default)]
    pub account: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub group: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Fanout {
    pub agents: Vec<String>,
    /// Optional: try each agent with each of these models.
    #[serde(default)]
    pub models: Vec<String>,
    pub repos: Vec<RepoSpec>,
    pub prompt: String,
    #[serde(default = "one")]
    pub n: usize,
    #[serde(default)]
    pub sandbox: Option<String>,
    #[serde(default)]
    pub permission: Option<String>,
    #[serde(default)]
    pub account: Option<String>,
}

fn one() -> usize {
    1
}

impl Daemon {
    pub fn manifests(&self) -> Vec<Manifest> {
        manifest::load_all(&self.home)
    }

    pub fn manifest(&self, name: &str) -> Option<Manifest> {
        self.manifests().into_iter().find(|m| m.name == name)
    }

    pub fn all_sessions(&self) -> Vec<Arc<Session>> {
        let mut v: Vec<_> = self.sessions.read().unwrap().values().cloned().collect();
        v.sort_by_key(|s| s.state().created);
        v
    }

    pub fn session(&self, id: &str) -> Result<Arc<Session>> {
        let all = self.sessions.read().unwrap();
        let hits: Vec<_> = all
            .values()
            .filter(|s| s.id.starts_with(id))
            .cloned()
            .collect();
        match hits.len() {
            1 => Ok(hits.into_iter().next().unwrap()),
            0 => bail!("no session {id}"),
            _ => bail!("{id} matches several sessions"),
        }
    }

    pub fn workspace_of(&self, s: &Session) -> Result<Workspace> {
        workspace::get(&self.home, &s.state().workspace)
    }

    pub fn create_session(self: &Arc<Self>, req: NewSession) -> Result<Arc<Session>> {
        let m = self
            .manifest(&req.agent)
            .with_context(|| format!("unknown agent {}", req.agent))?;
        let sandbox = req.sandbox.clone().unwrap_or_else(|| "none".into());
        if !crate::sandbox::TIERS.contains(&sandbox.as_str()) {
            bail!("unknown sandbox {sandbox}");
        }
        let title = req
            .title
            .clone()
            .or_else(|| {
                req.prompt
                    .as_ref()
                    .map(|p| crate::adapters::truncate(p.lines().next().unwrap_or(""), 60))
            })
            .unwrap_or_else(|| m.display.clone());
        let ws = match (&req.workspace, req.repos.is_empty()) {
            (Some(w), _) => workspace::get(&self.home, w)?,
            (None, false) => workspace::create(&self.home, &title, &req.repos)?,
            (None, true) => bail!("give a workspace id or at least one repo"),
        };
        let id = short_id("s");
        let s = Session::open(self.home.sessions().join(&id), &id, self.global.clone())?;
        s.emit(Ev::Created {
            agent: m.name.clone(),
            workspace: ws.id.clone(),
            cwd: ws.agent_cwd().display().to_string(),
            title,
            sandbox,
            permission: req.permission.clone().unwrap_or_else(|| "auto".into()),
            account: req.account.clone(),
            model: req.model.clone(),
            group: req.group.clone(),
        });
        if let Some(p) = req.prompt.as_deref().filter(|p| !p.trim().is_empty()) {
            s.queue_prompt(p);
        }
        self.sessions.write().unwrap().insert(id, s.clone());
        session::start(self.clone(), s.clone(), None);
        Ok(s)
    }

    /// Fan one prompt out to several agents, each in its own fresh workspace.
    pub fn fanout(self: &Arc<Self>, req: Fanout) -> Result<(String, Vec<Arc<Session>>)> {
        let group = short_id("g");
        let mut out = vec![];
        let models: Vec<Option<String>> = if req.models.is_empty() {
            vec![None]
        } else {
            req.models.iter().cloned().map(Some).collect()
        };
        for agent in &req.agents {
            for model in &models {
                for i in 0..req.n.max(1) {
                    let who = model
                        .as_ref()
                        .map(|m| format!("{agent} · {m}"))
                        .unwrap_or_else(|| agent.clone());
                    let title = format!(
                        "{who} #{} · {}",
                        i + 1,
                        crate::adapters::truncate(req.prompt.lines().next().unwrap_or(""), 40)
                    );
                    out.push(self.create_session(NewSession {
                        agent: agent.clone(),
                        model: model.clone(),
                        repos: req.repos.clone(),
                        prompt: Some(req.prompt.clone()),
                        sandbox: req.sandbox.clone(),
                        permission: req.permission.clone(),
                        account: req.account.clone(),
                        title: Some(title),
                        group: Some(group.clone()),
                        ..Default::default()
                    })?);
                }
            }
        }
        Ok((group, out))
    }

    /// Queue a prompt; if the agent is not running, resuming it is the explicit user action.
    pub fn prompt(self: &Arc<Self>, s: &Arc<Session>, text: &str) -> String {
        let (id, live) = s.queue_prompt(text);
        if !live && !s.is_running() {
            session::start(self.clone(), s.clone(), s.state().provider_session);
        }
        id
    }

    pub fn stop(&self, s: &Arc<Session>) {
        s.emit(Ev::StopRequested);
        if !s.send(Cmd::Stop)
            && !s.is_running()
            && !matches!(s.state().status, Status::Exited | Status::Failed)
        {
            // Parked (suspended/orphaned): mark it finished so it is never auto-resumed.
            s.emit(Ev::Exited {
                code: None,
                clean: true,
            });
        }
    }

    pub fn resume(self: &Arc<Self>, s: &Arc<Session>) -> Result<()> {
        if s.is_running() {
            bail!("session is already running");
        }
        session::start(self.clone(), s.clone(), s.state().provider_session);
        Ok(())
    }

    pub fn feedback(self: &Arc<Self>, s: &Arc<Session>) -> Result<String> {
        let notes: Vec<_> = s
            .state()
            .annotations
            .into_iter()
            .filter(|a| !a.sent)
            .collect();
        if notes.is_empty() {
            bail!("no unsent annotations");
        }
        let ws = self.workspace_of(s)?;
        let text = review::feedback_prompt(&ws, &notes);
        let pid = self.prompt(s, &text);
        s.emit(Ev::FeedbackSent {
            annotations: notes.iter().map(|a| a.id.clone()).collect(),
            prompt_id: pid.clone(),
        });
        Ok(pid)
    }

    pub fn merge(&self, s: &Arc<Session>) -> Result<Vec<review::MergeResult>> {
        if s.is_running() && s.state().status == Status::Working {
            bail!("agent is mid-turn; wait for it to finish or stop it first");
        }
        let ws = self.workspace_of(s)?;
        let st = s.state();
        let results = review::merge(
            &ws,
            &format!("{} ({}, Fleet-51 session {})", st.title, st.agent, st.id),
        )?;
        for r in &results {
            s.emit(Ev::Merged {
                checkout: r.checkout.clone(),
                branch: r.branch.clone(),
                into: r.into.clone(),
                commit: r.commit.clone(),
            });
        }
        Ok(results)
    }

    pub async fn suspend_all(&self) {
        let running: Vec<_> = self
            .all_sessions()
            .into_iter()
            .filter(|s| s.is_running())
            .collect();
        for s in &running {
            s.send(Cmd::Suspend);
        }
        for _ in 0..100 {
            if running.iter().all(|s| !s.is_running()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

async fn hand_off(home: &Home, token: &str) -> u64 {
    let Some(prev) = read_info(home) else {
        return 1;
    };
    let next = prev.generation + 1;
    if prev.pid == std::process::id() || !pid_alive(prev.pid) {
        return next;
    }
    eprintln!(
        "fleet51: asking generation {} (pid {}) to hand off",
        prev.generation, prev.pid
    );
    let url = format!("{}/api/shutdown", prev.url);
    let auth = format!("Bearer {token}");
    let _ = tokio::task::spawn_blocking(move || {
        ureq::post(&url).header("Authorization", &auth).send_empty()
    })
    .await;
    for _ in 0..150 {
        if !pid_alive(prev.pid) {
            return next;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    eprintln!(
        "fleet51: generation {} did not exit; terminating it",
        prev.generation
    );
    kill_pid(prev.pid);
    next
}

pub async fn serve(home: Home, listen: &str) -> Result<()> {
    home.ensure()?;
    let token = home.token()?;
    let generation = hand_off(&home, &token).await;
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    let addr = listener.local_addr()?;
    let host = if addr.ip().is_unspecified() {
        "127.0.0.1".to_string()
    } else {
        addr.ip().to_string()
    };
    let url = format!("http://{host}:{}", addr.port());
    let info = DaemonInfo {
        pid: std::process::id(),
        generation,
        url: url.clone(),
        started: now_ms(),
    };
    std::fs::write(home.daemon_file(), serde_json::to_string_pretty(&info)?)?;

    let (global, _) = broadcast::channel(4096);
    let d = Arc::new(Daemon {
        home: home.clone(),
        generation,
        token: token.clone(),
        url: url.clone(),
        exe: std::env::current_exe()?,
        sessions: RwLock::new(HashMap::new()),
        global,
        stop: Notify::new(),
    });

    // Recover sessions from their logs.
    let mut resumed = 0;
    for entry in std::fs::read_dir(home.sessions())?.flatten() {
        let id = entry.file_name().to_string_lossy().to_string();
        let s = match Session::open(entry.path(), &id, d.global.clone()) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let st = s.state();
        if st.status.is_live() {
            if let Some(pid) = st
                .pid
                .filter(|p| pid_alive(*p) && pid_belongs_to_session(*p, &id))
            {
                kill_pid(pid);
                s.emit(Ev::Note {
                    level: "warn".into(),
                    message: format!("reaped pid {pid} left by a crashed daemon"),
                });
            }
            s.emit(Ev::Orphaned { generation });
        }
        let auto = s.state().should_auto_resume();
        d.sessions.write().unwrap().insert(id, s.clone());
        if auto {
            resumed += 1;
            session::start(d.clone(), s.clone(), st.provider_session.clone());
        }
    }

    // Only one generation may run: exit if a newer one has taken over the lock file.
    {
        let d = d.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                if read_info(&d.home).is_some_and(|i| i.generation > d.generation) {
                    d.suspend_all().await;
                    d.stop.notify_one();
                    return;
                }
            }
        });
    }

    println!("Fleet-51 daemon generation {generation} listening on {url}");
    println!("Web UI: {url}/?token={token}");
    if resumed > 0 {
        println!("Resumed {resumed} interrupted session(s)");
    }
    let app = crate::server::router(d.clone());
    let d2 = d.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move { d2.stop.notified().await })
        .await?;
    if read_info(&home).is_some_and(|i| i.pid == std::process::id()) {
        let _ = std::fs::remove_file(home.daemon_file());
    }
    Ok(())
}
