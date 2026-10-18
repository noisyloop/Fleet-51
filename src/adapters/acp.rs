//! Agent Client Protocol (JSON-RPC 2.0 over stdio). One adapter covers every ACP agent.

use std::collections::VecDeque;
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::ChildStdin;
use tokio::sync::mpsc::UnboundedReceiver;

use super::{truncate, watch_stderr, Cmd, Exit, Spec};
use crate::events::Ev;
use crate::session::Session;

struct Rpc {
    w: ChildStdin,
    next: u64,
}

impl Rpc {
    async fn send(&mut self, v: Value) -> bool {
        self.w.write_all(format!("{v}\n").as_bytes()).await.is_ok() && self.w.flush().await.is_ok()
    }
    async fn request(&mut self, method: &str, params: Value) -> u64 {
        self.next += 1;
        let id = self.next;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        id
    }
    async fn respond(&mut self, id: Value, result: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "result": result}))
            .await;
    }
    async fn respond_err(&mut self, id: Value, code: i64, message: &str) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}))
            .await;
    }
}

#[derive(PartialEq)]
enum Phase {
    Init,
    Opening,
    Ready,
}

pub async fn run(spec: Spec, s: Arc<Session>, mut rx: UnboundedReceiver<Cmd>) -> Exit {
    let mut child = match spec.command(&spec.argv).spawn() {
        Ok(c) => c,
        Err(e) => return Exit::SpawnFailed(format!("{}: {e}", spec.argv[0])),
    };
    spec.spawned(&s, child.id(), &spec.argv);
    let mut rpc = Rpc {
        w: child.stdin.take().expect("piped stdin"),
        next: 0,
    };
    let re = spec.manifest.rate_limit_re();
    let stderr_hit = watch_stderr(&s, child.stderr.take(), re.clone());
    let mut lines = BufReader::new(child.stdout.take().expect("piped stdout")).lines();
    let cwd = spec.cwd.display().to_string();

    let mut phase = Phase::Init;
    let mut init_id = rpc
        .request("initialize", json!({"protocolVersion": 1, "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false}}))
        .await;
    let mut open_id = 0u64;
    let mut loading = false;
    let mut session_id = String::new();
    let mut queue: VecDeque<(String, String)> = VecDeque::new();
    // (rpc id, prompt id, acked)
    let mut current: Option<(u64, String, bool)> = None;
    let mut chunk = String::new();
    let mut outcome: Option<Exit> = None;
    let mut open = true;

    loop {
        if phase == Phase::Ready && current.is_none() {
            if let Some((pid, text)) = queue.pop_front() {
                let rid = rpc.request("session/prompt", json!({"sessionId": session_id, "prompt": [{"type": "text", "text": text}]})).await;
                s.emit(Ev::PromptDelivered { id: pid.clone() });
                current = Some((rid, pid, false));
            }
        }
        tokio::select! {
            line = lines.next_line() => {
                let l = match line { Ok(Some(l)) => l, _ => break };
                let Ok(v) = serde_json::from_str::<Value>(&l) else { continue };
                let method = v["method"].as_str();
                let id = v.get("id").cloned();
                match (method, id) {
                    // Response to one of our requests.
                    (None, Some(id)) => {
                        let id = id.as_u64().unwrap_or(0);
                        if let Some(err) = v.get("error") {
                            let msg = err["message"].as_str().unwrap_or("error").to_string();
                            if re.as_ref().is_some_and(|r| r.is_match(&msg)) {
                                outcome.get_or_insert(Exit::RateLimited(msg));
                                let _ = child.start_kill();
                                continue;
                            }
                            if current.as_ref().is_some_and(|c| c.0 == id) {
                                let (_, pid, acked) = current.take().unwrap();
                                if !acked { s.emit(Ev::PromptAcked { id: pid }); }
                                s.emit(Ev::TurnDone { ok: false, summary: Some(msg), input_tokens: 0, output_tokens: 0, cost_usd: 0.0 });
                            } else {
                                s.emit(Ev::Note { level: "error".into(), message: format!("ACP request failed: {msg}") });
                                if id == init_id || id == open_id {
                                    outcome.get_or_insert(Exit::Exited(None));
                                    let _ = child.start_kill();
                                }
                            }
                            continue;
                        }
                        let result = &v["result"];
                        if id == init_id && phase == Phase::Init {
                            init_id = 0;
                            phase = Phase::Opening;
                            let can_load = result["agentCapabilities"]["loadSession"].as_bool().unwrap_or(false);
                            match (&spec.resume, can_load) {
                                (Some(sid), true) => {
                                    session_id = sid.clone();
                                    loading = true;
                                    open_id = rpc.request("session/load", json!({"sessionId": sid, "cwd": cwd, "mcpServers": []})).await;
                                }
                                _ => open_id = rpc.request("session/new", json!({"cwd": cwd, "mcpServers": []})).await,
                            }
                        } else if id == open_id && phase == Phase::Opening {
                            if let Some(sid) = result["sessionId"].as_str() { session_id = sid.to_string(); }
                            loading = false;
                            phase = Phase::Ready;
                            s.emit(Ev::ProviderSession { id: session_id.clone() });
                            s.emit(Ev::Ready);
                        } else if current.as_ref().is_some_and(|c| c.0 == id) {
                            let (_, pid, acked) = current.take().unwrap();
                            if !acked { s.emit(Ev::PromptAcked { id: pid }); }
                            flush(&s, &mut chunk);
                            let stop = result["stopReason"].as_str().unwrap_or("end_turn");
                            s.emit(Ev::TurnDone {
                                ok: stop == "end_turn",
                                summary: s.state().last_text.map(|t| truncate(&t, 600)),
                                input_tokens: 0, output_tokens: 0, cost_usd: 0.0,
                            });
                        }
                    }
                    (Some("session/update"), None) => {
                        if loading { continue; }
                        if let Some(c) = current.as_mut() {
                            if !c.2 { c.2 = true; s.emit(Ev::PromptAcked { id: c.1.clone() }); }
                        }
                        let u = &v["params"]["update"];
                        match u["sessionUpdate"].as_str() {
                            Some("agent_message_chunk") => chunk.push_str(u["content"]["text"].as_str().unwrap_or_default()),
                            Some("tool_call") => {
                                flush(&s, &mut chunk);
                                let name = u["title"].as_str().or(u["kind"].as_str()).unwrap_or("tool");
                                s.emit_tool(name, u.get("rawInput").cloned().unwrap_or(Value::Null));
                            }
                            Some("plan") => { s.emit(Ev::Note { level: "plan".into(), message: u["entries"].to_string() }); }
                            _ => {}
                        }
                    }
                    (Some("session/request_permission"), Some(rid)) => {
                        let opts = v["params"]["options"].as_array().cloned().unwrap_or_default();
                        let allow = spec.permission != "plan";
                        let want = if allow { "allow" } else { "reject" };
                        let pick = opts.iter().find(|o| o["kind"].as_str().is_some_and(|k| k.starts_with(want)));
                        let tool = v["params"]["toolCall"]["title"].as_str().unwrap_or("tool").to_string();
                        match pick.and_then(|o| o["optionId"].as_str()) {
                            Some(opt) => {
                                s.emit(Ev::Permission { tool, decision: opt.to_string() });
                                rpc.respond(rid, json!({"outcome": {"outcome": "selected", "optionId": opt}})).await;
                            }
                            None => {
                                s.emit(Ev::Permission { tool, decision: "cancelled".into() });
                                rpc.respond(rid, json!({"outcome": {"outcome": "cancelled"}})).await;
                            }
                        }
                    }
                    (Some(_), Some(rid)) => rpc.respond_err(rid, -32601, "method not supported by Fleet-51").await,
                    _ => {}
                }
            },
            cmd = rx.recv(), if open => match cmd {
                Some(Cmd::Prompt { id, text }) => queue.push_back((id, text)),
                Some(Cmd::Input(_)) => {}
                Some(Cmd::Stop) => { outcome.get_or_insert(Exit::Stopped); let _ = child.start_kill(); }
                Some(Cmd::Suspend) => { outcome.get_or_insert(Exit::Suspended); let _ = child.start_kill(); }
                None => open = false,
            },
        }
    }
    flush(&s, &mut chunk);
    let code = child.wait().await.ok().and_then(|st| st.code());
    if let Some(o) = outcome {
        return o;
    }
    if let Some(m) = stderr_hit.lock().unwrap().take() {
        return Exit::RateLimited(m);
    }
    Exit::Exited(code)
}

fn flush(s: &Session, chunk: &mut String) {
    if !chunk.trim().is_empty() {
        s.emit(Ev::Text {
            text: std::mem::take(chunk),
        });
    }
    chunk.clear();
}
