//! Isolation tiers: `none` (the worktree is the only boundary), `docker` (container with host
//! networking so the agent can reach its API and Fleet-51's hooks), `docker-offline` (no network).
//! Paths are mounted at the same location inside the container so events and diffs line up.

use std::path::Path;

use anyhow::{bail, Result};

use crate::manifest::Manifest;

pub const TIERS: &[&str] = &["none", "docker", "docker-offline"];

pub struct Mounts<'a> {
    pub workspace: &'a Path,
    /// Paths outside the workspace root the agent needs: git metadata, in-place folders.
    pub extra: Vec<&'a Path>,
    pub overlay: &'a Path,
    pub account: Option<&'a Path>,
    pub exe: &'a Path,
}

pub fn prefix(tier: &str, m: &Manifest, mounts: &Mounts, env_keys: &[String], cwd: &Path, tty: bool) -> Result<Vec<String>> {
    let network = match tier {
        "" | "none" => return Ok(vec![]),
        "docker" => "host",
        "docker-offline" => "none",
        other => bail!("unknown sandbox {other}; choose one of {}", TIERS.join(", ")),
    };
    let image = m.sandbox_image.clone().unwrap_or_else(|| "debian:stable-slim".into());
    let mut v: Vec<String> = vec!["docker".into(), "run".into(), "--rm".into(), "-i".into()];
    if tty {
        v.push("-t".into());
    }
    v.extend(["--network".into(), network.into()]);
    if let Some(uid) = id_flag() {
        v.extend(["--user".into(), uid]);
    }
    let mut mount = |p: &Path, ro: bool| {
        let s = p.display().to_string();
        v.extend(["-v".into(), format!("{s}:{s}{}", if ro { ":ro" } else { "" })]);
    };
    mount(mounts.workspace, false);
    for p in &mounts.extra {
        mount(p, false);
    }
    mount(mounts.overlay, false);
    if let Some(a) = mounts.account {
        mount(a, false);
    }
    mount(mounts.exe, true);
    v.extend(["-w".into(), cwd.display().to_string()]);
    for k in env_keys {
        v.extend(["-e".into(), k.clone()]);
    }
    v.push(image);
    Ok(v)
}

fn id_flag() -> Option<String> {
    let run = |a: &str| std::process::Command::new("id").arg(a).output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    Some(format!("{}:{}", run("-u")?, run("-g")?)).filter(|s| s != ":")
}
