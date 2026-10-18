mod accounts;
mod adapters;
mod client;
mod daemon;
mod events;
mod home;
mod manifest;
mod mock;
mod review;
mod sandbox;
mod server;
mod session;
mod workspace;

use std::io::Read;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use serde_json::{json, Value};

use client::Client;
use home::Home;

#[derive(Parser)]
#[command(name = "fleet51", version, about = "Fleet-51: run a fleet of coding agents from one headless daemon")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon in the foreground (takes over from any running generation)
    Daemon {
        #[arg(long, default_value = "127.0.0.1:7451")]
        listen: String,
    },
    /// Start the daemon in the background
    Up {
        #[arg(long, default_value = "127.0.0.1:7451")]
        listen: String,
    },
    /// Stop the daemon; running sessions are suspended and resume on the next start
    Down,
    /// Daemon status
    Status,
    /// Print the web UI link (open it on any device that can reach the daemon)
    Url,
    /// List agent manifests and whether each is installed
    Agents,
    /// Workspaces: sets of checkouts (git worktrees, jj workspaces, plain folders)
    #[command(subcommand)]
    Ws(WsCmd),
    /// Start an agent session
    Run {
        agent: String,
        /// Repo or folder (repeatable; a subdirectory of a monorepo works)
        #[arg(long = "repo")]
        repos: Vec<String>,
        /// Use an existing workspace instead of creating one
        #[arg(long)]
        ws: Option<String>,
        #[arg(short, long)]
        prompt: Option<String>,
        /// none | docker | docker-offline
        #[arg(long)]
        sandbox: Option<String>,
        /// default | plan | auto | yolo (as mapped by the agent's manifest)
        #[arg(long)]
        permission: Option<String>,
        /// Account name, or "auto" to pick one that is not rate limited
        #[arg(long)]
        account: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        title: Option<String>,
        /// Stream the log until the first turn finishes
        #[arg(short, long)]
        wait: bool,
    },
    /// Send one prompt to several agents, each in its own workspace, to compare attempts
    Fanout {
        #[arg(long, value_delimiter = ',')]
        agents: Vec<String>,
        #[arg(long = "repo")]
        repos: Vec<String>,
        #[arg(short, long)]
        prompt: String,
        /// Attempts per agent
        #[arg(short, default_value_t = 1)]
        n: usize,
        #[arg(long)]
        sandbox: Option<String>,
        #[arg(long)]
        permission: Option<String>,
        #[arg(long)]
        account: Option<String>,
    },
    /// List sessions
    Ls,
    /// Show a session's event log
    Log {
        id: String,
        #[arg(short, long)]
        follow: bool,
    },
    /// Send a prompt (resumes the session if it is not running)
    Send { id: String, text: Vec<String> },
    Stop { id: String },
    Resume { id: String },
    /// Show the session's diff across every repo in its workspace
    Diff { id: String },
    /// Comment on a line of the diff: `fleet51 annotate s-1234 src/app.rs:42 handle the error`
    Annotate { id: String, at: String, comment: Vec<String> },
    /// Send all unsent annotations to the agent as one review prompt
    Feedback { id: String },
    /// Commit the agent's work and merge its branch into the repo's branch
    Merge { id: String },
    /// Compare the attempts of a fan-out group
    Compare { group: String },
    /// Open the session's checkout (optionally file:line) in your own editor
    Open { id: String, at: Option<String> },
    /// Token and cost usage by agent and account
    Usage,
    #[command(subcommand)]
    Accounts(AccountsCmd),
    /// Called by agent hooks (installed per session, never globally)
    #[command(hide = true)]
    Hook {
        #[arg(long)]
        session: String,
        #[arg(long)]
        name: String,
    },
    /// Offline agent that speaks a real protocol (claude | codex | acp | pty)
    #[command(hide = true)]
    MockAgent {
        #[arg(long)]
        protocol: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        rest: Vec<String>,
    },
}

#[derive(Subcommand)]
enum WsCmd {
    /// Create a workspace from one or more repos/folders
    Create {
        name: String,
        repos: Vec<String>,
        /// Force a kind: git | jj | folder
        #[arg(long)]
        kind: Option<String>,
    },
    Ls,
    Rm { id: String },
    Diff { id: String },
}

#[derive(Subcommand)]
enum AccountsCmd {
    Ls,
    /// Add an account: an isolated config dir the agent logs in to once
    Add { provider: String, name: String },
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("fleet51: {e:#}");
        std::process::exit(1);
    }
}

fn repos(paths: &[String], kind: Option<&str>) -> Value {
    json!(paths.iter().map(|p| json!({"path": p, "kind": kind})).collect::<Vec<_>>())
}

fn real_main() -> Result<()> {
    let cli = Cli::parse();
    let home = Home::resolve();
    match cli.cmd {
        Cmd::Daemon { listen } => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(daemon::serve(home, &listen))?;
            // Agent pipes and SSE readers can keep the runtime busy; we are done.
            std::process::exit(0);
        }
        Cmd::MockAgent { protocol, rest } => mock::run(&protocol, rest),
        Cmd::Up { listen } => up(&home, &listen),
        Cmd::Hook { session, name } => {
            let mut input = String::new();
            let _ = std::io::stdin().read_to_string(&mut input);
            let payload = serde_json::from_str::<Value>(&input).unwrap_or(Value::Null);
            // A hook must never break the agent, so failures are silent.
            if let Ok(c) = Client::new(&home) {
                let _ = c.post("/api/hook", json!({"session": session, "name": name, "payload": payload}));
            }
            Ok(())
        }
        cmd => client_cmd(&home, cmd),
    }
}

fn up(home: &Home, listen: &str) -> Result<()> {
    home.ensure()?;
    if let Some(info) = daemon::read_info(home) {
        if Client::new(home).and_then(|c| c.get("/api/health")).is_ok() {
            println!("Fleet-51 already running (generation {}) at {}", info.generation, info.url);
            return Ok(());
        }
    }
    let log = std::fs::File::create(home.root.join("daemon.log"))?;
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.args(["daemon", "--listen", listen]).stdin(std::process::Stdio::null()).stdout(log.try_clone()?).stderr(log);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn().context("starting daemon")?;
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(100));
        if let Ok(h) = Client::new(home).and_then(|c| c.get("/api/health")) {
            println!("Fleet-51 daemon generation {} running at {}", h["generation"], h["url"].as_str().unwrap_or(""));
            return Ok(());
        }
    }
    bail!("daemon did not come up; see {}", home.root.join("daemon.log").display())
}

fn client_cmd(home: &Home, cmd: Cmd) -> Result<()> {
    let c = Client::new(home)?;
    match cmd {
        Cmd::Down => {
            c.post("/api/shutdown", json!({}))?;
            println!("Fleet-51 daemon stopping; running sessions were suspended and resume on next start");
        }
        Cmd::Status => {
            let h = c.get("/api/health")?;
            let s = c.get("/api/sessions")?;
            let live = s.as_array().map(|a| a.iter().filter(|x| matches!(x["status"].as_str(), Some("idle" | "working" | "starting"))).count()).unwrap_or(0);
            println!("Fleet-51 {} · generation {} · pid {} · {}", h["version"].as_str().unwrap_or(""), h["generation"], h["pid"], h["url"].as_str().unwrap_or(""));
            println!("{} session(s), {live} live", s.as_array().map(|a| a.len()).unwrap_or(0));
        }
        Cmd::Url => println!("{}/?token={}", c.base, c.token),
        Cmd::Agents => {
            for a in c.get("/api/agents")?.as_array().into_iter().flatten() {
                let mark = if a["available"].as_bool() == Some(true) { "✓" } else { "·" };
                let hint = if a["available"].as_bool() == Some(true) { String::new() } else { a["install_hint"].as_str().map(|h| format!("  ({h})")).unwrap_or_default() };
                println!("{mark} {:<12} {:<20} {}{hint}", a["name"].as_str().unwrap_or(""), a["protocol"].as_str().unwrap_or(""), a["display"].as_str().unwrap_or(""));
            }
        }
        Cmd::Ws(w) => match w {
            WsCmd::Create { name, repos: r, kind } => {
                let ws = c.post("/api/workspaces", json!({"name": name, "repos": repos(&r, kind.as_deref())}))?;
                print_ws(&ws);
            }
            WsCmd::Ls => {
                for ws in c.get("/api/workspaces")?.as_array().into_iter().flatten() {
                    print_ws(ws);
                }
            }
            WsCmd::Rm { id } => println!("removed {}", c.delete(&format!("/api/workspaces/{id}"))?["removed"]),
            WsCmd::Diff { id } => {
                let d = c.get(&format!("/api/workspaces/{id}/diff"))?;
                print_diff(d.as_array().cloned().unwrap_or_default());
            }
        },
        Cmd::Run { agent, repos: r, ws, prompt, sandbox, permission, account, model, title, wait } => {
            let r = if r.is_empty() && ws.is_none() { vec![".".to_string()] } else { r };
            let st = c.post(
                "/api/sessions",
                json!({"agent": agent, "workspace": ws, "repos": repos(&r, None), "prompt": prompt, "sandbox": sandbox,
                       "permission": permission, "account": account, "model": model, "title": title}),
            )?;
            let id = st["id"].as_str().unwrap_or_default().to_string();
            println!("session {id} · {} in {}", st["agent"].as_str().unwrap_or(""), st["cwd"].as_str().unwrap_or(""));
            if wait {
                follow(&c, &id, true)?;
            }
        }
        Cmd::Fanout { agents, repos: r, prompt, n, sandbox, permission, account } => {
            let r = if r.is_empty() { vec![".".to_string()] } else { r };
            let out = c.post(
                "/api/fanout",
                json!({"agents": agents, "repos": repos(&r, None), "prompt": prompt, "n": n, "sandbox": sandbox, "permission": permission, "account": account}),
            )?;
            println!("group {}", out["group"].as_str().unwrap_or(""));
            for s in out["sessions"].as_array().into_iter().flatten() {
                println!("  {}  {}", s["id"].as_str().unwrap_or(""), s["title"].as_str().unwrap_or(""));
            }
            println!("compare with: fleet51 compare {}", out["group"].as_str().unwrap_or(""));
        }
        Cmd::Ls => {
            let list = c.get("/api/sessions")?;
            println!("{:<11} {:<12} {:<12} {:>5} {:>9}  TITLE", "ID", "AGENT", "STATUS", "TURNS", "COST");
            for s in list.as_array().into_iter().flatten() {
                println!(
                    "{:<11} {:<12} {:<12} {:>5} {:>9}  {}",
                    s["id"].as_str().unwrap_or(""),
                    s["agent"].as_str().unwrap_or(""),
                    s["status"].as_str().unwrap_or(""),
                    s["turns"].to_string(),
                    format!("${:.4}", s["cost_usd"].as_f64().unwrap_or(0.0)),
                    s["title"].as_str().unwrap_or("")
                );
            }
        }
        Cmd::Log { id, follow: f } => c.stream(&format!("/api/sessions/{id}/events?follow={f}"), |e| {
            if let Some(line) = fmt_event(&e) {
                println!("{line}");
            }
            true
        })?,
        Cmd::Send { id, text } => {
            let r = c.post(&format!("/api/sessions/{id}/prompt"), json!({"text": text.join(" ")}))?;
            println!("queued {}", r["prompt"].as_str().unwrap_or(""));
        }
        Cmd::Stop { id } => println!("{}", c.post(&format!("/api/sessions/{id}/stop"), json!({}))?["status"]),
        Cmd::Resume { id } => println!("{}", c.post(&format!("/api/sessions/{id}/resume"), json!({}))?["status"]),
        Cmd::Diff { id } => {
            let d = c.get(&format!("/api/sessions/{id}/diff"))?;
            print_diff(d["checkouts"].as_array().cloned().unwrap_or_default());
        }
        Cmd::Annotate { id, at, comment } => {
            let (file, line) = at.rsplit_once(':').context("use file:line")?;
            let r = c.post(&format!("/api/sessions/{id}/annotations"), json!({"file": file, "line": line.parse::<u32>()?, "comment": comment.join(" ")}))?;
            println!("annotation {}", r["annotation"].as_str().unwrap_or(""));
        }
        Cmd::Feedback { id } => println!("sent as prompt {}", c.post(&format!("/api/sessions/{id}/feedback"), json!({}))?["prompt"].as_str().unwrap_or("")),
        Cmd::Merge { id } => {
            for r in c.post(&format!("/api/sessions/{id}/merge"), json!({}))?.as_array().into_iter().flatten() {
                println!("merged {} into {} ({}) as {}", r["branch"].as_str().unwrap_or(""), r["into"].as_str().unwrap_or(""), r["checkout"].as_str().unwrap_or(""), &r["commit"].as_str().unwrap_or("")[..10.min(r["commit"].as_str().unwrap_or("").len())]);
            }
        }
        Cmd::Compare { group } => {
            let g = c.get(&format!("/api/groups/{group}"))?;
            println!("{:<11} {:<12} {:<9} {:>5} {:>6} {:>6} {:>9}  LAST MESSAGE", "ID", "AGENT", "STATUS", "FILES", "+", "-", "COST");
            for a in g["attempts"].as_array().into_iter().flatten() {
                let s = &a["session"];
                println!(
                    "{:<11} {:<12} {:<9} {:>5} {:>6} {:>6} {:>9}  {}",
                    s["id"].as_str().unwrap_or(""),
                    s["agent"].as_str().unwrap_or(""),
                    s["status"].as_str().unwrap_or(""),
                    a["files_changed"].to_string(),
                    a["added"].to_string(),
                    a["deleted"].to_string(),
                    format!("${:.4}", s["cost_usd"].as_f64().unwrap_or(0.0)),
                    adapters::truncate(s["last_text"].as_str().unwrap_or("").lines().next().unwrap_or(""), 60)
                );
            }
        }
        Cmd::Open { id, at } => open(&c, &id, at.as_deref())?,
        Cmd::Usage => {
            let u = c.get("/api/usage")?;
            let row = |name: &str, v: &Value| {
                println!(
                    "{:<20} {:>4} sessions {:>5} turns {:>9} in {:>8} out  ${:.4}",
                    name, v["sessions"], v["turns"], v["input_tokens"], v["output_tokens"], v["cost_usd"].as_f64().unwrap_or(0.0)
                )
            };
            row("total", &u["total"]);
            for (k, v) in u["by_agent"].as_object().into_iter().flatten() {
                row(&format!("agent:{k}"), v);
            }
            for (k, v) in u["by_account"].as_object().into_iter().flatten() {
                row(&format!("account:{k}"), v);
            }
        }
        Cmd::Accounts(a) => match a {
            AccountsCmd::Ls => {
                for a in c.get("/api/accounts")?.as_array().into_iter().flatten() {
                    let cd = if a["cooldown_until"].is_null() { "ready".to_string() } else { "cooling down".to_string() };
                    println!("{}/{}  {}  {}", a["provider"].as_str().unwrap_or(""), a["name"].as_str().unwrap_or(""), cd, a["dir"].as_str().unwrap_or(""));
                }
            }
            AccountsCmd::Add { provider, name } => {
                let r = c.post("/api/accounts", json!({"provider": provider, "name": name}))?;
                println!("account {provider}/{name} → {}", r["dir"].as_str().unwrap_or(""));
                println!("log in once with that directory as the agent's config dir, e.g. CLAUDE_CONFIG_DIR={} claude", r["dir"].as_str().unwrap_or(""));
            }
        },
        Cmd::Daemon { .. } | Cmd::Up { .. } | Cmd::Hook { .. } | Cmd::MockAgent { .. } => unreachable!(),
    }
    Ok(())
}

fn print_ws(ws: &Value) {
    println!("{}  {}", ws["id"].as_str().unwrap_or(""), ws["name"].as_str().unwrap_or(""));
    for c in ws["checkouts"].as_array().into_iter().flatten() {
        let sub = c["subdir"].as_str().map(|s| format!(" (cwd {s})")).unwrap_or_default();
        println!("  {:<6} {:<16} {}{sub}", c["kind"].as_str().unwrap_or(""), c["name"].as_str().unwrap_or(""), c["path"].as_str().unwrap_or(""));
    }
}

fn print_diff(checkouts: Vec<Value>) {
    for co in checkouts {
        println!("━━ {} ({}) {}", co["checkout"].as_str().unwrap_or(""), co["kind"].as_str().unwrap_or(""), co["path"].as_str().unwrap_or(""));
        if let Some(e) = co["error"].as_str() {
            println!("  error: {e}");
        }
        for f in co["files"].as_array().into_iter().flatten() {
            println!("  {:<40} +{} -{}", f["path"].as_str().unwrap_or(""), f["added"], f["deleted"]);
        }
        print!("{}", co["patch"].as_str().unwrap_or(""));
        println!();
    }
}

fn fmt_event(e: &Value) -> Option<String> {
    let s = |k: &str| e[k].as_str().unwrap_or("").to_string();
    Some(match e["type"].as_str()? {
        "created" => format!("● {} session in {} (sandbox {}, permission {})", s("agent"), s("cwd"), s("sandbox"), s("permission")),
        "spawned" => format!(
            "▶ started via {}{}{}{}",
            s("protocol"),
            e["pid"].as_u64().map(|p| format!(" pid {p}")).unwrap_or_default(),
            e["account"].as_str().map(|a| format!(" account {a}")).unwrap_or_default(),
            e["resume"].as_str().map(|r| format!(" resuming {r}")).unwrap_or_default()
        ),
        "ready" => "· ready".into(),
        "prompt_queued" => format!("› {}", s("text")),
        "prompt_delivered" => return None,
        "prompt_acked" => "  ✓ prompt acknowledged by agent".into(),
        "provider_session" => format!("  agent session {}", s("id")),
        "text" => s("text"),
        "tool_call" => format!("  ⚙ {} {}", s("name"), adapters::truncate(&e["input"].to_string(), 100)),
        "permission" => format!("  ⚖ {} → {}", s("tool"), s("decision")),
        "output" => {
            let t = adapters::pty::strip_ansi(&s("data"));
            if t.trim().is_empty() {
                return None;
            }
            t.trim_end().to_string()
        }
        "turn_done" => format!(
            "■ turn {} · {} in / {} out tokens · ${:.4}",
            if e["ok"].as_bool() == Some(true) { "done" } else { "failed" },
            e["input_tokens"],
            e["output_tokens"],
            e["cost_usd"].as_f64().unwrap_or(0.0)
        ),
        "rate_limited" => format!("⚠ rate limited ({}): {}", e["account"].as_str().unwrap_or("default"), s("message")),
        "account_switched" => format!("⇄ switched account {} → {}", e["from"].as_str().unwrap_or("default"), s("to")),
        "hook" => format!("  ↪ hook {}", s("name")),
        "annotation" => format!("✎ {}:{} {}", s("file"), e["line"], s("comment")),
        "feedback_sent" => format!("✎ sent {} annotation(s) as feedback", e["annotations"].as_array().map(|a| a.len()).unwrap_or(0)),
        "stop_requested" => "■ stop requested".into(),
        "suspended" => format!("⏸ suspended for daemon handoff (generation {})", e["generation"]),
        "orphaned" => format!("⚠ daemon restarted mid-session (now generation {})", e["generation"]),
        "exited" => format!("■ exited code {} ({})", e["code"], if e["clean"].as_bool() == Some(true) { "clean" } else { "unclean" }),
        "merged" => format!("⇢ merged {} into {}", s("branch"), s("into")),
        "note" => format!("  [{}] {}", s("level"), s("message")),
        _ => return None,
    })
}

/// Print history; with `until_turn`, keep following until a turn finishes or the agent exits.
fn follow(c: &Client, id: &str, until_turn: bool) -> Result<()> {
    let path = format!("/api/sessions/{id}/events?follow={until_turn}");
    c.stream(&path, |e| {
        if let Some(line) = fmt_event(&e) {
            println!("{line}");
        }
        match e["type"].as_str() {
            Some("turn_done" | "exited") => !until_turn,
            _ => true,
        }
    })
}

fn open(c: &Client, id: &str, at: Option<&str>) -> Result<()> {
    let st = c.get(&format!("/api/sessions/{id}"))?;
    let cwd = std::path::PathBuf::from(st["cwd"].as_str().unwrap_or("."));
    let (file, line) = match at {
        Some(a) => match a.rsplit_once(':') {
            Some((f, l)) if l.parse::<u32>().is_ok() => (Some(f.to_string()), l.parse::<u32>().ok()),
            _ => (Some(a.to_string()), None),
        },
        None => (None, None),
    };
    let target = file.as_ref().map(|f| cwd.join(f)).unwrap_or_else(|| cwd.clone());
    let local = c.base.contains("127.0.0.1") || c.base.contains("localhost");
    if !local {
        let host = c.base.split("//").nth(1).and_then(|h| h.split(':').next()).unwrap_or("host");
        println!("vscode://vscode-remote/ssh-remote+{host}{}{}", target.display(), line.map(|l| format!(":{l}")).unwrap_or_default());
        return Ok(());
    }
    let editor = ["FLEET51_EDITOR", "VISUAL", "EDITOR"]
        .iter()
        .find_map(|k| std::env::var(k).ok())
        .or_else(|| ["code", "cursor", "zed"].iter().find(|e| manifest::which(e)).map(|e| e.to_string()));
    let Some(editor) = editor else {
        println!("{}", target.display());
        return Ok(());
    };
    let name = std::path::Path::new(&editor).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let mut cmd = std::process::Command::new(&editor);
    match (name.as_str(), line) {
        ("code" | "cursor" | "codium", Some(l)) => cmd.arg("-g").arg(format!("{}:{l}", target.display())),
        ("zed", Some(l)) => cmd.arg(format!("{}:{l}", target.display())),
        (_, Some(l)) => cmd.arg(format!("+{l}")).arg(&target),
        (_, None) => cmd.arg(&target),
    };
    cmd.status().with_context(|| format!("launching {editor}"))?;
    Ok(())
}
