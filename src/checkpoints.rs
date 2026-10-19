//! Undo for bot file changes. Before a bot's first change in a reply (writing or editing a file,
//! or running code), its folder is copied aside; "Undo" puts the folder back exactly as it was.

use std::path::Path;

use anyhow::{bail, Context, Result};

const MAX_FILES: usize = 5000;
const MAX_BYTES: u64 = 200 * 1024 * 1024;
/// Checkpoints kept per bot; older ones are removed.
pub const KEEP: usize = 30;

fn files(
    root: &Path,
    dir: &Path,
    out: &mut Vec<std::path::PathBuf>,
    bytes: &mut u64,
) -> Result<()> {
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let p = e.path();
        let ft = e.file_type()?;
        if p.strip_prefix(root).is_ok_and(|r| r == Path::new(".tmp")) {
            continue;
        }
        // Links are never followed, so a checkpoint can only ever contain the folder itself.
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            files(root, &p, out, bytes)?;
        } else if ft.is_file() {
            *bytes += e.metadata()?.len();
            out.push(p);
        }
        if out.len() > MAX_FILES || *bytes > MAX_BYTES {
            bail!("the folder is too big to keep an undo copy (over {MAX_FILES} files or 200 MB)");
        }
    }
    Ok(())
}

/// Copy `folder` into `dest` (which must not exist yet).
pub fn snapshot(folder: &Path, dest: &Path) -> Result<()> {
    let mut list = Vec::new();
    let mut bytes = 0;
    files(folder, folder, &mut list, &mut bytes)?;
    std::fs::create_dir_all(dest)?;
    for f in list {
        let rel = f.strip_prefix(folder)?;
        let to = dest.join(rel);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&f, &to).with_context(|| format!("copying {}", rel.display()))?;
    }
    Ok(())
}

/// Make `folder` exactly what `checkpoint` holds: changed files come back, new ones go away.
pub fn restore(folder: &Path, checkpoint: &Path) -> Result<()> {
    if !checkpoint.is_dir() {
        bail!("that undo point is no longer available");
    }
    for e in std::fs::read_dir(folder)? {
        let e = e?;
        if e.file_name() == ".tmp" {
            continue;
        }
        let p = e.path();
        // remove_dir_all does not follow links; a link is removed, not what it points to.
        if e.file_type()?.is_dir() {
            std::fs::remove_dir_all(&p)?;
        } else {
            std::fs::remove_file(&p)?;
        }
    }
    let mut list = Vec::new();
    let mut bytes = 0;
    files(checkpoint, checkpoint, &mut list, &mut bytes)?;
    for f in list {
        let rel = f.strip_prefix(checkpoint)?;
        let to = folder.join(rel);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&f, &to)?;
    }
    Ok(())
}

/// Remove all but the newest KEEP checkpoints in `dir` (names sort by time).
pub fn prune(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut names: Vec<_> = rd.flatten().map(|e| e.path()).collect();
    names.sort();
    let extra = names.len().saturating_sub(KEEP);
    for p in names.into_iter().take(extra) {
        let _ = std::fs::remove_dir_all(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_brings_back_changed_and_removes_new_files() {
        let base = std::env::temp_dir().join(format!("fleet51-ckpt-{}", std::process::id()));
        let folder = base.join("bot");
        std::fs::create_dir_all(folder.join("src")).unwrap();
        std::fs::write(folder.join("src/a.py"), "print(1)").unwrap();
        snapshot(&folder, &base.join("c1")).unwrap();
        std::fs::write(folder.join("src/a.py"), "broken(").unwrap();
        std::fs::write(folder.join("new.txt"), "x").unwrap();
        restore(&folder, &base.join("c1")).unwrap();
        assert_eq!(
            std::fs::read_to_string(folder.join("src/a.py")).unwrap(),
            "print(1)"
        );
        assert!(!folder.join("new.txt").exists());
        let _ = std::fs::remove_dir_all(&base);
    }
}
