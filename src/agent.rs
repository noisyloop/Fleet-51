//! `fleet51 agent`: the bot behind each chat. It talks to a local Ollama model and speaks a small
//! JSON-lines protocol on stdio (Claude Code's stream-json shape), so the daemon gets explicit
//! prompt acknowledgements, resumable sessions, reasoning, tool events and token usage.
//!
//! A bot can only touch files inside its own folder: every path is resolved (following symlinks)
//! and checked against the folder before use, and code runs only inside the sandbox (see
//! sandbox.rs). Before a bot's first change in a reply its folder is copied aside for Undo.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

const MAX_TOOL_OUTPUT: usize = 8000;

pub struct Opts {
    pub model: Option<String>,
    pub resume: Option<String>,
    pub state_dir: Option<PathBuf>,
    pub max_steps: usize,
    /// Let reasoning models think before answering; the reasoning is shown in the chat.
    pub think: bool,
    /// Offer run_command (only if a sandbox works on this computer).
    pub run_code: bool,
    /// native | text | auto (native, falling back to text for models without tool calling)
    pub tool_mode: String,
}

fn out(v: Value) {
    let mut o = std::io::stdout().lock();
    let _ = writeln!(o, "{v}");
    let _ = o.flush();
}

/// The Ollama the daemon settled on, else Ollama's own OLLAMA_HOST convention.
pub fn ollama_root() -> String {
    if let Some(root) = crate::runtime::root() {
        return root;
    }
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
        .context("the model engine is not running yet")?;
    let v: Value = resp.body_mut().read_json()?;
    Ok(v["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["name"].as_str().map(str::to_string))
        .collect())
}

fn tools(can_run: bool) -> Value {
    let f = |name: &str, desc: &str, props: Value, req: &[&str]| {
        json!({"type": "function", "function": {"name": name, "description": desc,
            "parameters": {"type": "object", "properties": props, "required": req}}})
    };
    let mut t = json!([
        f(
            "list_files",
            "List files in your folder (or a subfolder).",
            json!({"path": {"type": "string"}}),
            &[]
        ),
        f(
            "read_file",
            "Read a text file from your folder.",
            json!({"path": {"type": "string"}}),
            &["path"]
        ),
        f(
            "search",
            "Search the files in your folder for a regular expression.",
            json!({"pattern": {"type": "string"}}),
            &["pattern"]
        ),
        f(
            "write_file",
            "Create or overwrite a file in your folder.",
            json!({"path": {"type": "string"}, "content": {"type": "string"}}),
            &["path", "content"]
        ),
        f(
            "edit_file",
            "Replace one exact occurrence of old_text with new_text in a file.",
            json!({"path": {"type": "string"}, "old_text": {"type": "string"}, "new_text": {"type": "string"}}),
            &["path", "old_text", "new_text"]
        ),
    ]);
    if can_run {
        t.as_array_mut().expect("array").push(f(
            "run_command",
            "Run a shell command in your folder, for example to test code you wrote (python3 app.py). \
             It has no internet access, can only change files in your folder and stops after 60 seconds.",
            json!({"command": {"type": "string"}}),
            &["command"],
        ));
    }
    t
}

const SYSTEM_PROMPT: &str =
    "You are a helpful assistant running privately on the user's own computer. \
You have a folder of your own and tools to list, read, search, create and edit files in it; you \
cannot touch anything outside it. Use the tools only when a request needs them; \
for ordinary questions and conversation, just answer. Be clear and concise.";

const RUN_PROMPT: &str =
    " You can also run commands in your folder with run_command (no internet, \
60-second limit). When you write code, run it to check it works before saying it is done.";

/// Tools that change the folder; the first one in a reply takes an undo checkpoint.
const CHANGES_FILES: &[&str] = &["write_file", "edit_file", "run_command"];

/// The bot's folder as a confined handle: every file operation through it is resolved by the
/// operating system inside the folder, so a link swapped in at the last moment (for example by
/// code the bot left running) can't redirect a read or write outside it.
fn folder(root: &Path) -> Result<cap_std::fs::Dir> {
    Ok(cap_std::fs::Dir::open_ambient_dir(
        root,
        cap_std::ambient_authority(),
    )?)
}

/// `p` (already checked by safe_path) relative to the folder.
fn inside<'a>(root: &Path, p: &'a Path) -> Result<&'a Path> {
    p.strip_prefix(root)
        .map_err(|_| anyhow!("{} is outside your folder", p.display()))
}

/// Resolve a model-supplied path inside `root`, following symlinks, and refuse anything outside.
fn safe_path(root: &Path, p: &str) -> Result<PathBuf> {
    let p = p.trim();
    let rel = Path::new(p).strip_prefix(root).unwrap_or(Path::new(p));
    if rel.is_absolute() {
        bail!("{p} is outside your folder");
    }
    let joined = root.join(rel);
    // The deepest part of the path that exists decides where it really points.
    let mut existing = joined.as_path();
    let mut rest = Vec::new();
    while !existing.exists() {
        rest.push(
            existing
                .file_name()
                .ok_or_else(|| anyhow!("{p} is outside your folder"))?
                .to_owned(),
        );
        existing = existing.parent().ok_or_else(|| anyhow!("bad path {p}"))?;
    }
    let mut real = existing.canonicalize()?;
    for part in rest.iter().rev() {
        real.push(part);
    }
    if !real.starts_with(root) {
        bail!("{p} is outside your folder");
    }
    Ok(real)
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

fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        if out.len() >= 400 {
            return;
        }
        let path = e.path();
        // Never follow links out of the folder while listing; skip hidden files.
        if path.is_symlink() || e.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let shown = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .display()
            .to_string();
        if path.is_dir() {
            out.push(format!("{shown}/"));
            walk(root, &path, out);
        } else {
            out.push(shown);
        }
    }
}

fn run_tool(
    root: &Path,
    sandbox: Option<crate::sandbox::Kind>,
    name: &str,
    a: &Value,
) -> Result<String> {
    let s = |k: &str| {
        a[k].as_str()
            .map(str::to_string)
            .ok_or_else(|| anyhow!("missing argument {k}"))
    };
    match name {
        "list_files" => {
            let p = safe_path(root, a["path"].as_str().unwrap_or(""))?;
            let mut v = vec![];
            walk(root, &p, &mut v);
            Ok(if v.is_empty() {
                "(empty)".into()
            } else {
                v.join("\n")
            })
        }
        "read_file" => {
            let p = safe_path(root, &s("path")?)?;
            Ok(clip(folder(root)?.read_to_string(inside(root, &p)?)?))
        }
        "search" => {
            let re = regex::Regex::new(&s("pattern")?)?;
            let mut files = vec![];
            walk(root, root, &mut files);
            let dir = folder(root)?;
            let mut hits = vec![];
            for f in files.iter().filter(|f| !f.ends_with('/')) {
                let Ok(text) = dir.read_to_string(f) else {
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
            let p = safe_path(root, &s("path")?)?;
            let rel = inside(root, &p)?;
            let dir = folder(root)?;
            if let Some(parent) = rel.parent().filter(|p| !p.as_os_str().is_empty()) {
                dir.create_dir_all(parent)?;
            }
            let content = s("content")?;
            dir.write(rel, &content)?;
            mark_made_by_bot(&p);
            Ok(format!(
                "wrote {} ({} bytes)",
                p.strip_prefix(root).unwrap_or(&p).display(),
                content.len()
            ))
        }
        "edit_file" => {
            let p = safe_path(root, &s("path")?)?;
            let rel = inside(root, &p)?;
            let dir = folder(root)?;
            let text = dir.read_to_string(rel)?;
            let (old, new) = (s("old_text")?, s("new_text")?);
            match text.matches(&old).count() {
                0 => bail!("old_text not found"),
                1 => {
                    dir.write(rel, text.replacen(&old, &new, 1))?;
                    mark_made_by_bot(&p);
                    Ok("edited".into())
                }
                n => bail!("old_text occurs {n} times; include more context"),
            }
        }
        "run_command" => {
            let kind = sandbox.ok_or_else(|| {
                anyhow!("running programs is switched off for you; the user can turn it on")
            })?;
            let (code, output) = crate::sandbox::run(kind, root, &s("command")?)?;
            let output = if output.trim().is_empty() {
                "(no output)".to_string()
            } else {
                output
            };
            Ok(clip(match code {
                Some(0) => output,
                Some(c) => format!("exit code {c}\n{output}"),
                None => format!("stopped by a signal\n{output}"),
            }))
        }
        other => bail!("unknown tool {other}"),
    }
}

/// Split a reply into (reasoning, answer). Some models put reasoning in <think> tags.
fn split_think(s: &str) -> (String, String) {
    let mut answer = s.to_string();
    let mut reasoning = Vec::new();
    while let (Some(a), Some(b)) = (answer.find("<think>"), answer.find("</think>")) {
        if b < a {
            break;
        }
        reasoning.push(answer[a + "<think>".len()..b].trim().to_string());
        answer.replace_range(a..b + "</think>".len(), "");
    }
    (reasoning.join("\n\n"), answer.trim().to_string())
}

/// For models without native tool calling: describe the tools and ask for a JSON object.
fn text_tools_prompt(tools: &Value) -> String {
    let mut s = String::from(
        "\n\nTo use a tool, reply with ONLY one JSON object and nothing else, for example:\n\
         {\"tool\": \"read_file\", \"args\": {\"path\": \"notes.md\"}}\n\
         You will get the result in the next message. Use one tool per reply. When you are done, \
         reply in plain text with no JSON.\nTools:",
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

fn chat(model: &str, think: bool, messages: &[Value], tools: Option<&Value>) -> Result<Value> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(900)))
        .build()
        .into();
    let mut body = json!({"model": model, "messages": messages, "stream": false});
    if let Some(t) = tools {
        body["tools"] = t.clone();
    }
    if !think {
        // Ollama maps this to think=false: replies arrive far sooner on small hardware.
        body["reasoning_effort"] = json!("none");
    }
    let mut resp = agent
        .post(&format!("{}/v1/chat/completions", ollama_root()))
        .send_json(body)
        .context("the model engine is not running yet")?;
    let status = resp.status();
    let body: Value = resp.body_mut().read_json().unwrap_or(Value::Null);
    if !status.is_success() {
        let msg = body["error"]["message"]
            .as_str()
            .or(body["error"].as_str())
            .unwrap_or("request failed");
        bail!("{msg}");
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

fn say(session: &str, block: Value) {
    out(
        json!({"type": "assistant", "session_id": session, "message": {"role": "assistant", "content": [block]}}),
    );
}

/// On macOS, tag a file a bot wrote the way downloads are tagged, so macOS asks before opening
/// it if it turns out to be a script or app.
fn mark_made_by_bot(path: &Path) {
    if cfg!(target_os = "macos") {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = std::process::Command::new("/usr/bin/xattr")
            .args([
                "-s",
                "-w",
                "com.apple.quarantine",
                &format!("0082;{secs:x};Fleet-51;"),
            ])
            .arg(path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// Whether a model accepts images (Ollama reports a "vision" capability).
fn can_see(model: &str) -> bool {
    ureq::post(&format!("{}/api/show", ollama_root()))
        .send_json(json!({"model": model}))
        .ok()
        .and_then(|mut r| r.body_mut().read_json::<Value>().ok())
        .and_then(|s| s["capabilities"].as_array().cloned())
        .is_some_and(|c| c.iter().any(|x| x == "vision"))
}

/// The line added to a message that comes with files.
fn attachment_note(root: &Path, names: &[String], has_images: bool, sees: bool) -> String {
    let described: Vec<String> = names
        .iter()
        .map(|n| {
            let text = format!("{n}.txt");
            if root.join(&text).is_file() {
                format!("{n} (its text is in {text})")
            } else {
                n.clone()
            }
        })
        .collect();
    let mut note = format!(
        "[The user added these files to your folder: {}. Read them with your tools as needed.]",
        described.join(", ")
    );
    if has_images && !sees {
        note.push_str(
            " [This model can't see images. If the request needs the picture, say so and suggest \
             switching this bot to a model that can see, such as gemma4 or qwen2.5vl.]",
        );
    }
    note
}

/// How an automation's instructions reach the bot: framed as a task the user set up, to carry out
/// now without asking questions.
fn automation_prompt(name: &str, instructions: &str) -> String {
    format!(
        "[Automation \"{name}\": a task the user scheduled earlier, running now on its own. The user \
         is not here to answer questions, so do exactly what the instructions say and reply with \
         the result only.]\n\n{instructions}"
    )
}

/// Copy the folder aside before the bot changes it, and tell the daemon so the chat offers Undo.
fn checkpoint(root: &Path, state_dir: Option<&Path>, session: &str) {
    let Some(dir) = state_dir.map(|d| d.join("checkpoints")) else {
        return;
    };
    let id = format!("{}", crate::home::now_ms());
    match crate::checkpoints::snapshot(root, &dir.join(&id)) {
        Ok(()) => {
            crate::checkpoints::prune(&dir);
            out(
                json!({"type": "system", "subtype": "checkpoint", "session_id": session, "id": id}),
            );
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(dir.join(&id));
            out(
                json!({"type": "system", "subtype": "checkpoint_failed", "session_id": session, "message": format!("Undo is not available for this reply: {e}")}),
            );
        }
    }
}

pub fn run(o: Opts) -> Result<()> {
    let root = std::env::current_dir()?.canonicalize()?;
    let session = o
        .resume
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let state_file = o
        .state_dir
        .as_ref()
        .map(|d| d.join(format!("agent-{session}.json")));
    let mut messages: Vec<Value> = state_file
        .as_ref()
        .and_then(|f| std::fs::read_to_string(f).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| vec![json!({"role": "system", "content": SYSTEM_PROMPT})]);
    let sandbox = if o.run_code {
        crate::sandbox::available()
    } else {
        None
    };
    // Running code can be switched on and off mid-conversation; keep the instructions in step.
    if let Some(sys) = messages.first_mut() {
        let text = sys["content"]
            .as_str()
            .unwrap_or_default()
            .replace(RUN_PROMPT, "");
        sys["content"] = json!(if sandbox.is_some() {
            text + RUN_PROMPT
        } else {
            text
        });
    }
    let tools = tools(sandbox.is_some());
    let mut text_mode = o.tool_mode == "text";
    if text_mode {
        enter_text_mode(&mut messages, &tools);
    }
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
        out(
            json!({"type": "user", "message": msg["message"], "session_id": session, "isReplay": true}),
        );
        // Resolve the model per message: the engine may have come up after this process started.
        let model = match o.model.clone().filter(|m| !m.is_empty()) {
            Some(m) => Ok(m),
            None => ollama_models().and_then(|m| {
                m.into_iter()
                    .next()
                    .ok_or_else(|| anyhow!("no model is downloaded yet"))
            }),
        };
        let model = match model {
            Ok(m) => m,
            Err(e) => {
                out(
                    json!({"type": "result", "subtype": "error_during_execution", "is_error": true, "session_id": session, "result": e.to_string()}),
                );
                continue;
            }
        };
        if !started {
            started = true;
            out(
                json!({"type": "system", "subtype": "init", "session_id": session, "model": model}),
            );
        }
        // An automation runs as a standalone task: just the instructions, not the whole chat, so
        // small models do what it says instead of carrying on the conversation.
        let automation = msg["automation"].as_str().map(str::to_string);
        let chat_history = automation.as_ref().map(|_| {
            let system = messages
                .first()
                .cloned()
                .unwrap_or(json!({"role": "system", "content": SYSTEM_PROMPT}));
            std::mem::replace(&mut messages, vec![system])
        });
        let text = match &automation {
            Some(name) => automation_prompt(name, &text),
            None => text,
        };
        // Files the user added with this message: say where they are, and show images to
        // models that can see.
        let attached: Vec<String> = msg["attachments"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(crate::files::clean_name))
            .filter(|n| root.join(n).is_file())
            .collect();
        let images = crate::files::image_urls(&root, &attached);
        let sees = !images.is_empty() && can_see(&model);
        let text = if attached.is_empty() {
            text
        } else {
            format!(
                "{text}\n\n{}",
                attachment_note(&root, &attached, !images.is_empty(), sees)
            )
        };
        if sees {
            let mut parts = vec![json!({"type": "text", "text": text})];
            parts.extend(
                images
                    .iter()
                    .map(|u| json!({"type": "image_url", "image_url": {"url": u}})),
            );
            messages.push(json!({"role": "user", "content": parts}));
        } else {
            messages.push(json!({"role": "user", "content": text}));
        }
        let user_at = messages.len() - 1;
        let (mut tin, mut tout) = (0u64, 0u64);
        let mut last = String::new();
        let mut error: Option<String> = None;
        let mut checkpointed = false;
        for step in 0..o.max_steps {
            let mut reply = chat(&model, o.think, &messages, (!text_mode).then_some(&tools));
            if let Err(e) = &reply {
                if !text_mode
                    && o.tool_mode == "auto"
                    && e.to_string().contains("does not support tools")
                {
                    text_mode = true;
                    enter_text_mode(&mut messages, &tools);
                    reply = chat(&model, o.think, &messages, None);
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
            let raw = m["content"].as_str().unwrap_or_default();
            let (inline_reasoning, content) = split_think(raw);
            let reasoning = m["reasoning"]
                .as_str()
                .or(m["reasoning_content"].as_str())
                .unwrap_or(&inline_reasoning)
                .trim()
                .to_string();
            if !reasoning.is_empty() {
                say(&session, json!({"type": "thinking", "thinking": reasoning}));
            }
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
            // A JSON tool call is not prose; do not show it as the bot's message.
            let content = if text_call.is_some() {
                String::new()
            } else {
                content
            };
            if !content.is_empty() {
                last = content.clone();
                say(&session, json!({"type": "text", "text": content}));
            }
            if text_mode {
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
                say(
                    &session,
                    json!({"type": "tool_use", "id": id, "name": name, "input": args}),
                );
                if CHANGES_FILES.contains(&name) && !checkpointed {
                    checkpointed = true;
                    checkpoint(&root, o.state_dir.as_deref(), &session);
                }
                let result =
                    run_tool(&root, sandbox, name, &args).unwrap_or_else(|e| format!("error: {e}"));
                if text_mode {
                    messages.push(
                        json!({"role": "user", "content": format!("Result of {name}:\n{result}")}),
                    );
                } else {
                    messages.push(json!({"role": "tool", "tool_call_id": id, "name": name, "content": result}));
                }
            }
            if step + 1 == o.max_steps {
                last = format!("stopped after {} steps", o.max_steps);
            }
        }
        // Images are sent once; the saved conversation keeps just the text.
        if sees {
            if let Some(m) = messages.get_mut(user_at) {
                m["content"] = json!(text);
            }
        }
        // Keep the automation's instructions and final answer in the chat's memory, so you can
        // ask the bot about it afterwards.
        if let Some(mut history) = chat_history {
            history.push(json!({"role": "user", "content": text}));
            if !last.is_empty() {
                history.push(json!({"role": "assistant", "content": last}));
            }
            messages = history;
        }
        if let Some(f) = &state_file {
            let _ = std::fs::write(f, serde_json::to_string(&messages).unwrap_or_default());
        }
        let usage = json!({"input_tokens": tin, "output_tokens": tout});
        match error {
            Some(e) => out(
                json!({"type": "result", "subtype": "error_during_execution", "is_error": true, "session_id": session, "result": e, "usage": usage}),
            ),
            None => out(
                json!({"type": "result", "subtype": "success", "is_error": false, "session_id": session, "result": last, "usage": usage}),
            ),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_stay_inside_the_folder_even_through_symlinks() {
        let base = std::env::temp_dir().join(format!("fleet51-safe-{}", std::process::id()));
        let root = base.join("bot");
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        assert!(safe_path(&root, "../etc/passwd").is_err());
        assert!(safe_path(&root, "/etc/passwd").is_err());
        assert!(safe_path(&root, "notes/new.md").unwrap().starts_with(&root));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc", root.join("escape")).unwrap();
            assert!(safe_path(&root, "escape/passwd").is_err());
            assert!(safe_path(&root, "escape/new-file").is_err());
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn text_tool_calls_are_found_in_prose_and_fences() {
        let (n, a) = parse_text_call(
            "Sure.\n```json\n{\"tool\": \"read_file\", \"args\": {\"path\": \"a.md\"}}\n```",
        )
        .unwrap();
        assert_eq!(n, "read_file");
        assert_eq!(a["path"], "a.md");
        assert!(parse_text_call("All done.").is_none());
    }

    #[test]
    fn inline_reasoning_is_split_from_the_answer() {
        assert_eq!(
            split_think("<think>17*3 is 51</think>\n51."),
            ("17*3 is 51".to_string(), "51.".to_string())
        );
    }
}
