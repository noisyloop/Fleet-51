//! `fleet51 agent`: a small built-in coding agent for any OpenAI-compatible model server
//! (Ollama, LM Studio, vLLM, llama.cpp server, OpenRouter...). It speaks Claude's stream-json
//! protocol on stdio, so Fleet-51 drives it exactly like Claude Code: acknowledged prompts,
//! resumable sessions, tool events, usage. Tools are confined to the working directory.

use std::io::{BufRead, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

const MAX_TOOL_OUTPUT: usize = 8000;

pub struct Opts {
    pub provider: String,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub resume: Option<String>,
    pub permission: String,
    pub state_dir: Option<PathBuf>,
    pub replay: bool,
    pub max_steps: usize,
    /// Let reasoning models think before answering (much slower on small local hardware).
    pub think: bool,
    /// native | text | auto (native, falling back to text for models without tool calling)
    pub tool_mode: String,
}

fn out(v: Value) {
    let mut o = std::io::stdout().lock();
    let _ = writeln!(o, "{v}");
    let _ = o.flush();
}

/// Ollama's own OLLAMA_HOST convention: "host:port" or a full URL.
pub fn ollama_root() -> String {
    let host = std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "127.0.0.1:11434".into());
    let host = if host.starts_with("http") {
        host
    } else {
        format!("http://{host}")
    };
    host.trim_end_matches('/').to_string()
}

pub fn ollama_models() -> Result<Vec<String>> {
    let mut resp = ureq::get(&format!("{}/api/tags", ollama_root()))
        .call()
        .context("cannot reach Ollama (is `ollama serve` running?)")?;
    let v: Value = resp.body_mut().read_json()?;
    Ok(v["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["name"].as_str().map(str::to_string))
        .collect())
}

struct Endpoint {
    url: String,
    key: Option<String>,
    model: String,
    no_think: bool,
}

fn endpoint(o: &Opts) -> Result<Endpoint> {
    let (base, key) = match o.provider.as_str() {
        "ollama" => (
            o.base_url
                .clone()
                .unwrap_or_else(|| format!("{}/v1", ollama_root())),
            None,
        ),
        "openai" => (
            o.base_url
                .clone()
                .or_else(|| std::env::var("OPENAI_BASE_URL").ok())
                .unwrap_or_else(|| "https://api.openai.com/v1".into()),
            std::env::var("OPENAI_API_KEY").ok(),
        ),
        other => bail!("unknown provider {other} (use ollama or openai)"),
    };
    let model = match o.model.clone().filter(|m| !m.is_empty()) {
        Some(m) => m,
        None if o.provider == "ollama" => ollama_models()?.into_iter().next().ok_or_else(|| {
            anyhow!("Ollama has no models yet; pull one, e.g. `ollama pull qwen3`")
        })?,
        None => bail!("pass a model (fleet51 run ... --model <name>)"),
    };
    Ok(Endpoint {
        url: format!("{}/chat/completions", base.trim_end_matches('/')),
        key,
        model,
        // Ollama maps reasoning_effort "none" to think=false; other servers may reject it.
        no_think: o.provider == "ollama" && !o.think,
    })
}

fn tools(permission: &str) -> Value {
    let f = |name: &str, desc: &str, props: Value, req: &[&str]| {
        json!({"type": "function", "function": {"name": name, "description": desc,
            "parameters": {"type": "object", "properties": props, "required": req}}})
    };
    let mut t = vec![
        f(
            "list_files",
            "List files under a directory (default: the project root).",
            json!({"path": {"type": "string"}}),
            &[],
        ),
        f(
            "read_file",
            "Read a text file. Optional 1-based line range.",
            json!({"path": {"type": "string"}, "start_line": {"type": "integer"}, "end_line": {"type": "integer"}}),
            &["path"],
        ),
        f(
            "search",
            "Search file contents for a regular expression; returns file:line matches.",
            json!({"pattern": {"type": "string"}}),
            &["pattern"],
        ),
    ];
    if permission != "plan" {
        t.push(f(
            "write_file",
            "Create or overwrite a file with the given content.",
            json!({"path": {"type": "string"}, "content": {"type": "string"}}),
            &["path", "content"],
        ));
        t.push(f("edit_file", "Replace one exact occurrence of old_text with new_text in a file.", json!({"path": {"type": "string"}, "old_text": {"type": "string"}, "new_text": {"type": "string"}}), &["path", "old_text", "new_text"]));
        t.push(f(
            "run_command",
            "Run a shell command in the project root (120s limit) and return its output.",
            json!({"command": {"type": "string"}}),
            &["command"],
        ));
    }
    json!(t)
}

fn system_prompt(permission: &str) -> String {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let mode = if permission == "plan" {
        "You are in read-only planning mode: inspect the code and propose a plan; do not change files."
    } else {
        "Make the change directly with the tools, then verify it if there is a way to (tests, build, running it)."
    };
    format!(
        "You are a coding agent working in the project at {cwd}. Use the tools to look at files before \
         changing them. All paths are relative to the project root. {mode} When you are finished, reply \
         with a short summary of what you did and stop calling tools."
    )
}

/// Resolve a model-supplied path inside the project root; refuse anything that escapes it.
fn safe_path(p: &str) -> Result<PathBuf> {
    let rel = Path::new(p.trim_start_matches("./"));
    if rel.is_absolute() {
        let cwd = std::env::current_dir()?;
        return rel
            .strip_prefix(&cwd)
            .map(|r| r.to_path_buf())
            .map_err(|_| anyhow!("{p} is outside the project"));
    }
    if rel.components().any(|c| matches!(c, Component::ParentDir)) {
        bail!("{p} is outside the project");
    }
    Ok(if rel.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        rel.to_path_buf()
    })
}

fn clip(s: String) -> String {
    if s.len() <= MAX_TOOL_OUTPUT {
        return s;
    }
    let mut cut = MAX_TOOL_OUTPUT;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n… [truncated {} bytes]", &s[..cut], s.len() - cut)
}

fn walk(dir: &Path, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        if out.len() >= 400 {
            return;
        }
        let name = e.file_name().to_string_lossy().to_string();
        if matches!(
            name.as_str(),
            ".git" | "target" | "node_modules" | ".venv" | "dist" | "build"
        ) {
            continue;
        }
        let path = e.path();
        let shown = path
            .strip_prefix(".")
            .unwrap_or(&path)
            .display()
            .to_string();
        if path.is_dir() {
            out.push(format!("{shown}/"));
            walk(&path, out);
        } else {
            out.push(shown);
        }
    }
}

fn run_tool(name: &str, a: &Value, permission: &str) -> Result<String> {
    let s = |k: &str| {
        a[k].as_str()
            .map(str::to_string)
            .ok_or_else(|| anyhow!("missing argument {k}"))
    };
    let writes = matches!(name, "write_file" | "edit_file" | "run_command");
    if writes && permission == "plan" {
        bail!("{name} is not allowed in plan mode");
    }
    match name {
        "list_files" => {
            let p = safe_path(a["path"].as_str().unwrap_or("."))?;
            let mut v = vec![];
            walk(&p, &mut v);
            Ok(if v.is_empty() {
                "(empty)".into()
            } else {
                v.join("\n")
            })
        }
        "read_file" => {
            let text = std::fs::read_to_string(safe_path(&s("path")?)?)?;
            let start = a["start_line"].as_u64().unwrap_or(1).max(1) as usize;
            let end = a["end_line"]
                .as_u64()
                .map(|e| e as usize)
                .unwrap_or(usize::MAX);
            Ok(clip(
                text.lines()
                    .enumerate()
                    .skip(start - 1)
                    .take_while(|(i, _)| *i < end)
                    .map(|(i, l)| format!("{:>5}  {l}", i + 1))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ))
        }
        "search" => {
            let re = regex::Regex::new(&s("pattern")?)?;
            let mut files = vec![];
            walk(Path::new("."), &mut files);
            let mut hits = vec![];
            for f in files.iter().filter(|f| !f.ends_with('/')) {
                let Ok(text) = std::fs::read_to_string(f) else {
                    continue;
                };
                for (i, l) in text.lines().enumerate() {
                    if re.is_match(l) {
                        hits.push(format!("{f}:{}: {}", i + 1, l.trim()));
                    }
                }
            }
            Ok(if hits.is_empty() {
                "no matches".into()
            } else {
                clip(hits.join("\n"))
            })
        }
        "write_file" => {
            let p = safe_path(&s("path")?)?;
            if let Some(parent) = p.parent().filter(|d| !d.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent)?;
            }
            let content = s("content")?;
            std::fs::write(&p, &content)?;
            Ok(format!("wrote {} ({} bytes)", p.display(), content.len()))
        }
        "edit_file" => {
            let p = safe_path(&s("path")?)?;
            let text = std::fs::read_to_string(&p)?;
            let (old, new) = (s("old_text")?, s("new_text")?);
            match text.matches(&old).count() {
                0 => bail!("old_text not found in {}", p.display()),
                1 => {
                    std::fs::write(&p, text.replacen(&old, &new, 1))?;
                    Ok(format!("edited {}", p.display()))
                }
                n => bail!(
                    "old_text occurs {n} times in {}; include more context",
                    p.display()
                ),
            }
        }
        "run_command" => run_command(&s("command")?),
        other => bail!("unknown tool {other}"),
    }
}

fn run_command(cmd: &str) -> Result<String> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let t_out = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        s
    });
    let t_err = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    let started = Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break Some(st);
        }
        if started.elapsed() > Duration::from_secs(120) {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let (o, e) = (
        t_out.join().unwrap_or_default(),
        t_err.join().unwrap_or_default(),
    );
    let code = match status {
        Some(st) => format!(
            "exit code {}",
            st.code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".into())
        ),
        None => "timed out after 120s".into(),
    };
    Ok(clip(format!("{code}\n--- stdout\n{o}\n--- stderr\n{e}")))
}

/// Small local models often wrap reasoning in <think> tags; keep only the answer.
fn strip_think(s: &str) -> String {
    let mut out = s.to_string();
    while let (Some(a), Some(b)) = (out.find("<think>"), out.find("</think>")) {
        if b < a {
            break;
        }
        out.replace_range(a..b + "</think>".len(), "");
    }
    out.trim().to_string()
}

/// For models without native tool calling: describe the tools and ask for a JSON object.
fn text_tools_prompt(tools: &Value) -> String {
    let mut s = String::from(
        "\n\nTo use a tool, reply with ONLY one JSON object and nothing else, for example:\n\
         {\"tool\": \"read_file\", \"args\": {\"path\": \"README.md\"}}\n\
         You will get the result in the next message. Use one tool per reply. When you are done, \
         reply with a short plain-text summary and no JSON.\nTools:",
    );
    for t in tools.as_array().into_iter().flatten() {
        let f = &t["function"];
        let params: Vec<String> = f["parameters"]["properties"]
            .as_object()
            .map(|p| p.keys().cloned().collect())
            .unwrap_or_default();
        s.push_str(&format!(
            "\n- {}({}): {}",
            f["name"].as_str().unwrap_or(""),
            params.join(", "),
            f["description"].as_str().unwrap_or("")
        ));
    }
    s
}

/// Find the first JSON object with a "tool" field anywhere in a reply (fenced or not).
fn parse_text_call(content: &str) -> Option<(String, Value)> {
    for (i, _) in content.match_indices('{') {
        let mut it = serde_json::Deserializer::from_str(&content[i..]).into_iter::<Value>();
        if let Some(Ok(v)) = it.next() {
            if let Some(name) = v["tool"].as_str().or(v["name"].as_str()) {
                let args = v
                    .get("args")
                    .or(v.get("arguments"))
                    .cloned()
                    .unwrap_or(json!({}));
                return Some((name.to_string(), args));
            }
        }
    }
    None
}

fn chat(ep: &Endpoint, messages: &[Value], tools: Option<&Value>) -> Result<Value> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(900)))
        .build()
        .into();
    let mut req = agent
        .post(&ep.url)
        .header("Content-Type", "application/json");
    if let Some(k) = &ep.key {
        req = req.header("Authorization", &format!("Bearer {k}"));
    }
    let mut body = json!({"model": ep.model, "messages": messages, "stream": false});
    if let Some(t) = tools {
        body["tools"] = t.clone();
    }
    if ep.no_think {
        body["reasoning_effort"] = json!("none");
    }
    let mut resp = req
        .send_json(body)
        .with_context(|| format!("cannot reach the model server at {}", ep.url))?;
    let status = resp.status();
    let body: Value = resp.body_mut().read_json().unwrap_or(Value::Null);
    if !status.is_success() {
        let msg = body["error"]["message"]
            .as_str()
            .or(body["error"].as_str())
            .unwrap_or("request failed");
        bail!("{} {}: {msg}", status.as_u16(), ep.model);
    }
    Ok(body)
}

fn enter_text_mode(messages: &mut [Value], tools: &Value) {
    if let Some(sys) = messages.first_mut() {
        let text = sys["content"].as_str().unwrap_or_default().to_string();
        if !text.contains("To use a tool, reply with ONLY") {
            sys["content"] = json!(text + &text_tools_prompt(tools));
        }
    }
}

fn state_file(o: &Opts, id: &str) -> Option<PathBuf> {
    o.state_dir
        .as_ref()
        .map(|d| d.join(format!("agent-{id}.json")))
}

pub fn run(o: Opts) -> Result<()> {
    let session = o
        .resume
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let mut messages: Vec<Value> = state_file(&o, &session)
        .and_then(|f| std::fs::read_to_string(f).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| {
            vec![json!({"role": "system", "content": system_prompt(&o.permission)})]
        });
    let tools = tools(&o.permission);
    let mut text_mode = o.tool_mode == "text";
    if text_mode {
        enter_text_mode(&mut messages, &tools);
    }
    let mut ep: Option<Endpoint> = None;
    let mut started = false;

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
            _ => continue,
        };
        if o.replay {
            out(
                json!({"type": "user", "message": msg["message"], "session_id": session, "isReplay": true}),
            );
        }
        if ep.is_none() {
            match endpoint(&o) {
                Ok(e) => ep = Some(e),
                Err(e) => {
                    out(
                        json!({"type": "result", "subtype": "error_during_execution", "is_error": true, "session_id": session, "result": e.to_string()}),
                    );
                    continue;
                }
            }
        }
        let ep = ep.as_ref().unwrap();
        if !started {
            started = true;
            out(
                json!({"type": "system", "subtype": "init", "session_id": session, "model": ep.model, "cwd": std::env::current_dir()?}),
            );
        }
        messages.push(json!({"role": "user", "content": text}));
        let (mut tin, mut tout) = (0u64, 0u64);
        let mut last = String::new();
        let mut error: Option<String> = None;
        for step in 0..o.max_steps {
            let mut reply = chat(ep, &messages, (!text_mode).then_some(&tools));
            if let Err(e) = &reply {
                if !text_mode
                    && o.tool_mode == "auto"
                    && e.to_string().contains("does not support tools")
                {
                    eprintln!(
                        "{} has no native tool calling; switching to JSON tool calls in text",
                        ep.model
                    );
                    text_mode = true;
                    enter_text_mode(&mut messages, &tools);
                    reply = chat(ep, &messages, None);
                }
            }
            let body = match reply {
                Ok(b) => b,
                Err(e) => {
                    error = Some(e.to_string());
                    break;
                }
            };
            tin += body["usage"]["prompt_tokens"].as_u64().unwrap_or(0);
            tout += body["usage"]["completion_tokens"].as_u64().unwrap_or(0);
            let m = &body["choices"][0]["message"];
            let content = strip_think(m["content"].as_str().unwrap_or_default());
            let mut calls = m["tool_calls"].as_array().cloned().unwrap_or_default();
            let text_call = if text_mode {
                parse_text_call(&content)
            } else {
                None
            };
            if let Some((name, args)) = &text_call {
                calls = vec![
                    json!({"id": format!("call_{step}"), "function": {"name": name, "arguments": args}}),
                ];
            }
            // A JSON tool call is not prose; do not show it as the agent's message.
            let content = if text_call.is_some() {
                String::new()
            } else {
                content
            };
            if !content.is_empty() {
                last = content.clone();
                out(
                    json!({"type": "assistant", "session_id": session, "message": {"role": "assistant", "content": [{"type": "text", "text": content}]}}),
                );
            }
            if text_mode {
                let raw = m["content"].as_str().unwrap_or_default();
                messages.push(json!({"role": "assistant", "content": raw}));
            } else {
                let mut assistant = json!({"role": "assistant", "content": content});
                if !calls.is_empty() {
                    assistant["tool_calls"] = json!(calls);
                }
                messages.push(assistant);
            }
            if calls.is_empty() {
                break;
            }
            for (i, c) in calls.iter().enumerate() {
                let id = c["id"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("call_{step}_{i}"));
                let name = c["function"]["name"].as_str().unwrap_or("unknown");
                let args = match &c["function"]["arguments"] {
                    Value::String(s) => serde_json::from_str(s).unwrap_or(json!({})),
                    v => v.clone(),
                };
                out(
                    json!({"type": "assistant", "session_id": session, "message": {"role": "assistant", "content": [{"type": "tool_use", "id": id, "name": name, "input": args}]}}),
                );
                let result =
                    run_tool(name, &args, &o.permission).unwrap_or_else(|e| format!("error: {e}"));
                out(
                    json!({"type": "user", "session_id": session, "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": id, "content": clip(result.clone())}]}}),
                );
                if text_mode {
                    messages.push(
                        json!({"role": "user", "content": format!("Result of {name}:\n{result}")}),
                    );
                } else {
                    messages.push(
                        json!({"role": "tool", "tool_call_id": id, "name": name, "content": result}),
                    );
                }
            }
            if step + 1 == o.max_steps {
                last = format!("stopped after {} steps", o.max_steps);
            }
        }
        if let Some(f) = state_file(&o, &session) {
            let _ = std::fs::write(f, serde_json::to_string(&messages).unwrap_or_default());
        }
        match error {
            Some(e) => out(
                json!({"type": "result", "subtype": "error_during_execution", "is_error": true, "session_id": session, "result": e,
                                  "usage": {"input_tokens": tin, "output_tokens": tout}, "total_cost_usd": 0.0}),
            ),
            None => out(
                json!({"type": "result", "subtype": "success", "is_error": false, "session_id": session, "result": last,
                               "usage": {"input_tokens": tin, "output_tokens": tout}, "total_cost_usd": 0.0}),
            ),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_stay_inside_the_project() {
        assert!(safe_path("../etc/passwd").is_err());
        assert!(safe_path("/etc/passwd").is_err());
        assert_eq!(
            safe_path("./src/main.rs").unwrap(),
            PathBuf::from("src/main.rs")
        );
        assert_eq!(safe_path("").unwrap(), PathBuf::from("."));
    }

    #[test]
    fn text_tool_calls_are_found_in_prose_and_fences() {
        let (n, a) = parse_text_call(
            "Sure.\n```json\n{\"tool\": \"read_file\", \"args\": {\"path\": \"a.py\"}}\n```",
        )
        .unwrap();
        assert_eq!(n, "read_file");
        assert_eq!(a["path"], "a.py");
        assert!(parse_text_call("All done, the test passes.").is_none());
    }

    #[test]
    fn think_blocks_are_removed() {
        assert_eq!(strip_think("<think>hmm</think>\nDone."), "Done.");
    }
}
