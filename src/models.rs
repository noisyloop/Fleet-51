//! Local model management through Ollama's own API: list what is installed (with whether each
//! model can call tools), download with live progress pushed to every client, and delete.

use std::io::{BufRead, BufReader};

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};

use crate::agent::ollama_root;

/// Small models that are a sensible start on a laptop. `tools` = native tool calling;
/// the rest work through the agent's text-mode fallback, less reliably.
pub const SUGGESTED: &[(&str, &str, bool, &str)] = &[
    (
        "qwen3:0.6b",
        "0.5 GB",
        true,
        "Smallest model with tool calling; fast, often wrong",
    ),
    ("qwen3:1.7b", "1.4 GB", true, "Good first choice on a CPU"),
    (
        "qwen3:4b",
        "2.5 GB",
        true,
        "Noticeably better, still laptop-sized",
    ),
    ("qwen2.5-coder:1.5b", "1.0 GB", true, "Small coding model"),
    (
        "qwen2.5-coder:7b",
        "4.7 GB",
        true,
        "Strong coding model if you have the memory",
    ),
    ("llama3.2:3b", "2.0 GB", true, "General model from Meta"),
    (
        "tinydolphin",
        "0.6 GB",
        false,
        "Tiny Dolphin (TinyLlama 1.1B); no native tool calling",
    ),
    (
        "dolphin-phi",
        "1.6 GB",
        false,
        "Dolphin on Phi-2 2.7B; no native tool calling",
    ),
    (
        "dolphin3",
        "4.9 GB",
        false,
        "Dolphin 3 on Llama 3.1 8B; no native tool calling",
    ),
];

#[derive(Clone, Debug, Serialize)]
pub struct Pull {
    pub name: String,
    pub status: String,
    pub completed: u64,
    pub total: u64,
    pub done: bool,
    pub error: Option<String>,
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._:/-".contains(c))
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into()
}

fn error_of(v: &Value, fallback: &str) -> String {
    v["error"].as_str().unwrap_or(fallback).to_string()
}

pub fn installed() -> Result<Vec<Value>> {
    let mut resp = agent()
        .get(&format!("{}/api/tags", ollama_root()))
        .call()
        .context("cannot reach Ollama (is `ollama serve` running?)")?;
    let v: Value = resp.body_mut().read_json()?;
    let mut out = vec![];
    for m in v["models"].as_array().into_iter().flatten() {
        let name = m["name"].as_str().unwrap_or_default().to_string();
        // /api/show reports capabilities such as "tools"; older Ollama versions omit them.
        let caps = agent()
            .post(&format!("{}/api/show", ollama_root()))
            .send_json(json!({"model": name}))
            .ok()
            .and_then(|mut r| r.body_mut().read_json::<Value>().ok())
            .and_then(|s| s["capabilities"].as_array().cloned());
        out.push(json!({
            "name": name,
            "size": m["size"],
            "parameter_size": m["details"]["parameter_size"],
            "quantization": m["details"]["quantization_level"],
            "tools": caps.map(|c| c.iter().any(|x| x == "tools")),
            // Ollama "cloud" models run on Ollama's servers: your messages leave this computer.
            "cloud": m["remote_host"].is_string() || m["remote_model"].is_string()
                || name.ends_with("-cloud") || name.ends_with(":cloud"),
        }));
    }
    Ok(out)
}

/// Download a model, calling `progress` as Ollama reports it. Blocking.
pub fn pull(name: &str, mut progress: impl FnMut(&Pull)) -> Result<()> {
    let resp = agent()
        .post(&format!("{}/api/pull", ollama_root()))
        .send_json(json!({"model": name, "stream": true}))
        .context("cannot reach Ollama (is `ollama serve` running?)")?;
    if !resp.status().is_success() {
        let mut resp = resp;
        let v: Value = resp.body_mut().read_json().unwrap_or(Value::Null);
        bail!("{}", error_of(&v, "download failed"));
    }
    let reader = BufReader::new(resp.into_body().into_with_config().limit(u64::MAX).reader());
    let mut p = Pull {
        name: name.to_string(),
        status: "starting".into(),
        completed: 0,
        total: 0,
        done: false,
        error: None,
    };
    for line in reader.lines() {
        let Ok(v) = serde_json::from_str::<Value>(&line?) else {
            continue;
        };
        if let Some(e) = v["error"].as_str() {
            bail!("{e}");
        }
        p.status = v["status"].as_str().unwrap_or("").to_string();
        if let (Some(c), Some(t)) = (v["completed"].as_u64(), v["total"].as_u64()) {
            p.completed = c;
            p.total = t;
        }
        progress(&p);
        if p.status == "success" {
            return Ok(());
        }
    }
    bail!("download ended before Ollama reported success")
}

pub fn delete(name: &str) -> Result<()> {
    let mut resp = agent()
        .delete(&format!("{}/api/delete", ollama_root()))
        .force_send_body()
        .send_json(json!({"model": name}))
        .context("cannot reach Ollama")?;
    if !resp.status().is_success() {
        let v: Value = resp.body_mut().read_json().unwrap_or(Value::Null);
        bail!("{}", error_of(&v, "delete failed"));
    }
    Ok(())
}
