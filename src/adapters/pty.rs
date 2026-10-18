//! Fallback for TUI-only agents. Prompts are held until the agent paints its declared readiness
//! marker; there is no "wait N ms then type" timer anywhere, so a slow start cannot eat a prompt.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use regex::Regex;
use tokio::sync::{mpsc, oneshot};

use super::{Cmd, Exit, Spec};
use crate::events::Ev;
use crate::session::Session;

pub fn strip_ansi(s: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(\x07|\x1b\\)|\x1b[@-Z\\-_]")
            .unwrap()
    });
    re.replace_all(s, "").replace('\r', "")
}

pub async fn run(spec: Spec, s: Arc<Session>, mut rx: mpsc::UnboundedReceiver<Cmd>) -> Exit {
    let rules = spec.manifest.pty.clone();
    let ready_re = rules
        .as_ref()
        .and_then(|r| Regex::new(&r.ready_pattern).ok());
    let idle_re = rules
        .as_ref()
        .and_then(|r| r.idle_pattern.as_deref())
        .and_then(|p| Regex::new(p).ok());
    let submit = rules
        .as_ref()
        .map(|r| r.submit.clone())
        .unwrap_or_else(|| "\r".into());

    let argv = spec.wrap(&spec.argv);
    let pair = match native_pty_system().openpty(PtySize {
        rows: 40,
        cols: 120,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(p) => p,
        Err(e) => return Exit::SpawnFailed(format!("openpty: {e}")),
    };
    let mut cb = CommandBuilder::new(&argv[0]);
    for a in &argv[1..] {
        cb.arg(a);
    }
    cb.cwd(&spec.cwd);
    for (k, v) in &spec.env {
        cb.env(k, v);
    }
    let mut child = match pair.slave.spawn_command(cb) {
        Ok(c) => c,
        Err(e) => return Exit::SpawnFailed(format!("{}: {e}", argv[0])),
    };
    drop(pair.slave);
    spec.spawned(&s, child.process_id(), &spec.argv);
    let mut killer = child.clone_killer();
    let mut reader = match pair.master.try_clone_reader() {
        Ok(r) => r,
        Err(e) => return Exit::SpawnFailed(format!("pty reader: {e}")),
    };
    let mut writer = match pair.master.take_writer() {
        Ok(w) => w,
        Err(e) => return Exit::SpawnFailed(format!("pty writer: {e}")),
    };

    let (otx, mut orx) = mpsc::unbounded_channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if otx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    let (etx, mut erx) = oneshot::channel::<Option<i32>>();
    std::thread::spawn(move || {
        let st = child.wait();
        let _ = etx.send(st.ok().map(|s| s.exit_code() as i32));
    });

    let started = Instant::now();
    let mut tail = String::new();
    let mut ready = ready_re.is_none();
    if ready {
        s.emit(Ev::Ready);
    }
    let mut busy: Option<(String, String, bool)> = None; // prompt id, echo needle, acked
    let mut queue: VecDeque<(String, String)> = VecDeque::new();
    let mut warned = false;
    let mut outcome: Option<Exit> = None;
    let mut open = true;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let code = loop {
        if ready && busy.is_none() {
            if let Some((id, text)) = queue.pop_front() {
                let ok = writer.write_all(text.as_bytes()).is_ok()
                    && writer.write_all(submit.as_bytes()).is_ok()
                    && writer.flush().is_ok();
                if ok {
                    s.emit(Ev::PromptDelivered { id: id.clone() });
                    let needle: String = text.chars().take(24).collect();
                    busy = Some((id, needle, false));
                    tail.clear();
                } else {
                    queue.push_front((id, text));
                }
            }
        }
        tokio::select! {
            Some(chunk) = orx.recv() => {
                let text = String::from_utf8_lossy(&chunk).to_string();
                s.emit(Ev::Output { data: text.clone() });
                tail.push_str(&strip_ansi(&text));
                if tail.len() > 4000 {
                    let cut = tail.len() - 2000;
                    let cut = (cut..tail.len()).find(|i| tail.is_char_boundary(*i)).unwrap_or(0);
                    tail.drain(..cut);
                }
                if !ready {
                    if ready_re.as_ref().is_some_and(|r| r.is_match(&tail)) {
                        ready = true;
                        s.emit(Ev::Ready);
                        tail.clear();
                    }
                } else if let Some((id, needle, acked)) = busy.as_mut() {
                    if !*acked && tail.contains(needle.as_str()) {
                        *acked = true;
                        s.emit(Ev::PromptAcked { id: id.clone() });
                    }
                    if *acked && idle_re.as_ref().is_some_and(|r| r.is_match(&tail)) {
                        s.emit(Ev::TurnDone { ok: true, summary: None, input_tokens: 0, output_tokens: 0, cost_usd: 0.0 });
                        busy = None;
                        tail.clear();
                    }
                }
            }
            cmd = rx.recv(), if open => match cmd {
                Some(Cmd::Prompt { id, text }) => queue.push_back((id, text)),
                Some(Cmd::Input(data)) => { let _ = writer.write_all(data.as_bytes()); let _ = writer.flush(); }
                Some(Cmd::Stop) => { outcome.get_or_insert(Exit::Stopped); let _ = killer.kill(); }
                Some(Cmd::Suspend) => { outcome.get_or_insert(Exit::Suspended); let _ = killer.kill(); }
                None => open = false,
            },
            code = &mut erx => break code.ok().flatten(),
            _ = tick.tick() => {
                if !ready && !warned && started.elapsed() > Duration::from_secs(30) {
                    warned = true;
                    s.emit(Ev::Note { level: "warn".into(), message: format!("{} has not shown its readiness marker after 30s; {} prompt(s) held, not typed blind", spec.manifest.name, queue.len()) });
                }
            }
        }
    };
    // Drain whatever the agent printed before exiting.
    while let Ok(chunk) = orx.try_recv() {
        s.emit(Ev::Output {
            data: String::from_utf8_lossy(&chunk).to_string(),
        });
    }
    outcome.unwrap_or(Exit::Exited(code))
}
