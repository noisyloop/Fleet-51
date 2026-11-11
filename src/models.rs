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
    if let Some((repo, quant)) = hf_parts(name) {
        return hf_size(&repo, quant.as_deref());
    }
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

// ---- Hugging Face ----------------------------------------------------------------------------
// Ollama downloads GGUF models straight from Hugging Face by the name hf.co/<owner>/<repo>, with
// an optional quantization as the tag (hf.co/<owner>/<repo>:Q4_K_M; Q4_K_M when left out).
//
// No Hugging Face account or login is ever needed or used: Fleet-51 sends no token, offers only
// public, ungated models, and refuses gated ones (which need a login to accept their terms)
// before downloading, pointing to the ungated GGUF copies most models have.

const HF_API: &str = "https://huggingface.co/api";
/// The quantization Ollama picks from a Hugging Face repository when the name gives none.
pub const HF_DEFAULT_QUANT: &str = "Q4_K_M";

/// What people paste for a Hugging Face model, as the name Ollama downloads: a page link
/// (https://huggingface.co/owner/repo, even with /tree/main or /blob/... after it), `ollama run
/// hf.co/...`, or owner/repo:quant with hf.co/ in front. Anything else comes back trimmed.
pub fn normalize_name(input: &str) -> String {
    let mut s = input.trim();
    for p in ["ollama run ", "ollama pull "] {
        if let Some(r) = s.strip_prefix(p) {
            s = r.trim();
        }
    }
    for p in ["https://", "http://"] {
        if let Some(r) = s.strip_prefix(p) {
            s = r;
        }
    }
    let rest = ["huggingface.co/", "www.huggingface.co/", "hf.co/"]
        .iter()
        .find_map(|p| s.strip_prefix(p));
    let Some(rest) = rest else {
        return s.to_string();
    };
    let (path, tag) = match rest.split_once(':') {
        Some((p, t)) => (p, Some(t)),
        None => (rest, None),
    };
    let mut parts = path.split('/').filter(|p| !p.is_empty());
    let (Some(owner), Some(repo)) = (parts.next(), parts.next()) else {
        return format!("hf.co/{rest}");
    };
    // A link to one file names its quantization: .../blob/main/Model-Q5_K_M.gguf
    let file_quant = path
        .rsplit('/')
        .next()
        .filter(|f| f.to_ascii_lowercase().ends_with(".gguf"))
        .and_then(quant_of);
    match tag.map(str::to_string).or(file_quant) {
        Some(t) if !t.is_empty() => format!("hf.co/{owner}/{repo}:{t}"),
        _ => format!("hf.co/{owner}/{repo}"),
    }
}

/// `hf.co/owner/repo:quant` as ("owner/repo", Some("quant")).
fn hf_parts(name: &str) -> Option<(String, Option<String>)> {
    let rest = name
        .strip_prefix("hf.co/")
        .or_else(|| name.strip_prefix("huggingface.co/"))?;
    let (repo, tag) = match rest.split_once(':') {
        Some((r, t)) => (r, Some(t.to_string())),
        None => (rest, None),
    };
    valid_repo(repo).then(|| (repo.to_string(), tag.filter(|t| t != "latest")))
}

/// owner/repo, in the characters Hugging Face allows (so it is safe in an API path).
pub fn valid_repo(repo: &str) -> bool {
    let mut parts = repo.split('/');
    let ok = |p: Option<&str>| {
        p.is_some_and(|p| {
            !p.is_empty()
                && p.len() <= 96
                && !p.starts_with('.')
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
        })
    };
    ok(parts.next()) && ok(parts.next()) && parts.next().is_none()
}

/// The quantization in a GGUF file's name: "Llama-3.2-1B-Instruct-Q4_K_M.gguf" is Q4_K_M.
pub fn quant_of(file: &str) -> Option<String> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)(?:^|[-_./])((?:I?Q[1-8](?:_[0-9A-Z]{1,2})*)|BF16|F16|F32)(?:[-_.]|$)",
        )
        .expect("valid regex")
    });
    let name = file.rsplit('/').next().unwrap_or(file);
    re.captures(name).map(|c| c[1].to_ascii_uppercase())
}

fn hf_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(20)))
        .http_status_as_error(false)
        .build()
        .into()
}

fn hf_get(url: &str, query: &[(&str, &str)]) -> Result<Value> {
    let mut req = hf_agent().get(url).header("Accept", "application/json");
    for (k, v) in query {
        req = req.query(*k, *v);
    }
    let mut resp = req
        .call()
        .context("cannot reach Hugging Face (are you online?)")?;
    if resp.status() == 404 || resp.status() == 401 || resp.status() == 403 {
        bail!("that Hugging Face model doesn't exist or isn't public (Fleet-51 only uses public models, no login)");
    }
    if !resp.status().is_success() {
        bail!("Hugging Face answered {}", resp.status());
    }
    Ok(resp.body_mut().read_json()?)
}

/// GGUF models on Hugging Face matching `q`, most downloaded first.
pub fn hf_search(q: &str) -> Result<Vec<Value>> {
    let q = q.trim();
    if q.is_empty() || q.len() > 100 {
        bail!("type what to search for");
    }
    let v = hf_get(
        &format!("{HF_API}/models"),
        &[
            ("search", q),
            ("filter", "gguf"),
            ("sort", "downloads"),
            ("direction", "-1"),
            // Only models anyone can download without an account.
            ("gated", "false"),
            ("limit", "30"),
        ],
    )?;
    Ok(search_results(&v).into_iter().take(20).collect())
}

/// Gated models (`"gated": "auto"` or `"manual"`) need a login to accept their terms first.
fn needs_login(m: &Value) -> bool {
    m["private"] == true
        || m["disabled"] == true
        || m["gated"].as_bool().unwrap_or(!m["gated"].is_null())
}

fn login_needed(repo: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "{repo} needs a Hugging Face login (its makers ask you to accept their terms first), and \
         Fleet-51 never asks you to log in. Search for a GGUF copy of it instead, for example from \
         bartowski, unsloth or lmstudio-community: those download without an account."
    )
}

/// Refuse a model that can't be downloaded without logging in to Hugging Face, before Ollama
/// tries. When Hugging Face can't be asked (offline), the download itself will say.
pub fn hf_check_public(name: &str) -> Result<()> {
    let Some((repo, _)) = hf_parts(name) else {
        return Ok(());
    };
    match hf_get(&format!("{HF_API}/models/{repo}"), &[]) {
        Ok(info) if needs_login(&info) => Err(login_needed(&repo)),
        Ok(_) => Ok(()),
        Err(e) if e.to_string().contains("isn't public") => Err(e),
        Err(_) => Ok(()),
    }
}

fn search_results(v: &Value) -> Vec<Value> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let id = m["id"].as_str().or(m["modelId"].as_str())?;
            (valid_repo(id) && !needs_login(m)).then(|| {
                json!({
                    "repo": id,
                    "name": format!("hf.co/{id}"),
                    "downloads": m["downloads"].as_u64().unwrap_or(0),
                    "likes": m["likes"].as_u64().unwrap_or(0),
                    "task": m["pipeline_tag"],
                })
            })
        })
        .collect()
}

/// Every GGUF file in a repository with its size, including those in folders.
fn hf_files(repo: &str) -> Result<Vec<(String, u64)>> {
    if !valid_repo(repo) {
        bail!("not a Hugging Face model: {repo} (expected owner/name)");
    }
    let v = hf_get(
        &format!("{HF_API}/models/{repo}/tree/main"),
        &[("recursive", "true")],
    )?;
    Ok(gguf_files(&v))
}

fn gguf_files(v: &Value) -> Vec<(String, u64)> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter(|f| f["type"] != "directory")
        .filter_map(|f| {
            let path = f["path"].as_str()?;
            path.to_ascii_lowercase().ends_with(".gguf").then(|| {
                let size = f["lfs"]["size"]
                    .as_u64()
                    .or(f["size"].as_u64())
                    .unwrap_or(0);
                (path.to_string(), size)
            })
        })
        .collect()
}

fn is_projector(path: &str) -> bool {
    path.rsplit('/')
        .next()
        .unwrap_or(path)
        .to_ascii_lowercase()
        .contains("mmproj")
}

/// Group a repository's GGUF files by quantization (a big model is split over several files).
/// A vision model's projector (mmproj) is downloaded with every quantization, so it is added in.
fn quants_from(files: &[(String, u64)]) -> Vec<Value> {
    let projector = files
        .iter()
        .filter(|(p, _)| is_projector(p))
        .map(|(_, s)| *s)
        .min()
        .unwrap_or(0);
    let mut by: Vec<(String, u64, usize)> = vec![];
    for (path, size) in files.iter().filter(|(p, _)| !is_projector(p)) {
        let Some(q) = quant_of(path) else { continue };
        match by.iter_mut().find(|(x, _, _)| *x == q) {
            Some(e) => {
                e.1 += size;
                e.2 += 1;
            }
            None => by.push((q, *size, 1)),
        }
    }
    by.sort_by_key(|(_, s, _)| *s);
    by.into_iter()
        .map(|(q, s, n)| {
            json!({"quant": q, "bytes": s + projector, "files": n, "vision": projector > 0, "default": q == HF_DEFAULT_QUANT})
        })
        .collect()
}

/// The quantizations a repository offers, smallest first, each with its download name.
pub fn hf_quants(repo: &str) -> Result<Value> {
    let repo = normalize_name(repo);
    let repo = repo.strip_prefix("hf.co/").unwrap_or(&repo);
    let repo = repo.split(':').next().unwrap_or(repo);
    hf_check_public(&format!("hf.co/{repo}"))?;
    let files = hf_files(repo)?;
    if files.is_empty() {
        bail!("{repo} has no GGUF files, so Ollama can't run it. Look for a version of it with GGUF in the name.");
    }
    let quants: Vec<Value> = quants_from(&files)
        .into_iter()
        .map(|mut q| {
            q["name"] = json!(format!(
                "hf.co/{repo}:{}",
                q["quant"].as_str().unwrap_or("")
            ));
            q
        })
        .collect();
    Ok(json!({"repo": repo, "quants": quants}))
}

/// The download size of hf.co/<repo>:<quant>, before downloading. `None` when it can't be told.
fn hf_size(repo: &str, quant: Option<&str>) -> Option<u64> {
    let files = hf_files(repo).ok()?;
    let want = quant.unwrap_or(HF_DEFAULT_QUANT).to_ascii_uppercase();
    quants_from(&files)
        .iter()
        .find(|q| q["quant"].as_str() == Some(want.as_str()))
        .and_then(|q| q["bytes"].as_u64())
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

    /// Against the real Hugging Face (needs the internet): `cargo test -- --ignored hugging_face_live`.
    /// The build runs it, without failing the build if Hugging Face is down.
    #[test]
    #[ignore]
    fn hugging_face_live() {
        let found = hf_search("Llama-3.2-1B-Instruct").expect("search");
        assert!(!found.is_empty(), "no search results");
        let repos: Vec<&str> = found.iter().filter_map(|f| f["repo"].as_str()).collect();
        println!("search: {repos:?}");
        assert!(
            !repos.iter().any(|r| r.starts_with("meta-llama/")),
            "a gated repo was offered: {repos:?}"
        );
        let q = hf_quants("https://huggingface.co/bartowski/Llama-3.2-1B-Instruct-GGUF")
            .expect("sizes");
        let quants = q["quants"].as_array().expect("quants");
        println!("sizes: {}", serde_json::to_string(quants).unwrap());
        let q4 = quants
            .iter()
            .find(|x| x["quant"] == "Q4_K_M")
            .expect("Q4_K_M listed");
        let bytes = q4["bytes"].as_u64().unwrap();
        assert!(
            (500_000_000..1_500_000_000).contains(&bytes),
            "Q4_K_M is {bytes} bytes"
        );
        assert_eq!(
            q4["name"],
            "hf.co/bartowski/Llama-3.2-1B-Instruct-GGUF:Q4_K_M"
        );
        assert_eq!(
            download_size("hf.co/bartowski/Llama-3.2-1B-Instruct-GGUF"),
            Some(bytes)
        );
        hf_check_public("hf.co/bartowski/Llama-3.2-1B-Instruct-GGUF").expect("public repo refused");
        let err = hf_check_public("hf.co/meta-llama/Llama-3.2-1B")
            .unwrap_err()
            .to_string();
        assert!(err.contains("needs a Hugging Face login"), "{err}");
    }

    #[test]
    fn hugging_face_names_links_and_quantizations() {
        let n = |s| normalize_name(s);
        assert_eq!(n(" qwen3:1.7b "), "qwen3:1.7b");
        assert_eq!(
            n("https://huggingface.co/bartowski/Llama-3.2-1B-Instruct-GGUF"),
            "hf.co/bartowski/Llama-3.2-1B-Instruct-GGUF"
        );
        assert_eq!(
            n("huggingface.co/bartowski/Llama-3.2-1B-Instruct-GGUF/tree/main/"),
            "hf.co/bartowski/Llama-3.2-1B-Instruct-GGUF"
        );
        assert_eq!(
            n("https://huggingface.co/bartowski/Llama-3.2-1B-Instruct-GGUF/blob/main/Llama-3.2-1B-Instruct-Q5_K_M.gguf"),
            "hf.co/bartowski/Llama-3.2-1B-Instruct-GGUF:Q5_K_M"
        );
        assert_eq!(
            n("ollama run hf.co/unsloth/gemma-3-1b-it-GGUF:Q8_0"),
            "hf.co/unsloth/gemma-3-1b-it-GGUF:Q8_0"
        );
        let name = n("hf.co/unsloth/gemma-3-1b-it-GGUF:IQ4_XS");
        assert!(valid_name(&name), "{name}");
        assert_eq!(manifest_url(&name), None);
        assert_eq!(
            hf_parts(&name),
            Some(("unsloth/gemma-3-1b-it-GGUF".into(), Some("IQ4_XS".into())))
        );
        assert_eq!(hf_parts("hf.co/../etc"), None);
        assert!(
            !valid_repo("a/b/c")
                && !valid_repo("a/../b")
                && !valid_repo("a/.x")
                && valid_repo("a_b/c.d-e")
        );
        for (file, q) in [
            ("Llama-3.2-1B-Instruct-Q4_K_M.gguf", Some("Q4_K_M")),
            ("model.q8_0.gguf", Some("Q8_0")),
            ("Qwen3-30B-A3B-IQ4_XS.gguf", Some("IQ4_XS")),
            ("Q4_K_M/Big-Q4_K_M-00001-of-00002.gguf", Some("Q4_K_M")),
            ("phi-4-BF16.gguf", Some("BF16")),
            ("tiny-f16.gguf", Some("F16")),
            ("Qwen2.5-7B.gguf", None),
        ] {
            assert_eq!(quant_of(file).as_deref(), q, "{file}");
        }
    }

    #[test]
    fn hugging_face_search_and_quantization_sizes() {
        let found = search_results(&json!([
            {"id": "bartowski/Llama-3.2-1B-Instruct-GGUF", "downloads": 900, "likes": 50, "pipeline_tag": "text-generation"},
            {"id": "not a repo"},
            {"id": "meta-llama/Llama-3.2-1B", "gated": "manual"},
            {"id": "google/gemma-3-1b-it", "gated": "auto"},
            {"id": "someone/secret-GGUF", "private": true},
            {"id": "unsloth/gemma-3-1b-it-GGUF", "gated": false}
        ]));
        // Only public, ungated models: nothing that needs a Hugging Face login.
        let repos: Vec<_> = found.iter().map(|f| f["repo"].as_str().unwrap()).collect();
        assert_eq!(
            repos,
            [
                "bartowski/Llama-3.2-1B-Instruct-GGUF",
                "unsloth/gemma-3-1b-it-GGUF"
            ]
        );
        assert!(login_needed("meta-llama/Llama-3.2-1B")
            .to_string()
            .contains("never asks you to log in"));
        assert_eq!(
            found[0]["name"],
            "hf.co/bartowski/Llama-3.2-1B-Instruct-GGUF"
        );
        let tree = json!([
            {"type": "file", "path": "README.md", "size": 10},
            {"type": "directory", "path": "Q8_0"},
            {"type": "file", "path": "M-Q4_K_M.gguf", "size": 133, "lfs": {"size": 800}},
            {"type": "file", "path": "Q8_0/M-Q8_0-00001-of-00002.gguf", "size": 1000},
            {"type": "file", "path": "Q8_0/M-Q8_0-00002-of-00002.gguf", "size": 500},
            {"type": "file", "path": "mmproj-M-f16.gguf", "size": 100}
        ]);
        let files = gguf_files(&tree);
        assert_eq!(files.len(), 4);
        let q = quants_from(&files);
        assert_eq!(q.len(), 2, "{q:?}");
        // Smallest first; the projector is part of every download; split files add up.
        assert_eq!(
            (q[0]["quant"].as_str(), q[0]["bytes"].as_u64()),
            (Some("Q4_K_M"), Some(900))
        );
        assert_eq!(q[0]["default"], true);
        assert_eq!(
            (
                q[1]["quant"].as_str(),
                q[1]["bytes"].as_u64(),
                q[1]["files"].as_u64()
            ),
            (Some("Q8_0"), Some(1600), Some(2))
        );
        assert_eq!(q[1]["vision"], true);
    }
}
