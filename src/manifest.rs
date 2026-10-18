//! Agents are data: a TOML manifest describes how to launch, talk to, resume and account-switch
//! an agent. Built-ins ship as manifests; files in ~/.fleet51/agents/ add or override them.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::home::Home;

const BUILTIN: &[(&str, &str)] = &[
    ("claude", include_str!("../agents/claude.toml")),
    ("codex", include_str!("../agents/codex.toml")),
    ("gemini", include_str!("../agents/gemini.toml")),
    ("opencode", include_str!("../agents/opencode.toml")),
    ("ollama", include_str!("../agents/ollama.toml")),
    (
        "openai-compatible",
        include_str!("../agents/openai-compatible.toml"),
    ),
    ("mock-claude", include_str!("../agents/mock-claude.toml")),
    ("mock-codex", include_str!("../agents/mock-codex.toml")),
    ("mock-acp", include_str!("../agents/mock-acp.toml")),
    ("mock-pty", include_str!("../agents/mock-pty.toml")),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Protocol {
    ClaudeStreamJson,
    CodexExecJson,
    Acp,
    Pty,
}

impl Protocol {
    pub fn name(self) -> &'static str {
        match self {
            Protocol::ClaudeStreamJson => "claude-stream-json",
            Protocol::CodexExecJson => "codex-exec-json",
            Protocol::Acp => "acp",
            Protocol::Pty => "pty",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OverlayFile {
    pub path: String,
    pub content: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PtyRules {
    pub ready_pattern: String,
    pub idle_pattern: Option<String>,
    #[serde(default = "default_submit")]
    pub submit: String,
}

fn default_submit() -> String {
    "\r".into()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub name: String,
    #[serde(default)]
    pub display: String,
    pub protocol: Protocol,
    pub command: Vec<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub resume_args: Vec<String>,
    #[serde(default)]
    pub model_args: Vec<String>,
    #[serde(default)]
    pub prompt_args: Vec<String>,
    #[serde(default)]
    pub resume_prompt_args: Vec<String>,
    #[serde(default)]
    pub permission_args: BTreeMap<String, Vec<String>>,
    pub account_env: Option<String>,
    pub account_provider: Option<String>,
    /// Whether a provider session id stays valid after switching accounts.
    #[serde(default)]
    pub resume_portable: bool,
    pub rate_limit_pattern: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub overlay: Vec<OverlayFile>,
    #[serde(default)]
    pub overlay_args: Vec<String>,
    pub sandbox_image: Option<String>,
    pub pty: Option<PtyRules>,
    pub install_hint: Option<String>,
    #[serde(default)]
    pub source: String,
}

impl Manifest {
    pub fn provider(&self) -> String {
        self.account_provider
            .clone()
            .unwrap_or_else(|| self.name.clone())
    }

    pub fn rate_limit_re(&self) -> Option<regex::Regex> {
        self.rate_limit_pattern
            .as_deref()
            .and_then(|p| regex::Regex::new(p).ok())
    }

    pub fn available(&self) -> bool {
        match self.command.first() {
            Some(c) if c == "{fleet51}" => true,
            Some(c) => which(c),
            None => false,
        }
    }
}

pub fn which(cmd: &str) -> bool {
    if cmd.contains('/') {
        return Path::new(cmd).exists();
    }
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(cmd).is_file()))
        .unwrap_or(false)
}

pub fn load_all(home: &Home) -> Vec<Manifest> {
    let mut by_name: BTreeMap<String, Manifest> = BTreeMap::new();
    for (name, text) in BUILTIN {
        match toml::from_str::<Manifest>(text) {
            Ok(mut m) => {
                m.source = "builtin".into();
                by_name.insert(m.name.clone(), m);
            }
            Err(e) => eprintln!("fleet51: builtin manifest {name} is invalid: {e}"),
        }
    }
    if let Ok(dir) = std::fs::read_dir(home.agents()) {
        for entry in dir.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let parsed = std::fs::read_to_string(&path)
                .map_err(anyhow::Error::from)
                .and_then(|t| toml::from_str::<Manifest>(&t).map_err(anyhow::Error::from));
            match parsed {
                Ok(mut m) => {
                    m.source = path.display().to_string();
                    by_name.insert(m.name.clone(), m);
                }
                Err(e) => eprintln!("fleet51: skipping {}: {e}", path.display()),
            }
        }
    }
    by_name.into_values().collect()
}

/// Replace `{key}` placeholders. Unknown placeholders are left as-is.
pub fn expand(s: &str, vars: &[(&str, &str)]) -> String {
    let mut out = s.to_string();
    for (k, v) in vars {
        out = out.replace(&format!("{{{k}}}"), v);
    }
    out
}

pub fn expand_all(items: &[String], vars: &[(&str, &str)]) -> Vec<String> {
    items.iter().map(|s| expand(s, vars)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_parse() {
        let home = Home {
            root: std::env::temp_dir().join("fleet51-manifest-test-none"),
        };
        let all = load_all(&home);
        assert_eq!(all.len(), BUILTIN.len());
        let claude = all.iter().find(|m| m.name == "claude").unwrap();
        assert_eq!(claude.protocol, Protocol::ClaudeStreamJson);
        assert!(claude.overlay_args.contains(&"--settings".to_string()));
    }
}
