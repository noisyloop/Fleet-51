//! A workspace is a set of checkouts. Each checkout is a git worktree, a jj workspace, or a plain
//! folder (diffed through a shadow git dir, so non-git folders still get review and stats).
//! Pointing at a subdirectory of a monorepo checks out the whole repo and runs the agent there.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::home::{now_ms, short_id, Home};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RepoSpec {
    pub path: String,
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Checkout {
    pub name: String,
    pub kind: String,
    pub source: PathBuf,
    pub path: PathBuf,
    pub subdir: Option<String>,
    pub base: Option<String>,
    pub branch: Option<String>,
    /// Branch checked out in `source` when the workspace was made; merges go here.
    pub source_branch: Option<String>,
}

impl Checkout {
    pub fn cwd(&self) -> PathBuf {
        match &self.subdir {
            Some(s) => self.path.join(s),
            None => self.path.clone(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Workspace {
    pub id: String,
    pub name: String,
    pub root: PathBuf,
    pub checkouts: Vec<Checkout>,
    pub created: u64,
}

impl Workspace {
    /// One repo: run inside it. Several repos: run at the root so the agent sees all of them.
    pub fn agent_cwd(&self) -> PathBuf {
        match self.checkouts.as_slice() {
            [one] => one.cwd(),
            _ => self.root.clone(),
        }
    }

    pub fn shadow_git(&self, c: &Checkout) -> PathBuf {
        self.root.join(format!(".shadow-{}.git", c.name))
    }
}

pub fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().context("running git")?;
    if !out.status.success() {
        bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

pub fn shadow(ws: &Workspace, c: &Checkout, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg(format!("--git-dir={}", ws.shadow_git(c).display()))
        .arg(format!("--work-tree={}", c.path.display()))
        .args(["-c", "user.name=fleet51", "-c", "user.email=fleet51@localhost"])
        .args(args)
        .output()?;
    if !out.status.success() {
        bail!("shadow git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

fn which_kind(path: &Path) -> String {
    let jj_root = Command::new("jj").arg("-R").arg(path).arg("root").output();
    if jj_root.map(|o| o.status.success()).unwrap_or(false) {
        return "jj".into();
    }
    if git(path, &["rev-parse", "--show-toplevel"]).is_ok() {
        return "git".into();
    }
    "folder".into()
}

fn slug(s: &str) -> String {
    let out: String = s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c.to_ascii_lowercase() } else { '-' }).collect();
    out.trim_matches('-').chars().take(40).collect()
}

pub fn create(home: &Home, name: &str, repos: &[RepoSpec]) -> Result<Workspace> {
    if repos.is_empty() {
        bail!("a workspace needs at least one repo or folder");
    }
    let id = short_id("w");
    let root = home.workspaces().join(&id);
    std::fs::create_dir_all(&root)?;
    let mut ws = Workspace { id: id.clone(), name: name.to_string(), root: root.clone(), checkouts: vec![], created: now_ms() };
    for r in repos {
        let given = std::fs::canonicalize(&r.path).with_context(|| format!("{} does not exist", r.path))?;
        let kind = r.kind.clone().unwrap_or_else(|| which_kind(&given));
        let mut co_name = given.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "repo".into());
        while ws.checkouts.iter().any(|c| c.name == co_name) {
            co_name.push_str("-2");
        }
        let co = match kind.as_str() {
            "git" => {
                let top = PathBuf::from(git(&given, &["rev-parse", "--show-toplevel"])?);
                let subdir = given.strip_prefix(&top).ok().filter(|p| !p.as_os_str().is_empty()).map(|p| p.display().to_string());
                co_name = top.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or(co_name);
                while ws.checkouts.iter().any(|c| c.name == co_name) {
                    co_name.push_str("-2");
                }
                let base = git(&top, &["rev-parse", "HEAD"])?;
                let source_branch = git(&top, &["symbolic-ref", "--short", "-q", "HEAD"]).ok();
                let branch = format!("fleet51/{}-{}", slug(name), &id[2..]);
                let path = root.join(&co_name);
                git(&top, &["worktree", "add", "-q", "-b", &branch, &path.display().to_string(), &base])?;
                Checkout { name: co_name, kind, source: top, path, subdir, base: Some(base), branch: Some(branch), source_branch }
            }
            "jj" => {
                let path = root.join(&co_name);
                let out = Command::new("jj")
                    .arg("-R")
                    .arg(&given)
                    .args(["workspace", "add", "--name", &format!("fleet51-{id}")])
                    .arg(&path)
                    .output()?;
                if !out.status.success() {
                    bail!("jj workspace add: {}", String::from_utf8_lossy(&out.stderr).trim());
                }
                Checkout { name: co_name, kind, source: given.clone(), path, subdir: None, base: Some("@-".into()), branch: None, source_branch: None }
            }
            "folder" => {
                let c = Checkout { name: co_name, kind, source: given.clone(), path: given.clone(), subdir: None, base: None, branch: None, source_branch: None };
                shadow(&ws, &c, &["init", "-q"]).or_else(|_| {
                    // `init` needs the git dir to exist as a bare-ish location.
                    std::fs::create_dir_all(ws.shadow_git(&c))?;
                    shadow(&ws, &c, &["init", "-q"])
                })?;
                shadow(&ws, &c, &["add", "-A"])?;
                shadow(&ws, &c, &["commit", "-q", "--allow-empty", "-m", "fleet51 snapshot"])?;
                c
            }
            other => bail!("unknown checkout kind {other} (use git, jj or folder)"),
        };
        ws.checkouts.push(co);
    }
    save(home, &ws)?;
    Ok(ws)
}

pub fn save(home: &Home, ws: &Workspace) -> Result<()> {
    std::fs::create_dir_all(&ws.root)?;
    std::fs::write(home.workspaces().join(&ws.id).join("workspace.json"), serde_json::to_string_pretty(ws)?)?;
    Ok(())
}

pub fn list(home: &Home) -> Vec<Workspace> {
    let mut out: Vec<Workspace> = std::fs::read_dir(home.workspaces())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| std::fs::read_to_string(e.path().join("workspace.json")).ok())
        .filter_map(|t| serde_json::from_str(&t).ok())
        .collect();
    out.sort_by_key(|w| w.created);
    out
}

pub fn get(home: &Home, id: &str) -> Result<Workspace> {
    let all = list(home);
    let hits: Vec<_> = all.into_iter().filter(|w| w.id.starts_with(id) || w.name == id).collect();
    match hits.len() {
        1 => Ok(hits.into_iter().next().unwrap()),
        0 => bail!("no workspace {id}"),
        _ => bail!("{id} matches several workspaces"),
    }
}

pub fn remove(home: &Home, id: &str) -> Result<()> {
    let ws = get(home, id)?;
    for c in &ws.checkouts {
        match c.kind.as_str() {
            "git" => {
                let _ = git(&c.source, &["worktree", "remove", "--force", &c.path.display().to_string()]);
            }
            "jj" => {
                let _ = Command::new("jj").arg("-R").arg(&c.source).args(["workspace", "forget", &format!("fleet51-{}", ws.id)]).status();
            }
            _ => {}
        }
    }
    std::fs::remove_dir_all(&ws.root)?;
    Ok(())
}
