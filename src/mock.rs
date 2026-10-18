//! `fleet51 mock-agent`: an offline agent that speaks each supported protocol and really edits
//! files, so the whole loop (launch → prompt → status → diff → feedback → merge) runs with no
//! API keys. Knobs, for demos and tests:
//!   - a prompt containing "crash-once" kills the agent mid-turn the first time it is seen
//!   - FLEET51_MOCK_RATE_LIMIT=acct1,acct2 makes those accounts report a rate limit

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use serde_json::{json, Value};

fn out(v: Value) {
    let mut o = std::io::stdout().lock();
    let _ = writeln!(o, "{v}");
    let _ = o.flush();
}

fn pause() {
    std::thread::sleep(Duration::from_millis(250));
}

fn account() -> String {
    std::env::var("MOCK_ACCOUNT_DIR")
        .ok()
        .and_then(|d| {
            Path::new(&d)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| "default".into())
}

fn rate_limited() -> bool {
    let acct = account();
    std::env::var("FLEET51_MOCK_RATE_LIMIT")
        .map(|v| v.split(',').any(|a| a.trim() == acct))
        .unwrap_or(false)
}

fn crash_once(prompt: &str) {
    let marker = PathBuf::from(".mock-crashed");
    if prompt.contains("crash-once") && !marker.exists() {
        let _ = std::fs::write(&marker, "");
        eprintln!("mock agent: simulated crash mid-turn");
        std::process::exit(3);
    }
}

/// Do the "work": append the prompt to a markdown file named after it. In a multi-repo
/// workspace root, do it in every repo so the cross-repo diff has something to show.
fn work(prompt: &str) -> String {
    let repos: Vec<PathBuf> = std::fs::read_dir(".")
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join(".git").exists())
        .collect();
    if !Path::new(".git").exists() && !repos.is_empty() {
        return repos
            .iter()
            .map(|r| work_in(r, prompt))
            .collect::<Vec<_>>()
            .join(", ");
    }
    work_in(Path::new(""), prompt)
}

fn work_in(dir: &Path, prompt: &str) -> String {
    let slug: String = prompt
        .split_whitespace()
        .take(4)
        .collect::<Vec<_>>()
        .join("-")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug = if slug.trim_matches('-').is_empty() {
        "note".to_string()
    } else {
        slug.trim_matches('-').to_string()
    };
    let path = dir.join("mock").join(format!("{slug}.md"));
    let _ = std::fs::create_dir_all(dir.join("mock"));
    let mut text = std::fs::read_to_string(&path).unwrap_or_else(|_| format!("# {slug}\n\n"));
    text.push_str(&format!(
        "- [{}] {}\n",
        account(),
        prompt.lines().next().unwrap_or("")
    ));
    let _ = std::fs::write(&path, text);
    path.display().to_string()
}

fn run_stop_hooks(settings: Option<&str>) {
    let Some(v) = settings
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    else {
        return;
    };
    for group in v["hooks"]["Stop"].as_array().into_iter().flatten() {
        for h in group["hooks"].as_array().into_iter().flatten() {
            if let Some(cmd) = h["command"].as_str() {
                let _ = std::process::Command::new("sh")
                    .arg("-c")
                    .arg(cmd)
                    .stdin(std::process::Stdio::null())
                    .status();
            }
        }
    }
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

pub fn run(protocol: &str, args: Vec<String>) -> Result<()> {
    match protocol {
        "claude" => claude(&args),
        "codex" => codex(&args),
        "acp" => acp(),
        "pty" => pty(),
        other => anyhow::bail!("unknown mock protocol {other}"),
    }
}

fn claude(args: &[String]) -> Result<()> {
    let session = flag(args, "--resume").unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let settings = flag(args, "--settings");
    let replay = args.iter().any(|a| a == "--replay-user-messages");
    let mut started = false;
    let mut turn = 0;
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if msg["type"] != "user" {
            continue;
        }
        let text = match &msg["message"]["content"] {
            Value::String(s) => s.clone(),
            Value::Array(b) => b
                .iter()
                .filter_map(|x| x["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        if !started {
            started = true;
            out(
                json!({"type": "system", "subtype": "init", "session_id": session, "model": "mock", "cwd": std::env::current_dir()?}),
            );
        }
        if replay {
            out(
                json!({"type": "user", "message": msg["message"], "session_id": session, "isReplay": true}),
            );
        }
        turn += 1;
        pause();
        if rate_limited() {
            out(
                json!({"type": "result", "subtype": "error_during_execution", "is_error": true, "session_id": session,
                       "result": format!("Mock usage limit reached for account {} (rate limit)", account())}),
            );
            continue;
        }
        out(
            json!({"type": "assistant", "session_id": session, "message": {"role": "assistant", "content": [{"type": "text", "text": format!("On it ({}): {}", account(), text.lines().next().unwrap_or(""))}]}}),
        );
        pause();
        crash_once(&text);
        let file = work(&text);
        out(
            json!({"type": "assistant", "session_id": session, "message": {"role": "assistant", "content": [{"type": "tool_use", "id": format!("t{turn}"), "name": "Write", "input": {"file_path": file}}]}}),
        );
        pause();
        out(
            json!({"type": "user", "session_id": session, "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": format!("t{turn}"), "content": "ok"}]}}),
        );
        let reply = format!("Done. Updated {file}.");
        out(
            json!({"type": "assistant", "session_id": session, "message": {"role": "assistant", "content": [{"type": "text", "text": reply}]}}),
        );
        run_stop_hooks(settings.as_deref());
        out(
            json!({"type": "result", "subtype": "success", "is_error": false, "session_id": session, "result": reply,
                   "usage": {"input_tokens": 120 + text.len(), "output_tokens": 48}, "total_cost_usd": 0.0012}),
        );
    }
    Ok(())
}

fn codex(args: &[String]) -> Result<()> {
    let thread = flag(args, "resume").unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let prompt = args.last().cloned().unwrap_or_default();
    out(json!({"type": "thread.started", "thread_id": thread}));
    out(json!({"type": "turn.started"}));
    pause();
    if rate_limited() {
        out(
            json!({"type": "turn.failed", "error": {"message": format!("rate limit reached for account {}", account())}}),
        );
        std::process::exit(1);
    }
    out(
        json!({"type": "item.completed", "item": {"id": "i0", "type": "agent_message", "text": format!("Planning ({}): {}", account(), prompt.lines().next().unwrap_or(""))}}),
    );
    pause();
    crash_once(&prompt);
    out(
        json!({"type": "item.completed", "item": {"id": "i1", "type": "command_execution", "command": "ls", "exit_code": 0}}),
    );
    let file = work(&prompt);
    out(
        json!({"type": "item.completed", "item": {"id": "i2", "type": "file_change", "changes": [{"path": file, "kind": "update"}]}}),
    );
    out(
        json!({"type": "item.completed", "item": {"id": "i3", "type": "agent_message", "text": format!("Done. Updated {file}.")}}),
    );
    out(
        json!({"type": "turn.completed", "usage": {"input_tokens": 200 + prompt.len(), "cached_input_tokens": 0, "output_tokens": 64}}),
    );
    Ok(())
}

fn acp() -> Result<()> {
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    let mut next_req = 0;
    while let Some(line) = lines.next() {
        let Ok(msg) = serde_json::from_str::<Value>(&line?) else {
            continue;
        };
        let id = msg["id"].clone();
        match msg["method"].as_str() {
            Some("initialize") => out(
                json!({"jsonrpc": "2.0", "id": id, "result": {"protocolVersion": 1, "agentCapabilities": {"loadSession": true}}}),
            ),
            Some("session/new") => out(
                json!({"jsonrpc": "2.0", "id": id, "result": {"sessionId": uuid::Uuid::new_v4().to_string()}}),
            ),
            Some("session/load") => out(json!({"jsonrpc": "2.0", "id": id, "result": null})),
            Some("session/prompt") => {
                let sid = msg["params"]["sessionId"].clone();
                let text: String = msg["params"]["prompt"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|b| b["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                let update = |u: Value| {
                    out(
                        json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": sid, "update": u}}),
                    )
                };
                if rate_limited() {
                    out(
                        json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": "rate limit reached"}}),
                    );
                    continue;
                }
                update(
                    json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "On it: "}}),
                );
                update(
                    json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": format!("{}\n", text.lines().next().unwrap_or(""))}}),
                );
                pause();
                crash_once(&text);
                next_req += 1;
                let perm_id = format!("perm-{next_req}");
                out(
                    json!({"jsonrpc": "2.0", "id": perm_id, "method": "session/request_permission", "params": {
                        "sessionId": sid,
                        "toolCall": {"toolCallId": "w1", "title": "Write mock notes", "kind": "edit"},
                        "options": [{"optionId": "allow", "name": "Allow", "kind": "allow_once"}, {"optionId": "reject", "name": "Reject", "kind": "reject_once"}]
                    }}),
                );
                let mut allowed = false;
                for l in lines.by_ref() {
                    let Ok(r) = serde_json::from_str::<Value>(&l?) else {
                        continue;
                    };
                    if r["id"] == perm_id.as_str() {
                        allowed = r["result"]["outcome"]["optionId"] == "allow";
                        break;
                    }
                }
                if allowed {
                    let file = work(&text);
                    update(
                        json!({"sessionUpdate": "tool_call", "toolCallId": "w1", "title": format!("Write {file}"), "kind": "edit", "status": "completed", "rawInput": {"path": file}}),
                    );
                    update(
                        json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": format!("Done. Updated {file}.")}}),
                    );
                } else {
                    update(
                        json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Permission denied, so I made no changes."}}),
                    );
                }
                out(json!({"jsonrpc": "2.0", "id": id, "result": {"stopReason": "end_turn"}}));
            }
            Some(_) if !id.is_null() => out(
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "unsupported"}}),
            ),
            _ => {}
        }
    }
    Ok(())
}

fn pty() -> Result<()> {
    let mut o = std::io::stdout();
    writeln!(o, "Booting mock agent (slow start, 2s)…")?;
    o.flush()?;
    std::thread::sleep(Duration::from_secs(2));
    write!(o, "mock> ")?;
    o.flush()?;
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        let text = line.trim();
        if !text.is_empty() {
            writeln!(o, "working on: {text}")?;
            o.flush()?;
            std::thread::sleep(Duration::from_millis(500));
            crash_once(text);
            let file = work(text);
            writeln!(o, "wrote {file}")?;
        }
        write!(o, "mock> ")?;
        o.flush()?;
    }
    Ok(())
}
