//! Running a bot's code safely. Commands run with the bot's folder as the only writable place, no
//! network, the rest of your home folder hidden, a clean environment and a time limit.
//! macOS uses its built-in sandbox (sandbox-exec); Linux uses bubblewrap (bwrap) when installed.
//! Where neither works, bots are not offered the tool at all.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

pub const TIMEOUT: Duration = Duration::from_secs(60);
const MAX_OUTPUT: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    MacOs,
    Bwrap,
}

/// Which sandbox this computer has, checked once by actually running one.
pub fn available() -> Option<Kind> {
    static FOUND: std::sync::OnceLock<Option<Kind>> = std::sync::OnceLock::new();
    *FOUND.get_or_init(detect)
}

fn detect() -> Option<Kind> {
    if std::env::var_os("FLEET51_NO_SANDBOX").is_some() {
        return None;
    }
    let quiet = |mut c: Command| {
        c.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    if cfg!(target_os = "macos") {
        let mut c = Command::new("/usr/bin/sandbox-exec");
        c.args([
            "-p",
            "(version 1)(allow default)(deny network*)",
            "/usr/bin/true",
        ]);
        return quiet(c).then_some(Kind::MacOs);
    }
    let mut c = Command::new("bwrap");
    c.args([
        "--ro-bind",
        "/",
        "/",
        "--dev",
        "/dev",
        "--unshare-all",
        "--die-with-parent",
        "true",
    ]);
    quiet(c).then_some(Kind::Bwrap)
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .and_then(|h| h.canonicalize().ok())
        .unwrap_or_else(|| PathBuf::from("/nonexistent"))
}

/// Quote a path for the macOS sandbox profile language.
fn sb_str(p: &Path) -> String {
    format!(
        "\"{}\"",
        p.display()
            .to_string()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    )
}

fn macos_profile(folder: &Path, extra_read: Option<&Path>) -> String {
    let (f, h) = (sb_str(folder), sb_str(&home()));
    let extra = extra_read
        .map(|p| format!("(allow file-read-data (literal {}))\n", sb_str(p)))
        .unwrap_or_default();
    // Later rules win: deny broadly, then allow the bot's folder back. Beyond files and network,
    // code must not reach other apps: no Apple Events (scripting Finder etc.), no LaunchServices
    // (`open` would start apps outside the sandbox), no clipboard, keychain or window server,
    // and no signals to processes outside the sandbox.
    format!(
        "(version 1)
(allow default)
(deny network*)
(deny file-write*)
(allow file-write* (subpath {f}) (literal \"/dev/null\") (literal \"/dev/zero\") (regex #\"^/dev/fd/\") (regex #\"^/dev/tty\"))
(deny file-read-data (subpath {h}))
(allow file-read-data (subpath {f}))
{extra}(deny appleevent-send)
(deny mach-lookup
  (global-name \"com.apple.coreservices.launchservicesd\")
  (global-name \"com.apple.lsd.mapdb\")
  (global-name \"com.apple.lsd.modifydb\")
  (global-name \"com.apple.pasteboard.1\")
  (global-name \"com.apple.SecurityServer\")
  (global-name \"com.apple.securityd.xpc\")
  (global-name \"com.apple.windowserver.active\")
  (global-name \"com.apple.coreservices.appleevents\"))
(deny process-exec (literal \"/usr/bin/open\") (literal \"/usr/bin/osascript\") (literal \"/usr/bin/pbcopy\") (literal \"/usr/bin/pbpaste\"))
(deny signal)
(allow signal (target same-sandbox))
(deny nvram*)
"
    )
}

/// Run `command` with `sh -c` inside the sandbox, in `folder`. Returns the exit code and output.
pub fn run(kind: Kind, folder: &Path, command: &str) -> Result<(Option<i32>, String)> {
    run_program(
        Some(kind),
        folder,
        Path::new("/bin/sh"),
        &["-c", command],
        None,
    )
}

/// Run a program inside the sandbox (or, with no sandbox, as a plain child process with the same
/// clean environment and time limit). `extra_read` is one more file it may read, such as itself.
pub fn run_program(
    kind: Option<Kind>,
    folder: &Path,
    program: &Path,
    args: &[&str],
    extra_read: Option<&Path>,
) -> Result<(Option<i32>, String)> {
    let folder = folder.canonicalize()?;
    let tmp = folder.join(".tmp");
    std::fs::create_dir_all(&tmp)?;
    let mut cmd = match kind {
        None => {
            let mut c = Command::new(program);
            c.args(args);
            c
        }
        Some(Kind::MacOs) => {
            let mut c = Command::new("/usr/bin/sandbox-exec");
            c.arg("-p").arg(macos_profile(&folder, extra_read));
            c.arg(program).args(args);
            c
        }
        Some(Kind::Bwrap) => {
            let mut c = Command::new("bwrap");
            c.args(["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc"]);
            c.args(["--tmpfs", "/tmp"]);
            // /run holds the D-Bus session and display sockets: through them a program could ask
            // the desktop to start things outside the sandbox.
            c.args(["--tmpfs", "/run"]);
            // Hide the home folder (and Fleet-51's own data), then put the bot's folder back.
            let h = home();
            if h.is_dir() {
                c.arg("--tmpfs").arg(&h);
            }
            let data = crate::home::Home::resolve().root;
            if data.is_dir() && !folder.starts_with(&data) {
                c.arg("--tmpfs").arg(&data);
            }
            c.arg("--bind").arg(&folder).arg(&folder);
            if let Some(p) = extra_read {
                c.arg("--ro-bind").arg(p).arg(p);
            }
            c.args(["--unshare-all", "--die-with-parent", "--new-session"]);
            c.arg("--chdir").arg(&folder);
            c.arg(program).args(args);
            c
        }
    };
    cmd.current_dir(&folder)
        .env_clear()
        .env(
            "PATH",
            "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin",
        )
        .env("HOME", &folder)
        .env("TMPDIR", &tmp)
        .env("LANG", "en_US.UTF-8")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().context("starting the sandbox")?;
    // Output is collected as it arrives, so a program left running in the background that keeps
    // the pipe open can't make Fleet-51 wait for it.
    let read = |r: Option<Box<dyn Read + Send>>| {
        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let b = buf.clone();
        std::thread::spawn(move || {
            if let Some(mut r) = r {
                let mut chunk = [0u8; 8192];
                while let Ok(n) = r.read(&mut chunk) {
                    if n == 0 {
                        break;
                    }
                    let mut b = b.lock().unwrap();
                    if b.len() < MAX_OUTPUT * 2 {
                        b.extend_from_slice(&chunk[..n]);
                    }
                }
            }
        });
        buf
    };
    let take = |b: &std::sync::Arc<std::sync::Mutex<Vec<u8>>>| b.lock().unwrap().clone();
    let out = read(child.stdout.take().map(|r| Box::new(r) as _));
    let err = read(child.stderr.take().map(|r| Box::new(r) as _));
    let started = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break Some(s);
        }
        if started.elapsed() > TIMEOUT {
            #[cfg(unix)]
            {
                let _ = Command::new("kill")
                    .args(["-KILL", &format!("-{}", child.id())])
                    .status();
            }
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    // Stop anything the command left running in the background, then give the readers a moment.
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-KILL", &format!("-{}", child.id())])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    std::thread::sleep(Duration::from_millis(100));
    let mut text = String::from_utf8_lossy(&take(&out)).to_string();
    let err = String::from_utf8_lossy(&take(&err)).to_string();
    if !err.trim().is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&err);
    }
    if text.len() > MAX_OUTPUT {
        let mut cut = MAX_OUTPUT;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n… [output truncated]");
    }
    match status {
        None => bail!(
            "stopped after {} seconds (time limit). Output so far:\n{text}",
            TIMEOUT.as_secs()
        ),
        Some(s) => Ok((s.code(), text)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_can_write_only_inside_the_folder() {
        let Some(kind) = available() else {
            eprintln!("no sandbox on this computer; skipping");
            return;
        };
        let base = std::env::temp_dir().join(format!("fleet51-sbx-{}", std::process::id()));
        let folder = base.join("bot");
        std::fs::create_dir_all(&folder).unwrap();
        let (code, out) = run(kind, &folder, "echo hello > a.txt && cat a.txt").unwrap();
        assert_eq!(code, Some(0), "{out}");
        assert!(out.contains("hello"));
        // Refused (macOS) or written to a throwaway view of the disk (bubblewrap); never real.
        let _ = run(kind, &folder, "echo nope > ../outside.txt").unwrap();
        assert!(!base.join("outside.txt").exists());
        // A program left running in the background is stopped, and doesn't hold things up.
        let started = Instant::now();
        let (_, out) = run(
            kind,
            &folder,
            "(sleep 30; echo late > late.txt) & echo started",
        )
        .unwrap();
        assert!(out.contains("started"), "{out}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "waited for the background program"
        );
        std::thread::sleep(Duration::from_secs(1));
        // Code can't reach other apps or the desktop session.
        if kind == Kind::MacOs {
            for cmd in [
                "osascript -e 'tell application \"Finder\" to get name'",
                "open -g -a TextEdit",
                "pbpaste",
            ] {
                let (code, out) = run(kind, &folder, cmd).unwrap();
                assert_ne!(code, Some(0), "{cmd} worked in the sandbox: {out}");
            }
        } else {
            let (_, out) = run(kind, &folder, "ls -A /run | wc -l").unwrap();
            assert_eq!(out.trim(), "0", "/run is visible in the sandbox");
        }
        let _ = std::fs::remove_dir_all(&base);
    }
}
