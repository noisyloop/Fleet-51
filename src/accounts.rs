//! Several accounts per provider, each an isolated config dir handed to the agent through the
//! manifest's `account_env` (for example CLAUDE_CONFIG_DIR). Rate-limited accounts cool down.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{bail, Result};
use serde::Serialize;

use crate::home::{now_ms, Home};

#[derive(Debug, Serialize)]
pub struct Account {
    pub provider: String,
    pub name: String,
    pub dir: PathBuf,
    pub cooldown_until: Option<u64>,
}

fn cooldowns_file(home: &Home) -> PathBuf {
    home.accounts().join("cooldowns.json")
}

fn cooldowns(home: &Home) -> BTreeMap<String, u64> {
    std::fs::read_to_string(cooldowns_file(home))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn dir(home: &Home, provider: &str, name: &str) -> PathBuf {
    home.accounts().join(provider).join(name)
}

pub fn add(home: &Home, provider: &str, name: &str) -> Result<PathBuf> {
    let ok = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    };
    if !ok(provider) || !ok(name) {
        bail!("provider and account names may only use letters, digits, - and _");
    }
    let d = dir(home, provider, name);
    std::fs::create_dir_all(&d)?;
    Ok(d)
}

pub fn list(home: &Home) -> Vec<Account> {
    let cd = cooldowns(home);
    let now = now_ms();
    let mut out = vec![];
    for p in std::fs::read_dir(home.accounts())
        .into_iter()
        .flatten()
        .flatten()
    {
        if !p.path().is_dir() {
            continue;
        }
        let provider = p.file_name().to_string_lossy().to_string();
        for a in std::fs::read_dir(p.path()).into_iter().flatten().flatten() {
            if !a.path().is_dir() {
                continue;
            }
            let name = a.file_name().to_string_lossy().to_string();
            let until = cd
                .get(&format!("{provider}/{name}"))
                .copied()
                .filter(|u| *u > now);
            out.push(Account {
                provider: provider.clone(),
                name,
                dir: a.path(),
                cooldown_until: until,
            });
        }
    }
    out.sort_by(|a, b| (&a.provider, &a.name).cmp(&(&b.provider, &b.name)));
    out
}

pub fn cool_down(home: &Home, provider: &str, name: &str, secs: u64) {
    let mut cd = cooldowns(home);
    cd.insert(format!("{provider}/{name}"), now_ms() + secs * 1000);
    let _ = std::fs::write(
        cooldowns_file(home),
        serde_json::to_string_pretty(&cd).unwrap_or_default(),
    );
}

/// First account for `provider` that is not cooling down and is not `exclude`.
pub fn pick(home: &Home, provider: &str, exclude: Option<&str>) -> Option<String> {
    list(home)
        .into_iter()
        .filter(|a| {
            a.provider == provider && a.cooldown_until.is_none() && Some(a.name.as_str()) != exclude
        })
        .map(|a| a.name)
        .next()
}
