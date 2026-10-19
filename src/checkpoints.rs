//! Undo for bot file changes. Before a bot's first change in a reply (writing or editing a file,
//! or running code), its folder is copied aside; "Undo" puts the folder back exactly as it was.

use std::path::{Path, PathBuf};

use cap_std::fs::Dir;

use anyhow::{bail, Context, Result};

const MAX_FILES: usize = 5000;
const MAX_BYTES: u64 = 200 * 1024 * 1024;
/// Checkpoints kept per bot; older ones are removed.
pub const KEEP: usize = 30;

/// The files in `root` (relative paths), walked through a confined handle: links are never
/// followed, and a link swapped in mid-walk can't lead outside (see agent::folder).
fn files(dir: &Dir, rel: &Path, out: &mut Vec<PathBuf>, bytes: &mut u64) -> Result<()> {
    for e in dir.entries()? {
        let e = e?;
        let name = PathBuf::from(e.file_name());
        let ft = e.file_type()?;
        if rel.as_os_str().is_empty() && name == Path::new(".tmp") {
            continue;
        }
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            files(&e.open_dir()?, &rel.join(&name), out, bytes)?;
        } else if ft.is_file() {
            *bytes += e.metadata()?.len();
            out.push(rel.join(&name));
        }
        if out.len() > MAX_FILES || *bytes > MAX_BYTES {
            bail!("the folder is too big to keep an undo copy (over {MAX_FILES} files or 200 MB)");
        }
    }
    Ok(())
}

fn open(path: &Path) -> Result<Dir> {
    Ok(Dir::open_ambient_dir(path, cap_std::ambient_authority())?)
}

/// Copy the bot's `folder` into `dest`, Fleet-51's own copy (which must not exist yet).
pub fn snapshot(folder: &Path, dest: &Path) -> Result<()> {
    let src = open(folder)?;
    let mut list = Vec::new();
    let mut bytes = 0;
    files(&src, Path::new(""), &mut list, &mut bytes)?;
    std::fs::create_dir_all(dest)?;
    let out = open(dest)?;
    for rel in list {
        if let Some(parent) = rel.parent().filter(|p| !p.as_os_str().is_empty()) {
            out.create_dir_all(parent)?;
        }
        let data = src
            .read(&rel)
            .with_context(|| format!("copying {}", rel.display()))?;
        out.write(&rel, data)?;
    }
    Ok(())
}

/// Make the bot's `folder` exactly what `checkpoint` holds: changed files come back, new ones go
/// away. Every change goes through the confined handle, so nothing outside the folder is touched
/// even if something inside it is swapped for a link while this runs.
pub fn restore(folder: &Path, checkpoint: &Path) -> Result<()> {
    if !checkpoint.is_dir() {
        bail!("that undo point is no longer available");
    }
    let dir = open(folder)?;
    for e in dir.entries()? {
        let e = e?;
        let name = e.file_name();
        if name == ".tmp" {
            continue;
        }
        if e.file_type()?.is_dir() {
            dir.remove_dir_all(&name)?;
        } else {
            dir.remove_file(&name)?;
        }
    }
    let src = open(checkpoint)?;
    let mut list = Vec::new();
    let mut bytes = 0;
    files(&src, Path::new(""), &mut list, &mut bytes)?;
    for rel in list {
        if let Some(parent) = rel.parent().filter(|p| !p.as_os_str().is_empty()) {
            dir.create_dir_all(parent)?;
        }
        dir.write(&rel, src.read(&rel)?)?;
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
