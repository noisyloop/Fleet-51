mod agent;
mod automations;
mod checkpoints;
mod client;
mod daemon;
mod events;
mod files;
mod home;
mod models;
mod runtime;
mod sandbox;
mod server;
mod session;
mod zip_range;

use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use serde_json::json;

use client::Client;
use home::Home;

#[derive(Parser)]
#[command(
    name = "fleet51",
    version,
    about = "Fleet-51: a fleet of private AI bots on your own computer"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the background service in the foreground (takes over from an older one)
    Daemon {
        #[arg(long, default_value = "127.0.0.1:7451")]
        listen: String,
    },
    /// Start the background service if it isn't running (or replace an older version)
    Up {
        #[arg(long, default_value = "127.0.0.1:7451")]
        listen: String,
    },
    /// Stop the background service and all bots
    Down,
    /// Show whether the service and the model engine are running
    Status,
    /// Print the link to open Fleet-51 in a browser
    Url,
    /// Local models: list, download (`models pull qwen3:1.7b`), delete
    Models {
        #[command(subcommand)]
        cmd: Option<ModelsCmd>,
    },
    /// Save the text of a PDF or Word document (runs inside the sandbox)
    #[command(hide = true)]
    Extract {
        input: std::path::PathBuf,
        output: std::path::PathBuf,
    },
    /// The bot process the service runs for each chat
    #[command(hide = true)]
    Agent {
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        resume: Option<String>,
        #[arg(long)]
        state_dir: Option<std::path::PathBuf>,
        #[arg(long, default_value_t = 30)]
        max_steps: usize,
        #[arg(long)]
        think: bool,
        /// Offer the sandboxed run_command tool
        #[arg(long)]
        run_code: bool,
        #[arg(long, default_value = "auto")]
        tool_mode: String,
    },
}

#[derive(Subcommand)]
enum ModelsCmd {
    /// Download a model through Ollama, with progress
    Pull { name: String },
    /// Delete a local model
    Rm { name: String },
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("fleet51: {e:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let cli = Cli::parse();
    let home = Home::resolve();
    match cli.cmd {
        Cmd::Daemon { listen } => {
            tokio::runtime::Runtime::new()?.block_on(daemon::serve(home, &listen))?;
            // Agent pipes and open streams can keep the runtime busy; we are done.
            std::process::exit(0);
        }
        Cmd::Extract { input, output } => files::extract_command(&input, &output),
        Cmd::Agent {
            model,
            resume,
            state_dir,
            max_steps,
            think,
            run_code,
            tool_mode,
        } => agent::run(agent::Opts {
            model,
            resume,
            state_dir,
            max_steps,
            think,
            run_code,
            tool_mode,
        }),
        Cmd::Up { listen } => up(&home, &listen),
        cmd => client_cmd(&home, cmd),
    }
}

fn up(home: &Home, listen: &str) -> Result<()> {
    home.ensure()?;
    if let Ok(h) = Client::new(home).and_then(|c| c.get("/api/health")) {
        if h["build"].as_str() == Some(daemon::build_id().as_str()) {
            println!(
                "Fleet-51 already running at {}",
                h["url"].as_str().unwrap_or("")
            );
            return Ok(());
        }
        // An older (or newer) build is running: start ours, which takes over from it.
        println!(
            "Replacing Fleet-51 {} with {}",
            h["version"].as_str().unwrap_or("(older)"),
            env!("CARGO_PKG_VERSION")
        );
    }
    let log = std::fs::File::create(home.root.join("daemon.log"))?;
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.args(["daemon", "--listen", listen])
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    // Windows: detached, in its own process group, with no console window.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0000_0008 | 0x0000_0200 | home::CREATE_NO_WINDOW);
    }
    cmd.spawn().context("starting the background service")?;
    for _ in 0..100 {
        std::thread::sleep(Duration::from_millis(100));
        if let Ok(h) = Client::new(home).and_then(|c| c.get("/api/health")) {
            if h["build"].as_str() == Some(daemon::build_id().as_str()) {
                println!("Fleet-51 running at {}", h["url"].as_str().unwrap_or(""));
                return Ok(());
            }
        }
    }
    bail!(
        "the background service did not start; see {}",
        home.root.join("daemon.log").display()
    )
}

fn client_cmd(home: &Home, cmd: Cmd) -> Result<()> {
    let c = Client::new(home)?;
    match cmd {
        Cmd::Down => {
            c.post("/api/shutdown", json!({}))?;
            println!("Fleet-51 stopped");
        }
        Cmd::Status => {
            let h = c.get("/api/health")?;
            let bots = c.get("/api/bots")?;
            println!(
                "Fleet-51 {} · {} · {} bot(s)",
                h["version"].as_str().unwrap_or(""),
                h["url"].as_str().unwrap_or(""),
                bots.as_array().map(|a| a.len()).unwrap_or(0)
            );
            let rt = &c.get("/api/runtime")?["runtime"];
            match rt["state"].as_str() {
                Some("ready") => println!(
                    "models: ready ({} Ollama at {})",
                    rt["source"].as_str().unwrap_or(""),
                    rt["url"].as_str().unwrap_or("")
                ),
                Some("failed") => {
                    println!("models: failed: {}", rt["message"].as_str().unwrap_or(""))
                }
                Some(other) => println!("models: {other}"),
                None => {}
            }
        }
        Cmd::Url => println!("{}/?token={}", c.base, c.token),
        Cmd::Models { cmd } => match cmd {
            None => {
                let m = c.get("/api/models")?;
                if let Some(e) = m["error"].as_str() {
                    bail!("{e}");
                }
                for x in m["installed"].as_array().into_iter().flatten() {
                    println!(
                        "{:<28} {:>6.1} GB  {}",
                        x["name"].as_str().unwrap_or(""),
                        x["size"].as_f64().unwrap_or(0.0) / 1e9,
                        x["parameter_size"].as_str().unwrap_or("")
                    );
                }
            }
            Some(ModelsCmd::Pull { name }) => {
                c.post("/api/models/pull", json!({"name": name}))?;
                let mut failed = None;
                c.stream("/api/stream", |v| {
                    if v["kind"] != "pull" || v["name"] != name.as_str() {
                        return true;
                    }
                    let (done, total) = (
                        v["completed"].as_u64().unwrap_or(0),
                        v["total"].as_u64().unwrap_or(0),
                    );
                    match (done * 100).checked_div(total) {
                        Some(pct) => eprint!(
                            "\r{name}: {} {pct:>3}%   ",
                            v["status"].as_str().unwrap_or("")
                        ),
                        None => eprint!("\r{name}: {}   ", v["status"].as_str().unwrap_or("")),
                    }
                    if v["done"].as_bool() == Some(true) {
                        failed = v["error"].as_str().map(str::to_string);
                        eprintln!();
                        return false;
                    }
                    true
                })?;
                match failed {
                    Some(e) => bail!("download of {name} failed: {e}"),
                    None => println!("{name} is ready"),
                }
            }
            Some(ModelsCmd::Rm { name }) => {
                c.post("/api/models/delete", json!({"name": name}))?;
                println!("deleted {name}");
            }
        },
        Cmd::Daemon { .. } | Cmd::Up { .. } | Cmd::Agent { .. } | Cmd::Extract { .. } => {
            unreachable!()
        }
    }
    Ok(())
}
