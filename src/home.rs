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
            .unwrap_or_else(|| {
                let home = std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| ".".into());
                home.join(".fleet51")
            });
        Home { root }
    }

    pub fn ensure(&self) -> anyhow::Result<()> {
        for d in ["sessions", "workspaces", "agents", "accounts"] {
            std::fs::create_dir_all(self.root.join(d))?;
        }
        Ok(())
    }

    pub fn sessions(&self) -> PathBuf {
        self.root.join("sessions")
    }
    pub fn workspaces(&self) -> PathBuf {
        self.root.join("workspaces")
    }
    pub fn agents(&self) -> PathBuf {
        self.root.join("agents")
    }
    pub fn accounts(&self) -> PathBuf {
        self.root.join("accounts")
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

pub fn write_private(path: &Path, contents: &str) -> anyhow::Result<()> {
    std::fs::write(path, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
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

/// Linux-only liveness check; elsewhere we assume alive and let the handoff request decide.
pub fn pid_alive(pid: u32) -> bool {
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
        true
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
    let _ = std::process::Command::new("kill")
        .arg(pid.to_string())
        .status();
}
