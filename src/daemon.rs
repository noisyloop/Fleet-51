//! fleet51 daemon: one per user, generation-numbered. It owns every bot, the model engine and
//! model downloads. Starting a newer build hands over from the old daemon (its bots suspend and
//! resume here), so an update never leaves an old version running.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, Notify};

use crate::events::{Ev, Live};
use crate::home::{kill_pid, now_ms, pid_alive, pid_belongs_to_session, short_id, Home};
use crate::models::Pull;
use crate::runtime::Runtime;
use crate::session::{self, Cmd, Session};

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

/// Identifies this exact binary, so `fleet51 up` can tell an older daemon is still running.
pub fn build_id() -> String {
    let stamp = std::env::current_exe()
        .and_then(std::fs::metadata)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{}-{stamp}", env!("CARGO_PKG_VERSION"))
}

pub const BOT_SHAPES: &[&str] = &[
    "circle", "cloud", "squircle", "sparkle", "flower", "clover", "pebble", "star", "drop", "gem",
    "blossom", "burst", "pillow",
];
pub const BOT_COLORS: &[&str] = &[
    "white", "brown", "red", "orange", "amber", "green", "teal", "blue", "purple", "pink", "gray",
];

/// Where bots keep their files: ~/Fleet-51/<bot name> (FLEET51_BOTS_DIR overrides).
fn bots_root() -> PathBuf {
    std::env::var_os("FLEET51_BOTS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| ".".into())).join("Fleet-51")
        })
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
    pub pulls: Mutex<HashMap<String, Pull>>,
    pub runtime: Mutex<Runtime>,
    pub ollama: Mutex<Option<std::process::Child>>,
    pub automations: crate::automations::Automations,
    /// Whether the internet is reachable. Bots work either way; only downloads need it.
    pub online: AtomicBool,
}

impl Daemon {
    /// Bots that have not been deleted, newest first.
    pub fn bots(&self) -> Vec<Arc<Session>> {
        let mut v: Vec<_> = self
            .sessions
            .read()
            .unwrap()
            .values()
            .filter(|s| {
                let st = s.state();
                st.profile.is_some() && !st.archived
            })
            .cloned()
            .collect();
        v.sort_by_key(|s| std::cmp::Reverse(s.state().created));
        v
    }

    pub fn bot(&self, id: &str) -> Result<Arc<Session>> {
        self.bots()
            .into_iter()
            .find(|s| s.id == id)
            .with_context(|| format!("no bot {id}"))
    }

    /// The model new bots use: the default if installed (or nothing is), else the first installed.
    pub fn default_model(&self) -> String {
        let installed = crate::agent::ollama_models().unwrap_or_default();
        let d = crate::runtime::DEFAULT_MODEL;
        if installed.is_empty() || installed.iter().any(|m| m == d) {
            d.to_string()
        } else {
            installed[0].clone()
        }
    }

    pub fn create_bot(
        self: &Arc<Self>,
        name: &str,
        shape: &str,
        color: &str,
    ) -> Result<Arc<Session>> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 60 {
            bail!("give the bot a name (up to 60 characters)");
        }
        let shape = if BOT_SHAPES.contains(&shape) {
            shape
        } else {
            BOT_SHAPES[0]
        };
        let color = if BOT_COLORS.contains(&color) {
            color
        } else {
            "green"
        };
        let slug: String = name
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || " -_".contains(c) {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let slug = slug.trim().trim_matches('.').to_string();
        let slug = if slug.is_empty() {
            "bot".to_string()
        } else {
            slug
        };
        let base = bots_root();
        let mut folder = base.join(&slug);
        let mut n = 2;
        while folder.exists() {
            folder = base.join(format!("{slug} {n}"));
            n += 1;
        }
        std::fs::create_dir_all(&folder)?;
        // Only you (and the bot, inside it) can read a bot's files.
        crate::home::private_dir(&base);
        crate::home::private_dir(&folder);
        let id = short_id("s");
        let s = Session::open(self.home.sessions().join(&id), &id, self.global.clone())?;
        s.emit(Ev::Created {
            cwd: folder.display().to_string(),
            title: name.into(),
            model: Some(self.default_model()),
        });
        s.emit(Ev::Profile {
            name: name.into(),
            shape: shape.into(),
            color: color.into(),
        });
        self.sessions.write().unwrap().insert(id, s.clone());
        session::start(self.clone(), s.clone(), None);
        Ok(s)
    }

    /// Send a message; a bot that is not running is started again to answer it.
    pub fn prompt(
        self: &Arc<Self>,
        s: &Arc<Session>,
        text: &str,
        automation: Option<&str>,
    ) -> String {
        let (id, live) = s.queue_prompt(text, automation);
        if !live && !s.is_running() {
            session::start(self.clone(), s.clone(), s.state().provider_session);
        }
        id
    }

    pub fn set_model(&self, s: &Arc<Session>, model: &str) -> Result<()> {
        if !crate::models::valid_name(model) {
            bail!("not a valid model name: {model}");
        }
        s.emit(Ev::ModelChanged {
            model: model.into(),
        });
        s.send(Cmd::Restart);
        Ok(())
    }

    pub fn set_think(&self, s: &Arc<Session>, think: bool) {
        s.emit(Ev::ThinkChanged { think });
        s.send(Cmd::Restart);
    }

    /// Put the bot's folder back as it was at a checkpoint (before one of its replies).
    pub fn undo(&self, s: &Arc<Session>, id: &str) -> Result<()> {
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_digit()) {
            bail!("not an undo point: {id}");
        }
        let st = s.state();
        if st.status == crate::events::Status::Working || !st.pending.is_empty() {
            bail!("wait for {} to finish replying, then undo", st.title);
        }
        let folder = PathBuf::from(&st.cwd).canonicalize()?;
        let root = bots_root().canonicalize()?;
        // Restoring empties the folder first, so be certain it is a bot's own folder.
        if !folder.starts_with(&root) || folder == root {
            bail!("{} is not a bot folder", folder.display());
        }
        crate::checkpoints::restore(&folder, &s.dir.join("agent").join("checkpoints").join(id))?;
        s.emit(Ev::Undone { id: id.into() });
        Ok(())
    }

    /// Start the conversation over; files stay as they are.
    pub fn clear_chat(&self, s: &Arc<Session>) {
        s.emit(Ev::Cleared);
        s.send(Cmd::Restart);
    }

    pub fn delete_bot(&self, s: &Arc<Session>) {
        crate::automations::delete_for_bot(self, &s.id);
        s.emit(Ev::Archived);
        s.emit(Ev::StopRequested);
        s.send(Cmd::Stop);
    }

    /// Start downloading a model in the background; progress goes out on the live stream.
    pub fn pull_model(self: &Arc<Self>, name: &str) -> Result<()> {
        if !crate::models::valid_name(name) {
            bail!("not a valid model name: {name}");
        }
        {
            let mut pulls = self.pulls.lock().unwrap();
            if pulls.get(name).is_some_and(|p| !p.done) {
                return Ok(());
            }
            let p = Pull {
                name: name.into(),
                status: "starting".into(),
                completed: 0,
                total: 0,
                done: false,
                error: None,
            };
            let _ = self.global.send(Live::Pull(p.clone()));
            pulls.insert(name.into(), p);
        }
        let d = self.clone();
        let name = name.to_string();
        tokio::task::spawn_blocking(move || {
            let (mut last_pct, mut last_status) = (u64::MAX, String::new());
            let result = crate::models::pull(&name, |p| {
                let pct = (p.completed * 100).checked_div(p.total).unwrap_or(0);
                if pct != last_pct || p.status != last_status {
                    last_pct = pct;
                    last_status = p.status.clone();
                    d.pulls.lock().unwrap().insert(name.clone(), p.clone());
                    let _ = d.global.send(Live::Pull(p.clone()));
                }
            });
            let mut pulls = d.pulls.lock().unwrap();
            let p = pulls.entry(name.clone()).or_insert_with(|| Pull {
                name: name.clone(),
                status: String::new(),
                completed: 0,
                total: 0,
                done: false,
                error: None,
            });
            p.done = true;
            match result {
                Ok(()) => p.status = "success".into(),
                Err(e) => {
                    p.status = "failed".into();
                    p.error = Some(e.to_string());
                }
            }
            let _ = d.global.send(Live::Pull(p.clone()));
        });
        Ok(())
    }

    /// Find or set up Ollama in the background, then make sure there is a model and a bot.
    pub fn start_runtime(self: Arc<Self>) {
        if std::env::var_os("FLEET51_NO_RUNTIME").is_some() {
            *self.runtime.lock().unwrap() = Runtime::Failed {
                message: "model engine disabled (FLEET51_NO_RUNTIME)".into(),
            };
            return;
        }
        tokio::task::spawn_blocking(move || {
            let d = self.clone();
            let report = move |r: Runtime| {
                *d.runtime.lock().unwrap() = r.clone();
                let _ = d.global.send(Live::Runtime(r));
            };
            match crate::runtime::ensure(&self.home, &report) {
                Ok(child) => {
                    *self.ollama.lock().unwrap() = child;
                    if crate::agent::ollama_models().unwrap_or_default().is_empty() {
                        let _ = self.pull_model(crate::runtime::DEFAULT_MODEL);
                    }
                    if self.bots().is_empty() {
                        if let Err(e) = self.create_bot("Fleet", "cloud", "green") {
                            eprintln!("fleet51: could not create the first bot: {e:#}");
                        }
                    }
                }
                Err(e) => report(Runtime::Failed {
                    message: format!("{e:#}"),
                }),
            }
        });
    }

    pub async fn suspend_all(&self) {
        let running: Vec<_> = self
            .sessions
            .read()
            .unwrap()
            .values()
            .filter(|s| s.is_running())
            .cloned()
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

/// Check now and then whether the internet is reachable, for the app's Offline badge.
fn watch_network(d: Arc<Daemon>) {
    tokio::spawn(async move {
        loop {
            let online = tokio::task::spawn_blocking(internet_reachable)
                .await
                .unwrap_or(false);
            if d.online.swap(online, Ordering::SeqCst) != online {
                let _ = d.global.send(Live::Network { online });
            }
            tokio::time::sleep(Duration::from_secs(if online { 60 } else { 15 })).await;
        }
    });
}

fn internet_reachable() -> bool {
    if let Ok(v) = std::env::var("FLEET51_ONLINE") {
        return v != "0";
    }
    use std::net::{TcpStream, ToSocketAddrs};
    ["registry.ollama.ai:443", "github.com:443"]
        .iter()
        .any(|host| {
            host.to_socket_addrs()
                .ok()
                .and_then(|mut a| a.next())
                .is_some_and(|addr| {
                    TcpStream::connect_timeout(&addr, Duration::from_secs(4)).is_ok()
                })
        })
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
        pulls: Mutex::new(HashMap::new()),
        runtime: Mutex::new(Runtime::Checking),
        ollama: Mutex::new(None),
        automations: crate::automations::Automations::load(&home),
        online: AtomicBool::new(true),
    });

    // Recover bots from their logs; anything that is not a bot (older developer sessions) is ignored.
    for entry in std::fs::read_dir(home.sessions())?.flatten() {
        let id = entry.file_name().to_string_lossy().to_string();
        let Ok(s) = Session::open(entry.path(), &id, d.global.clone()) else {
            continue;
        };
        let st = s.state();
        if st.profile.is_none() || st.archived {
            continue;
        }
        if st.status.is_live() {
            if let Some(pid) = st
                .pid
                .filter(|p| pid_alive(*p) && pid_belongs_to_session(*p, &id))
            {
                kill_pid(pid);
            }
            s.emit(Ev::Orphaned { generation });
        }
        let auto = s.state().should_auto_resume();
        d.sessions.write().unwrap().insert(id, s.clone());
        if auto {
            session::start(d.clone(), s.clone(), st.provider_session.clone());
        }
    }

    // Only one generation may run: exit if a newer one has taken over.
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
    println!("Open: {url}/?token={token}");
    d.clone().start_runtime();
    crate::automations::start(d.clone());
    watch_network(d.clone());
    let app = crate::server::router(d.clone());
    let d2 = d.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move { d2.stop.notified().await })
        .await?;
    if let Some(mut child) = d.ollama.lock().unwrap().take() {
        let _ = child.kill();
        let _ = child.wait();
    }
    if read_info(&home).is_some_and(|i| i.pid == std::process::id()) {
        let _ = std::fs::remove_file(home.daemon_file());
    }
    Ok(())
}
