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
        "Recommended: much better at files and code, still laptop-sized",
    ),
    ("qwen2.5-coder:1.5b", "1.0 GB", true, "Small coding model"),
    (
        "gemma4",
        "about 10 GB",
        true,
        "Sees images (and reads documents); needs 16 GB of memory",
    ),
    (
        "qwen2.5vl:3b",
        "about 3 GB",
        false,
        "Sees images; smaller and faster",
    ),
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

/// Space left free on the disk after a download, so a model never fills it.
pub const KEEP_FREE: u64 = 2_000_000_000;

/// "1.4 GB" or "about 10 GB" (as in SUGGESTED) in bytes.
pub fn size_bytes(size: &str) -> Option<u64> {
    let n: f64 = size.split_whitespace().find_map(|w| w.parse().ok())?;
    Some((n * 1e9) as u64)
}

/// Where Ollama keeps models: OLLAMA_MODELS, else ~/.ollama/models (or the nearest folder of it
/// that exists, which is on the same disk).
fn models_dir() -> std::path::PathBuf {
    let dir = std::env::var_os("OLLAMA_MODELS")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| crate::home::user_home().join(".ollama").join("models"));
    dir.ancestors()
        .find(|d| d.is_dir())
        .map(|d| d.to_path_buf())
        .unwrap_or(dir)
}

/// Free space on the disk that holds Ollama's models, in bytes.
pub fn free_bytes() -> Option<u64> {
    let dir = models_dir();
    let text = if cfg!(windows) {
        let drive: String = dir.to_string_lossy().chars().take(2).collect();
        if !drive.ends_with(':') {
            return None;
        }
        let mut c = std::process::Command::new("powershell.exe");
        c.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!("([System.IO.DriveInfo]::new('{drive}\\')).AvailableFreeSpace"),
        ]);
        crate::home::no_window(&mut c);
        String::from_utf8_lossy(&c.output().ok()?.stdout).to_string()
    } else {
        let out = std::process::Command::new("df")
            .arg("-Pk")
            .arg(&dir)
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        let kb: u64 = text
            .lines()
            .nth(1)?
            .split_whitespace()
            .nth(3)?
            .parse()
            .ok()?;
        (kb * 1024).to_string()
    };
    text.trim().parse().ok()
}

/// A model's download size from Ollama's registry, before downloading anything. `None` when it
/// can't be known (offline, another registry, a cloud model).
pub fn download_size(name: &str) -> Option<u64> {
    let url = manifest_url(name)?;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(10)))
        .build()
        .into();
    let v: Value = agent
        .get(&url)
        .header(
            "Accept",
            "application/vnd.docker.distribution.manifest.v2+json",
        )
        .call()
        .ok()?
        .body_mut()
        .read_json()
        .ok()?;
    let layers = v["layers"].as_array()?;
    Some(
        layers
            .iter()
            .filter_map(|l| l["size"].as_u64())
            .sum::<u64>()
            + v["config"]["size"].as_u64().unwrap_or(0),
    )
}

/// Where Ollama's registry keeps a model's manifest (only for models from ollama.com).
fn manifest_url(name: &str) -> Option<String> {
    let (path, tag) = name.split_once(':').unwrap_or((name, "latest"));
    if tag.contains("cloud") || name.ends_with("-cloud") {
        return None;
    }
    let path = match path.split('/').collect::<Vec<_>>()[..] {
        [model] => format!("library/{model}"),
        [first, _] if !first.contains('.') => path.to_string(),
        _ => return None,
    };
    Some(format!(
        "https://registry.ollama.ai/v2/{path}/manifests/{tag}"
    ))
}

fn gb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1e9)
}

/// Refuse a download of `needed` more bytes that would leave less than KEEP_FREE on the disk.
pub fn check_space(name: &str, needed: u64) -> Result<()> {
    match free_bytes() {
        Some(free) => fits(name, needed, free),
        None => Ok(()),
    }
}

fn fits(name: &str, needed: u64, free: u64) -> Result<()> {
    if needed.saturating_add(KEEP_FREE) > free {
        bail!(
            "not enough disk space: {name} needs {} and {} is free (Fleet-51 keeps at least {} free). Choose a smaller model or free up space.",
            gb(needed),
            gb(free),
            gb(KEEP_FREE)
        );
    }
    Ok(())
}

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
            "tools": caps.as_ref().map(|c| c.iter().any(|x| x == "tools")),
            "vision": caps.as_ref().is_some_and(|c| c.iter().any(|x| x == "vision")),
            // Ollama "cloud" models run on Ollama's servers: your messages leave this computer.
            "cloud": m["remote_host"].is_string() || m["remote_model"].is_string()
                || name.ends_with("-cloud") || name.ends_with(":cloud"),
        }));
    }
    Ok(out)
}

/// Download a model, calling `progress` as Ollama reports it; `progress` returns an error to
/// stop the download. Blocking.
pub fn pull(name: &str, mut progress: impl FnMut(&Pull) -> Result<()>) -> Result<()> {
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
        progress(&p)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_and_disk_space_are_checked_before_downloading() {
        assert_eq!(size_bytes("1.4 GB"), Some(1_400_000_000));
        assert_eq!(size_bytes("about 10 GB"), Some(10_000_000_000));
        assert!(SUGGESTED.iter().all(|s| size_bytes(s.1).is_some()));
        let reg = "https://registry.ollama.ai/v2";
        assert_eq!(
            manifest_url("qwen3:1.7b").unwrap(),
            format!("{reg}/library/qwen3/manifests/1.7b")
        );
        assert_eq!(
            manifest_url("gemma4").unwrap(),
            format!("{reg}/library/gemma4/manifests/latest")
        );
        assert_eq!(
            manifest_url("someone/model:q4").unwrap(),
            format!("{reg}/someone/model/manifests/q4")
        );
        assert_eq!(manifest_url("hf.co/someone/model"), None);
        assert_eq!(manifest_url("deepseek-v4.1-flash:cloud"), None);
        // 1.4 GB fits in 3.5 GB (2 GB stays free) but not in 3.3 GB.
        assert!(fits("qwen3:1.7b", 1_400_000_000, 3_500_000_000).is_ok());
        let err = fits("qwen3:1.7b", 1_400_000_000, 3_300_000_000)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("not enough disk space") && err.contains("3.3 GB is free"),
            "{err}"
        );
        assert!(free_bytes().is_some_and(|b| b > 0), "free space unknown");
    }
}
