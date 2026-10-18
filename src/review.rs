//! Review is the part Fleet-51 owns instead of being half an IDE: one diff across every repo in a
//! workspace, line annotations sent back as structured feedback, attempt comparison and merging.

use std::process::Command;

use anyhow::{bail, Result};
use serde::Serialize;

use crate::events::AnnotationState;
use crate::workspace::{git, shadow, Checkout, Workspace};

#[derive(Clone, Debug, Serialize)]
pub struct FileStat {
    pub path: String,
    pub added: u64,
    pub deleted: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct CheckoutDiff {
    pub checkout: String,
    pub kind: String,
    pub path: String,
    pub patch: String,
    pub files: Vec<FileStat>,
    pub error: Option<String>,
}

fn parse_numstat(s: &str) -> Vec<FileStat> {
    s.lines()
        .filter_map(|l| {
            let mut it = l.splitn(3, '\t');
            let a = it.next()?.parse().unwrap_or(0);
            let d = it.next()?.parse().unwrap_or(0);
            Some(FileStat { path: it.next()?.to_string(), added: a, deleted: d })
        })
        .collect()
}

fn checkout_diff(ws: &Workspace, c: &Checkout) -> Result<(String, Vec<FileStat>)> {
    match c.kind.as_str() {
        "git" => {
            // Intent-to-add makes new files show up in the diff; this is Fleet-51's own worktree.
            git(&c.path, &["add", "-A", "-N"])?;
            let base = c.base.as_deref().unwrap_or("HEAD");
            let patch = git(&c.path, &["diff", "--no-color", base])?;
            let stats = git(&c.path, &["diff", "--numstat", base])?;
            Ok((patch, parse_numstat(&stats)))
        }
        "folder" => {
            shadow(ws, c, &["add", "-A", "-N"])?;
            let patch = shadow(ws, c, &["diff", "--no-color", "HEAD"])?;
            let stats = shadow(ws, c, &["diff", "--numstat", "HEAD"])?;
            Ok((patch, parse_numstat(&stats)))
        }
        "jj" => {
            let out = Command::new("jj").arg("-R").arg(&c.path).args(["diff", "--git"]).output()?;
            let patch = String::from_utf8_lossy(&out.stdout).to_string();
            let out = Command::new("jj").arg("-R").arg(&c.path).args(["diff", "--stat"]).output()?;
            let files = String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter_map(|l| l.split_once('|'))
                .map(|(p, _)| FileStat { path: p.trim().to_string(), added: 0, deleted: 0 })
                .collect();
            Ok((patch, files))
        }
        other => bail!("cannot diff checkout kind {other}"),
    }
}

pub fn diff(ws: &Workspace) -> Vec<CheckoutDiff> {
    ws.checkouts
        .iter()
        .map(|c| {
            let (patch, files, error) = match checkout_diff(ws, c) {
                Ok((p, f)) => (p, f, None),
                Err(e) => (String::new(), vec![], Some(e.to_string())),
            };
            CheckoutDiff { checkout: c.name.clone(), kind: c.kind.clone(), path: c.path.display().to_string(), patch, files, error }
        })
        .collect()
}

/// Turn unsent line annotations into one feedback prompt, quoting the line each comment is on.
pub fn feedback_prompt(ws: &Workspace, notes: &[AnnotationState]) -> String {
    let mut out = String::from("Review feedback on your changes. Address each point, then summarise what you changed.\n");
    for a in notes {
        let line_text = ws
            .checkouts
            .iter()
            .map(|c| c.path.join(&a.file))
            .chain(std::iter::once(ws.agent_cwd().join(&a.file)))
            .find_map(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| t.lines().nth(a.line.saturating_sub(1) as usize).map(|l| l.trim().to_string()));
        match line_text {
            Some(l) if !l.is_empty() => out.push_str(&format!("\n- {}:{} (`{}`): {}", a.file, a.line, l, a.comment)),
            _ => out.push_str(&format!("\n- {}:{}: {}", a.file, a.line, a.comment)),
        }
    }
    out
}

#[derive(Debug, Serialize)]
pub struct MergeResult {
    pub checkout: String,
    pub branch: String,
    pub into: String,
    pub commit: String,
}

/// Commit the agent's work on its branch and merge it into the branch the repo was on.
pub fn merge(ws: &Workspace, message: &str) -> Result<Vec<MergeResult>> {
    let mut results = vec![];
    for c in &ws.checkouts {
        if c.kind != "git" {
            bail!("merge is implemented for git checkouts; {} is {}", c.name, c.kind);
        }
        let branch = c.branch.clone().unwrap_or_default();
        git(&c.path, &["add", "-A"])?;
        if !git(&c.path, &["status", "--porcelain"])?.is_empty() {
            git(&c.path, &["-c", "user.name=fleet51", "-c", "user.email=fleet51@localhost", "commit", "-q", "-m", message])?;
        }
        let into = c.source_branch.clone().unwrap_or_else(|| "HEAD".into());
        let current = git(&c.source, &["symbolic-ref", "--short", "-q", "HEAD"]).unwrap_or_default();
        if current != into {
            bail!("{} is on {current:?}, expected {into}; switch back before merging", c.source.display());
        }
        if !git(&c.source, &["status", "--porcelain", "--untracked-files=no"])?.is_empty() {
            bail!("{} has uncommitted changes; commit or stash them first", c.source.display());
        }
        let msg = format!("Merge {branch} (Fleet-51)");
        if let Err(e) = git(&c.source, &["-c", "user.name=fleet51", "-c", "user.email=fleet51@localhost", "merge", "--no-ff", "-m", &msg, &branch]) {
            let _ = git(&c.source, &["merge", "--abort"]);
            bail!("merge of {branch} conflicted and was aborted: {e}");
        }
        let commit = git(&c.source, &["rev-parse", "HEAD"])?;
        results.push(MergeResult { checkout: c.name.clone(), branch, into, commit });
    }
    Ok(results)
}
