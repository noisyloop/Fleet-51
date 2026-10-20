//! The local model runtime, so Fleet-51 works out of the box. In order of preference:
//! 1. an Ollama the user is already running (OLLAMA_HOST or 127.0.0.1:11434);
//! 2. a Fleet-51-managed Ollama on a private port, started from an `ollama` already installed;
//! 3. on macOS and Windows, a pinned Ollama release downloaded on first launch and checked
//!    against SHA-256s built into Fleet-51: on macOS the hash Ollama publishes in
//!    sha256sum.txt; on Windows one per file (see `download_windows`).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::RwLock;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::home::Home;

pub const OLLAMA_VERSION: &str = "v0.40.6";
const DARWIN_TGZ_SHA256: &str = "b2b55434865a668decb494110d7fd970eed3ecc8837be2872f672ccbb95df974";
/// The files taken from Ollama's Windows zip, each with its SHA-256. Made by
/// scripts/ollama_windows_files.py from the official zip, checked against Ollama's own hash.
const WINDOWS_FILES: &str = include_str!("ollama-windows-files.txt");
/// The Ollama program's file name on this system.
const OLLAMA_EXE: &str = if cfg!(windows) {
    "ollama.exe"
} else {
    "ollama"
};
const MANAGED_ADDR: &str = "127.0.0.1:11435";
/// Pulled automatically on first launch when no model is installed.
pub const DEFAULT_MODEL: &str = "qwen3:1.7b";

/// The Ollama this daemon settled on; read by everything that talks to Ollama.
static ROOT: RwLock<Option<String>> = RwLock::new(None);

/// In the daemon: the settled address. In agent processes: the address the daemon last wrote.
pub fn root() -> Option<String> {
    if let Some(r) = ROOT.read().unwrap().clone() {
        return Some(r);
    }
    std::fs::read_to_string(Home::resolve().root.join("runtime").join("ollama-url"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn settle(home: &Home, root: &str) {
    *ROOT.write().unwrap() = Some(root.to_string());
    let dir = home.root.join("runtime");
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join("ollama-url"), root);
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Runtime {
    Checking,
    Downloading { completed: u64, total: u64 },
    Starting,
    Ready { source: String, url: String },
    Failed { message: String },
}

fn reachable(root: &str) -> bool {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(2)))
        .build()
        .into();
    agent.get(&format!("{root}/api/version")).call().is_ok()
}

fn system_root() -> String {
    let host = std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "127.0.0.1:11434".into());
    let host = if host.starts_with("http") {
        host
    } else {
        format!("http://{host}")
    };
    host.trim_end_matches('/').to_string()
}

fn on_path(cmd: &str) -> Option<PathBuf> {
    let local =
        std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Programs").join("Ollama"));
    [
        "/opt/homebrew/bin",
        "/usr/local/bin",
        "/Applications/Ollama.app/Contents/Resources",
    ]
    .iter()
    .map(PathBuf::from)
    .chain(local)
    .chain(
        std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
            .unwrap_or_default(),
    )
    .map(|d| d.join(cmd))
    .find(|p| p.is_file())
}

/// Download the pinned macOS build into `dir`, reporting progress, and verify its checksum.
/// Download `file` from the pinned Ollama release into `dir`, checking its SHA-256.
fn download_checked(
    dir: &Path,
    file: &str,
    sha256: &str,
    report: &dyn Fn(Runtime),
) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let url = format!("https://github.com/ollama/ollama/releases/download/{OLLAMA_VERSION}/{file}");
    let resp = ureq::get(&url).call().context("downloading Ollama")?;
    let total = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut reader = resp.into_body().into_with_config().limit(u64::MAX).reader();
    let part = dir.join(format!("{file}.part"));
    let mut out = std::fs::File::create(&part)?;
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    let (mut done, mut last) = (0u64, Instant::now());
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        hash.update(&buf[..n]);
        done += n as u64;
        if last.elapsed() > Duration::from_millis(250) {
            last = Instant::now();
            report(Runtime::Downloading {
                completed: done,
                total,
            });
        }
    }
    out.flush()?;
    let got: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if got != sha256 {
        let _ = std::fs::remove_file(&part);
        bail!("Ollama download failed its checksum (got {got}); nothing was installed");
    }
    Ok(part)
}

fn download_darwin(dir: &Path, report: &dyn Fn(Runtime)) -> Result<PathBuf> {
    let tgz = download_checked(dir, "ollama-darwin.tgz", DARWIN_TGZ_SHA256, report)?;
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(&tgz)
        .arg("-C")
        .arg(dir)
        .status()?;
    let _ = std::fs::remove_file(&tgz);
    if !status.success() {
        bail!("could not unpack Ollama");
    }
    Ok(dir.join("ollama"))
}

/// Windows: the pinned zip is about 1.4 GB, nearly all NVIDIA CUDA libraries. Only the files in
/// `WINDOWS_FILES` (about 37 MB: the program, CPU support and Vulkan, which runs models on
/// NVIDIA, AMD and Intel graphics) are read out of it, each checked against its SHA-256. They
/// go into a scratch folder that becomes `dir` only once everything has arrived.
fn download_windows(dir: &Path, report: &dyn Fn(Runtime)) -> Result<PathBuf> {
    let url = format!("https://github.com/ollama/ollama/releases/download/{OLLAMA_VERSION}/ollama-windows-amd64.zip");
    download_windows_from(&url, dir, report)
}

fn download_windows_from(url: &str, dir: &Path, report: &dyn Fn(Runtime)) -> Result<PathBuf> {
    let files: Vec<(&str, &str)> = WINDOWS_FILES
        .lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.split_once("  "))
        .collect();
    let name = dir.file_name().context("no folder for Ollama")?;
    let part = dir.with_file_name(format!("{}.part", name.to_string_lossy()));
    let _ = std::fs::remove_dir_all(&part);
    std::fs::create_dir_all(&part)?;
    let fetched = crate::zip_range::fetch_files(url, &part, &files, &|completed, total| {
        report(Runtime::Downloading { completed, total })
    });
    if let Err(e) = fetched {
        let _ = std::fs::remove_dir_all(&part);
        return Err(e.context("downloading Ollama failed; nothing was installed"));
    }
    let _ = std::fs::remove_dir_all(dir);
    // Antivirus scanning the new files can hold them open for a moment.
    let mut tries = 0;
    while let Err(e) = std::fs::rename(&part, dir) {
        tries += 1;
        if tries == 20 {
            return Err(e).context("could not put Ollama in place");
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Ok(dir.join(OLLAMA_EXE))
}

/// Find or set up Ollama. Returns the managed child process if Fleet-51 started one.
pub fn ensure(home: &Home, report: &dyn Fn(Runtime)) -> Result<Option<Child>> {
    report(Runtime::Checking);
    let system = system_root();
    let managed = format!("http://{MANAGED_ADDR}");
    for (root, source) in [(system.as_str(), "system"), (managed.as_str(), "managed")] {
        if reachable(root) {
            settle(home, root);
            report(Runtime::Ready {
                source: source.into(),
                url: root.into(),
            });
            return Ok(None);
        }
    }
    let dir = home
        .root
        .join("runtime")
        .join(format!("ollama-{OLLAMA_VERSION}"));
    let bin = if dir.join(OLLAMA_EXE).is_file() {
        dir.join(OLLAMA_EXE)
    } else if let Some(installed) = on_path(OLLAMA_EXE) {
        installed
    } else if cfg!(target_os = "macos") {
        download_darwin(&dir, report)?
    } else if cfg!(windows) {
        download_windows(&dir, report)?
    } else {
        bail!("Ollama is not installed. Install it from https://ollama.com and reopen Fleet-51.");
    };
    report(Runtime::Starting);
    std::fs::create_dir_all(home.root.join("runtime"))?;
    let log = std::fs::File::create(home.root.join("runtime").join("ollama.log"))?;
    let log_path = home.root.join("runtime").join("ollama.log");
    let mut cmd = Command::new(&bin);
    cmd.arg("serve")
        .env("OLLAMA_HOST", MANAGED_ADDR)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    crate::home::no_window(&mut cmd);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("starting {}", bin.display()))?;
    // The first start of a freshly downloaded Ollama can take minutes while macOS checks it.
    let started = Instant::now();
    while started.elapsed() < START_TIMEOUT {
        if reachable(&managed) {
            settle(home, &managed);
            report(Runtime::Ready {
                source: "managed".into(),
                url: managed,
            });
            return Ok(Some(child));
        }
        if let Ok(Some(status)) = child.try_wait() {
            bail!(
                "Ollama stopped while starting ({status}): {}",
                log_tail(&log_path)
            );
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let _ = child.kill();
    let _ = child.wait();
    bail!(
        "Ollama did not start within {} seconds: {}",
        START_TIMEOUT.as_secs(),
        log_tail(&log_path)
    )
}

const START_TIMEOUT: Duration = Duration::from_secs(300);

/// The end of Ollama's log, to say why it did not start.
fn log_tail(path: &std::path::Path) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = lines[lines.len().saturating_sub(3)..].join(" | ");
    if tail.is_empty() {
        format!("no output (see {})", path.display())
    } else {
        format!("{tail} (see {})", path.display())
    }
}

/// This computer's memory in GB, to suggest a model that fits.
pub fn memory_gb() -> Option<u64> {
    static MEMORY: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *MEMORY.get_or_init(read_memory_gb)
}

fn read_memory_gb() -> Option<u64> {
    let bytes = if cfg!(windows) {
        let mut c = Command::new("powershell.exe");
        c.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "(Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory",
        ]);
        crate::home::no_window(&mut c);
        let out = c.output().ok()?;
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse::<u64>()
            .ok()?
    } else if cfg!(target_os = "macos") {
        let out = Command::new("/usr/sbin/sysctl")
            .args(["-n", "hw.memsize"])
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse::<u64>()
            .ok()?
    } else {
        let info = std::fs::read_to_string("/proc/meminfo").ok()?;
        let kb = info
            .lines()
            .find(|l| l.starts_with("MemTotal:"))?
            .split_whitespace()
            .nth(1)?
            .parse::<u64>()
            .ok()?;
        kb * 1024
    };
    Some((bytes + (1 << 29)) >> 30)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real thing, from GitHub: `cargo test -- --ignored` after changing OLLAMA_VERSION.
    /// FLEET51_TEST_OLLAMA_ZIP points it at a copy or relay of the zip instead.
    #[test]
    #[ignore = "downloads about 37 MB"]
    fn the_pinned_windows_files_download_and_match() {
        let base = std::env::temp_dir().join(format!("fleet51-ollama-win-{}", std::process::id()));
        let dir = base.join(format!("ollama-{OLLAMA_VERSION}"));
        let total = std::sync::Mutex::new(0);
        let report = |r| {
            if let Runtime::Downloading { total: t, .. } = r {
                *total.lock().unwrap() = t;
            }
        };
        let got = match std::env::var("FLEET51_TEST_OLLAMA_ZIP") {
            Ok(url) => download_windows_from(&url, &dir, &report),
            Err(_) => download_windows(&dir, &report),
        }
        .unwrap_or_else(|e| panic!("{e:#}"));
        assert_eq!(got, dir.join(OLLAMA_EXE));
        assert!(dir.join("ollama.exe").is_file());
        assert!(dir.join("lib/ollama/vulkan/ggml-vulkan.dll").is_file());
        assert!(!dir.join("lib/ollama/cuda_v13").exists());
        let mb = *total.lock().unwrap() / 1_000_000;
        assert!((20..60).contains(&mb), "{mb} MB");
        let _ = std::fs::remove_dir_all(&base);
    }
}
