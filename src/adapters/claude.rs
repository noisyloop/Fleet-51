//! Claude Code `--input-format stream-json --output-format stream-json`.

use std::collections::VecDeque;
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc::UnboundedReceiver;

use super::{truncate, watch_stderr, Cmd, Exit, Spec};
use crate::events::Ev;
use crate::session::Session;

pub async fn run(spec: Spec, s: Arc<Session>, mut rx: UnboundedReceiver<Cmd>) -> Exit {
    let mut child = match spec.command(&spec.argv).spawn() {
        Ok(c) => c,
        Err(e) => return Exit::SpawnFailed(format!("{}: {e}", spec.argv[0])),
    };
    spec.spawned(&s, child.id(), &spec.argv);
    let mut stdin = child.stdin.take();
    let mut lines = BufReader::new(child.stdout.take().expect("piped stdout")).lines();
    let re = spec.manifest.rate_limit_re();
    let stderr_hit = watch_stderr(&s, child.stderr.take(), re.clone());
    // Input goes through a pipe, so it is buffered until the agent reads it: no readiness race.
    s.emit(Ev::Ready);

    let mut inflight: VecDeque<String> = VecDeque::new();
    let mut outcome: Option<Exit> = None;
    let mut open = true;
    loop {
        tokio::select! {
            line = lines.next_line() => match line {
                Ok(Some(l)) => {
                    if let Some(x) = handle_line(&s, &l, &mut inflight, re.as_ref()) {
                        outcome.get_or_insert(x);
                        let _ = child.start_kill();
                    }
                }
                _ => break,
            },
            cmd = rx.recv(), if open => match cmd {
                Some(Cmd::Prompt { id, text }) => {
                    let msg = json!({"type": "user", "message": {"role": "user", "content": [{"type": "text", "text": text}]}});
                    let ok = match stdin.as_mut() {
                        Some(w) => w.write_all(format!("{msg}\n").as_bytes()).await.is_ok() && w.flush().await.is_ok(),
                        None => false,
                    };
                    if ok {
                        s.emit(Ev::PromptDelivered { id: id.clone() });
                        inflight.push_back(id);
                    } else {
                        s.emit(Ev::Note { level: "error".into(), message: format!("could not deliver prompt {id}; it stays queued") });
                    }
                }
                Some(Cmd::Input(_)) => {
                    s.emit(Ev::Note { level: "info".into(), message: "raw terminal input is not used by structured agents; send a prompt".into() });
                }
                Some(Cmd::Stop) => {
                    outcome.get_or_insert(Exit::Stopped);
                    stdin = None;
                    let _ = child.start_kill();
                }
                Some(Cmd::Suspend) => {
                    outcome.get_or_insert(Exit::Suspended);
                    stdin = None;
                    let _ = child.start_kill();
                }
                None => open = false,
            },
        }
    }
    let code = child.wait().await.ok().and_then(|st| st.code());
    if let Some(o) = outcome {
        return o;
    }
    if let Some(msg) = stderr_hit.lock().unwrap().take() {
        return Exit::RateLimited(msg);
    }
    Exit::Exited(code)
}

fn is_replayed_prompt(v: &Value) -> bool {
    if v["isReplay"].as_bool() == Some(true) {
        return true;
    }
    match &v["message"]["content"] {
        Value::String(_) => true,
        Value::Array(blocks) => blocks.iter().any(|b| b["type"] == "text") && !blocks.iter().any(|b| b["type"] == "tool_result"),
        _ => false,
    }
}

fn handle_line(s: &Session, line: &str, inflight: &mut VecDeque<String>, re: Option<&regex::Regex>) -> Option<Exit> {
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => {
            if !line.trim().is_empty() {
                s.emit(Ev::Note { level: "stdout".into(), message: line.to_string() });
            }
            return None;
        }
    };
    match v["type"].as_str() {
        Some("system") if v["subtype"] == "init" => {
            if let Some(id) = v["session_id"].as_str() {
                s.emit(Ev::ProviderSession { id: id.to_string() });
            }
        }
        Some("user") => {
            if is_replayed_prompt(&v) {
                if let Some(id) = inflight.pop_front() {
                    s.emit(Ev::PromptAcked { id });
                }
            }
        }
        Some("assistant") => {
            for b in v["message"]["content"].as_array().into_iter().flatten() {
                match b["type"].as_str() {
                    Some("text") => {
                        let t = b["text"].as_str().unwrap_or_default();
                        if !t.trim().is_empty() {
                            s.emit(Ev::Text { text: t.to_string() });
                        }
                    }
                    Some("tool_use") => s.emit_tool(b["name"].as_str().unwrap_or("tool"), b["input"].clone()),
                    _ => {}
                }
            }
        }
        Some("result") => {
            // Older CLIs without --replay-user-messages: a finished turn proves receipt.
            if let Some(id) = inflight.pop_front() {
                s.emit(Ev::PromptAcked { id });
            }
            let is_err = v["is_error"].as_bool().unwrap_or(false) || v["subtype"].as_str().is_some_and(|st| st != "success");
            let text = v["result"].as_str().map(str::to_string).unwrap_or_else(|| v["subtype"].to_string());
            if is_err && re.is_some_and(|r| r.is_match(&text)) {
                return Some(Exit::RateLimited(text));
            }
            let u = &v["usage"];
            let input = u["input_tokens"].as_u64().unwrap_or(0)
                + u["cache_read_input_tokens"].as_u64().unwrap_or(0)
                + u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
            s.emit(Ev::TurnDone {
                ok: !is_err,
                summary: Some(truncate(&text, 600)),
                input_tokens: input,
                output_tokens: u["output_tokens"].as_u64().unwrap_or(0),
                cost_usd: v["total_cost_usd"].as_f64().unwrap_or(0.0),
            });
        }
        _ => {}
    }
    None
}
