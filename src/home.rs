use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Everything Fleet-51 stores lives under one directory. Agent configs in $HOME are never touched.
#[derive(Clone, Debug)]
pub struct Home {
    pub root: PathBuf,
}

impl Home {
    pub fn resolve() -> Home {
        let root = std::env::var_os("FLEET51_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| user_home().join(".fleet51"));
        Home { root }
    }

    pub fn ensure(&self) -> anyhow::Result<()> {
        for d in ["sessions", "runtime"] {
            std::fs::create_dir_all(self.root.join(d))?;
        }
        // Chats, automations and undo copies are readable only by you.
        private_dir(&self.root);
        Ok(())
    }

    pub fn sessions(&self) -> PathBuf {
        self.root.join("sessions")
    }
    pub fn daemon_file(&self) -> PathBuf {
        self.root.join("daemon.json")
    }
    pub fn token_file(&self) -> PathBuf {
        self.root.join("token")
    }

    pub fn token(&self) -> anyhow::Result<String> {
        let path = self.token_file();
        if let Ok(t) = std::fs::read_to_string(&path) {
            if !t.trim().is_empty() {
                return Ok(t.trim().to_string());
            }
        }
        let t = uuid::Uuid::new_v4().simple().to_string();
        write_private(&path, &t)?;
        Ok(t)
    }
}

/// Make a folder readable and writable only by its owner (no-op where that does not apply).
pub fn private_dir(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
    }
    #[cfg(not(unix))]
    let _ = path;
}

pub fn write_private(path: &Path, contents: &str) -> anyhow::Result<()> {
    use std::io::Write;
    let mut f = std::fs::File::create(path)?;
    f.write_all(contents.as_bytes())?;
    // On disk now, not in a few seconds: a computer (or VM) switched off right after keeps it.
    f.sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        if let Some(dir) = path.parent().and_then(|d| std::fs::File::open(d).ok()) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn short_id(prefix: &str) -> String {
    let u = uuid::Uuid::new_v4().simple().to_string();
    format!("{prefix}-{}", &u[..8])
}

/// The user's home folder (HOME, or USERPROFILE on Windows).
pub fn user_home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| ".".into())
}

/// On Windows, run a helper program without flashing a console window.
pub fn no_window(cmd: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Is `pid` a running (not zombie) process?
pub fn pid_alive(pid: u32) -> bool {
    if cfg!(windows) {
        let mut c = std::process::Command::new("tasklist");
        c.args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"]);
        no_window(&mut c);
        return c
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains(&format!("\"{pid}\"")));
    }
    if cfg!(target_os = "linux") {
        // A zombie still has a /proc entry but is not running.
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => !stat
                .rsplit(')')
                .next()
                .is_some_and(|rest| rest.trim_start().starts_with('Z')),
            Err(_) => false,
        }
    } else {
        // macOS and other Unixes: signal 0 checks existence without touching the process.
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }
}

/// True only if `pid` is a process Fleet-51 launched for `session` (guards against pid reuse).
pub fn pid_belongs_to_session(pid: u32, session: &str) -> bool {
    let Ok(env) = std::fs::read(format!("/proc/{pid}/environ")) else {
        return false;
    };
    let needle = format!("FLEET51_SESSION={session}");
    env.split(|b| *b == 0).any(|kv| kv == needle.as_bytes())
}

pub fn kill_pid(pid: u32) {
    let mut c = if cfg!(windows) {
        let mut c = std::process::Command::new("taskkill");
        c.args(["/PID", &pid.to_string(), "/T", "/F"]);
        c
    } else {
        let mut c = std::process::Command::new("kill");
        c.arg(pid.to_string());
        c
    };
    no_window(&mut c);
    let _ = c
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}
