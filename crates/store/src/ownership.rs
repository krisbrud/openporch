use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

use crate::{Result, StoreError};

/// Exclusive ownership of an installation's data directory, held for the life of
/// the engine through an OS file lock. The lock is released when the process exits,
/// even after a crash, so a stale owner never blocks a restart.
#[derive(Debug)]
pub struct Ownership {
    id: String,
    dir: PathBuf,
    _lock: File,
}

impl Ownership {
    /// Takes ownership of `dir`, creating it if needed. Fails with
    /// [`StoreError::AlreadyOwned`] if another engine holds it.
    pub fn acquire(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir).map_err(|source| StoreError::Io {
            context: format!("creating data directory {}", dir.display()),
            source,
        })?;
        let path = dir.join("openporch.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|source| StoreError::Io {
                context: format!("opening {}", path.display()),
                source,
            })?;
        match file.try_lock() {
            Ok(()) => Ok(Self {
                id: crate::random_hex(16),
                dir: dir.to_owned(),
                _lock: file,
            }),
            Err(TryLockError::WouldBlock) => Err(StoreError::AlreadyOwned {
                path: path.display().to_string(),
            }),
            Err(TryLockError::Error(source)) => Err(StoreError::Io {
                context: format!("locking {}", path.display()),
                source,
            }),
        }
    }

    /// A random ID for this ownership, recorded in the database.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The owned data directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}
