//! Codex `exec --json`: one structured process per turn, continued with `exec resume <thread>`.

use std::collections::VecDeque;
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc::UnboundedReceiver;

use super::{truncate, watch_stderr, Cmd, Exit, Spec};
use crate::events::Ev;
use crate::manifest::expand;
use crate::session::Session;

enum TurnEnd {
    Done,
    RateLimited(String),
    Stopped,
    Suspended,
    SpawnFailed(String),
}

pub async fn run(spec: Spec, s: Arc<Session>, mut rx: UnboundedReceiver<Cmd>) -> Exit {
    spec.spawned(&s, None, &spec.manifest.command);
    s.emit(Ev::Ready);
    let mut thread = spec.resume.clone();
    let mut queue: VecDeque<(String, String)> = VecDeque::new();
    loop {
        let (id, text) = match queue.pop_front() {
            Some(p) => p,
            None => match rx.recv().await {
                Some(Cmd::Prompt { id, text }) => (id, text),
                Some(Cmd::Input(_)) => continue,
                Some(Cmd::Stop) => return Exit::Stopped,
                Some(Cmd::Suspend) => return Exit::Suspended,
                None => return Exit::Exited(Some(0)),
            },
        };
        match turn(&spec, &s, &mut rx, &mut queue, &mut thread, id, text).await {
            TurnEnd::Done => {}
            TurnEnd::RateLimited(m) => return Exit::RateLimited(m),
            TurnEnd::Stopped => return Exit::Stopped,
            TurnEnd::Suspended => return Exit::Suspended,
            TurnEnd::SpawnFailed(e) => return Exit::SpawnFailed(e),
        }
    }
}

fn turn_argv(spec: &Spec, thread: Option<&str>, prompt: &str) -> Vec<String> {
    let template = match thread {
        Some(_) if !spec.manifest.resume_prompt_args.is_empty() => &spec.manifest.resume_prompt_args,
        _ => &spec.manifest.prompt_args,
    };
    let mut argv = spec.argv.clone();
    for a in template {
        if a == "{flags}" {
            argv.extend(spec.flags.iter().cloned());
        } else {
            argv.push(expand(a, &[("session", thread.unwrap_or_default()), ("prompt", prompt)]));
        }
    }
    argv
}

async fn turn(
    spec: &Spec,
    s: &Arc<Session>,
    rx: &mut UnboundedReceiver<Cmd>,
    queue: &mut VecDeque<(String, String)>,
    thread: &mut Option<String>,
    id: String,
    text: String,
) -> TurnEnd {
    let argv = turn_argv(spec, thread.as_deref(), &text);
    let mut child = match spec.command(&argv).spawn() {
        Ok(c) => c,
        Err(e) => return TurnEnd::SpawnFailed(format!("{}: {e}", argv[0])),
    };
    drop(child.stdin.take());
    s.emit(Ev::PromptDelivered { id: id.clone() });
    let re = spec.manifest.rate_limit_re();
    let stderr_hit = watch_stderr(s, child.stderr.take(), re.clone());
    let mut lines = BufReader::new(child.stdout.take().expect("piped stdout")).lines();
    let mut acked = false;
    let mut finished = false;
    let mut end: Option<TurnEnd> = None;
    loop {
        tokio::select! {
            line = lines.next_line() => match line {
                Ok(Some(l)) => {
                    let Ok(v) = serde_json::from_str::<Value>(&l) else { continue };
                    let kind = v["type"].as_str().unwrap_or_default();
                    if !acked && matches!(kind, "thread.started" | "turn.started") {
                        acked = true;
                        s.emit(Ev::PromptAcked { id: id.clone() });
                    }
                    match kind {
                        "thread.started" => {
                            if let Some(t) = v["thread_id"].as_str() {
                                if thread.as_deref() != Some(t) {
                                    *thread = Some(t.to_string());
                                    s.emit(Ev::ProviderSession { id: t.to_string() });
                                }
                            }
                        }
                        "item.completed" => item(s, &v["item"]),
                        "turn.completed" => {
                            finished = true;
                            let u = &v["usage"];
                            s.emit(Ev::TurnDone {
                                ok: true,
                                summary: s.state().last_text.map(|t| truncate(&t, 600)),
                                input_tokens: u["input_tokens"].as_u64().unwrap_or(0),
                                output_tokens: u["output_tokens"].as_u64().unwrap_or(0),
                                cost_usd: 0.0,
                            });
                        }
                        "turn.failed" | "error" => {
                            let msg = v["error"]["message"].as_str().or(v["message"].as_str()).unwrap_or("turn failed").to_string();
                            if re.as_ref().is_some_and(|r| r.is_match(&msg)) {
                                end.get_or_insert(TurnEnd::RateLimited(msg));
                                let _ = child.start_kill();
                            } else if !finished {
                                finished = true;
                                s.emit(Ev::TurnDone { ok: false, summary: Some(msg), input_tokens: 0, output_tokens: 0, cost_usd: 0.0 });
                            }
                        }
                        _ => {}
                    }
                }
                _ => break,
            },
            cmd = rx.recv() => match cmd {
                Some(Cmd::Prompt { id, text }) => queue.push_back((id, text)),
                Some(Cmd::Stop) => { end.get_or_insert(TurnEnd::Stopped); let _ = child.start_kill(); }
                Some(Cmd::Suspend) => { end.get_or_insert(TurnEnd::Suspended); let _ = child.start_kill(); }
                Some(Cmd::Input(_)) | None => {}
            },
        }
    }
    let code = child.wait().await.ok().and_then(|st| st.code());
    if let Some(e) = end {
        return e;
    }
    if let Some(m) = stderr_hit.lock().unwrap().take() {
        if !finished {
            return TurnEnd::RateLimited(m);
        }
    }
    if !finished {
        s.emit(Ev::TurnDone { ok: false, summary: Some(format!("turn process exited with {code:?}")), input_tokens: 0, output_tokens: 0, cost_usd: 0.0 });
    }
    TurnEnd::Done
}

fn item(s: &Session, it: &Value) {
    match it["type"].as_str() {
        Some("agent_message") => {
            if let Some(t) = it["text"].as_str() {
                s.emit(Ev::Text { text: t.to_string() });
            }
        }
        Some("command_execution") => s.emit_tool("shell", json!({"command": it["command"], "exit_code": it["exit_code"]})),
        Some("file_change") => s.emit_tool("edit", json!({"changes": it["changes"]})),
        Some("mcp_tool_call") => s.emit_tool(it["tool"].as_str().unwrap_or("mcp"), it["arguments"].clone()),
        _ => {}
    }
}
