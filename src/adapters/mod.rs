//! Protocol adapters. Structured protocols are preferred; the PTY adapter is the fallback for
//! agents that only have a TUI, and even then prompts wait for a declared readiness marker.

pub mod acp;
pub mod claude;
pub mod codex;
pub mod pty;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{ChildStderr, Command};

use crate::events::Ev;
use crate::manifest::Manifest;
use crate::session::Session;

pub enum Cmd {
    Prompt { id: String, text: String },
    Input(String),
    Stop,
    Suspend,
}

#[derive(Debug)]
pub enum Exit {
    Exited(Option<i32>),
    Stopped,
    Suspended,
    RateLimited(String),
    SpawnFailed(String),
}

pub struct Spec {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
    pub manifest: Manifest,
    pub resume: Option<String>,
    pub account: Option<String>,
    pub generation: u64,
    pub permission: String,
    /// Permission and model flags, for protocols that build argv per turn.
    pub flags: Vec<String>,
    /// Sandbox wrapper (for example `docker run ... image`) put in front of every spawn.
    pub sandbox_prefix: Vec<String>,
}

impl Spec {
    pub fn wrap(&self, argv: &[String]) -> Vec<String> {
        let mut v = self.sandbox_prefix.clone();
        v.extend(argv.iter().cloned());
        v
    }

    pub fn command(&self, argv: &[String]) -> Command {
        let full = self.wrap(argv);
        let mut c = Command::new(&full[0]);
        c.args(&full[1..])
            .current_dir(&self.cwd)
            .envs(self.env.iter().cloned())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        c
    }

    pub fn spawned(&self, s: &Session, pid: Option<u32>, argv: &[String]) {
        s.emit(Ev::Spawned {
            pid,
            protocol: self.manifest.protocol.name().into(),
            argv: self.wrap(argv),
            account: self.account.clone(),
            generation: self.generation,
            resume: self.resume.clone(),
        });
    }
}

/// Surface stderr as notes and remember the last line that looks like a rate limit.
pub fn watch_stderr(
    s: &Arc<Session>,
    stderr: Option<ChildStderr>,
    re: Option<regex::Regex>,
) -> Arc<Mutex<Option<String>>> {
    let hit = Arc::new(Mutex::new(None));
    if let Some(err) = stderr {
        let s = s.clone();
        let hit2 = hit.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(err).lines();
            let mut n = 0;
            while let Ok(Some(l)) = lines.next_line().await {
                if re.as_ref().is_some_and(|r| r.is_match(&l)) {
                    *hit2.lock().unwrap() = Some(l.clone());
                }
                n += 1;
                if n <= 200 && !l.trim().is_empty() {
                    s.emit(Ev::Note {
                        level: "stderr".into(),
                        message: l,
                    });
                }
            }
        });
    }
    hit
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max).collect();
        t.push('…');
        t
    }
}

pub async fn run(
    spec: Spec,
    s: Arc<Session>,
    rx: tokio::sync::mpsc::UnboundedReceiver<Cmd>,
) -> Exit {
    use crate::manifest::Protocol::*;
    match spec.manifest.protocol {
        ClaudeStreamJson => claude::run(spec, s, rx).await,
        CodexExecJson => codex::run(spec, s, rx).await,
        Acp => acp::run(spec, s, rx).await,
        Pty => pty::run(spec, s, rx).await,
    }
}
