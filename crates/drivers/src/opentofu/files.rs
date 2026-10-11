//! Writing generated files inside a run directory: relative paths only, no
//! traversal, no writing through symlinks, and owner-only permissions.

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Component, Path, PathBuf};

/// Creates `dir` and its parents, owner-only.
pub(super) fn private_dir(dir: &Path) -> std::io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

/// `path` checked to be relative with only normal components.
pub(super) fn relative(path: &str) -> Option<PathBuf> {
    let p = Path::new(path);
    let ok = !path.is_empty() && p.components().all(|c| matches!(c, Component::Normal(_)));
    ok.then(|| p.to_path_buf())
}

/// `base/rel`, refusing a path that leaves `base`, including through a symlink
/// already in the tree (a repository may contain them).
pub(super) fn inside(base: &Path, rel: &str) -> Result<PathBuf, String> {
    let rel = relative(rel)
        .ok_or_else(|| format!("`{rel}` must be a relative path inside the module"))?;
    let base = base
        .canonicalize()
        .map_err(|e| format!("cannot open the module directory: {e}"))?;
    let mut at = base.clone();
    for part in rel.components() {
        at.push(part);
        if let Ok(meta) = fs::symlink_metadata(&at)
            && meta.file_type().is_symlink()
        {
            let real = at
                .canonicalize()
                .map_err(|_| format!("`{}` is a dangling symlink", rel.display()))?;
            if !real.starts_with(&base) {
                return Err(format!(
                    "`{}` leaves the module through a symlink",
                    rel.display()
                ));
            }
        }
    }
    Ok(base.join(rel))
}

/// Writes `content` to `path` with mode 0600, creating parent directories.
/// Refuses to replace a symlink.
pub(super) fn write_private(path: &Path, content: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        private_dir(parent)?;
    }
    if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(std::io::Error::other("refusing to write through a symlink"));
    }
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(content)
}
