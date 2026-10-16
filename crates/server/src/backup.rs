//! `openporch backup` and `openporch restore`: a consistent copy of an
//! installation taken while the server is stopped, and its return into an
//! empty data directory. A restored server observes every resource before
//! anything changes; see [`crate::state`].
//!
//! A backup is a directory:
//!
//! - `backup.json`, the manifest;
//! - `data/`, the data directory: a consistent copy of the database and every
//!   other file, except the lock, the master key and the bootstrap token;
//! - `opentofu-state/`, the OpenTofu driver's `state` directory: local state,
//!   and the scripts and lock files resources were provisioned with.
//!
//! The master key is never in a backup; it is backed up on its own, and the
//! manifest names the key IDs the backup needs.

use std::path::{Path, PathBuf};

use openporch_store::{Ownership, SCHEMA_VERSION, StoreError};
use serde::{Deserialize, Serialize};

use crate::Config;
use crate::config::StateSettings;

/// The manifest's file name.
pub const MANIFEST: &str = "backup.json";
/// Marks a data directory restored from a backup whose resources have not all
/// been observed yet.
pub const RESTORE_MARKER: &str = "restore.json";
/// The format this build writes and reads.
const FORMAT: u32 = 1;
const DATA: &str = "data";
const OPENTOFU_STATE: &str = "opentofu-state";

/// What a backup holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// The backup format.
    pub format: u32,
    /// When it was taken, in milliseconds since the Unix epoch.
    pub created_at: u64,
    /// The version of the server that took it.
    pub version: String,
    /// The database's schema version.
    pub schema_version: i64,
    /// The organization.
    pub org: String,
    /// The master key IDs the restored server needs.
    pub key_ids: Vec<String>,
    /// Resources recorded as active.
    pub active_resources: i64,
    /// Where OpenTofu state was kept: `local`, in the backup, or the S3
    /// location, which the backup does not hold.
    pub state_backend: String,
}

/// Why a backup or restore failed.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct BackupError(String);

impl From<StoreError> for BackupError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::AlreadyOwned { .. } => Self(
                "the data directory is in use: stop the server first, a backup or restore needs \
                 it stopped"
                    .into(),
            ),
            e => Self(e.to_string()),
        }
    }
}

fn io(context: impl std::fmt::Display) -> impl FnOnce(std::io::Error) -> BackupError {
    move |e| BackupError(format!("{context}: {e}"))
}

/// Backs the stopped installation up into `dest`, which must be absent or
/// an empty directory.
pub async fn backup(config: &Config, dest: &Path) -> Result<Manifest, BackupError> {
    if !config.data_dir.join("openporch.db").is_file() {
        return Err(BackupError(format!(
            "{} holds no installation",
            config.data_dir.display()
        )));
    }
    let ownership = Ownership::acquire(&config.data_dir)?;
    let created = !dest.exists();
    prepare_empty(dest)?;
    // What a failed backup wrote is removed, so the directory can be reused.
    let taken = take(config, &ownership, dest).await;
    if taken.is_err() && created {
        let _ = std::fs::remove_dir_all(dest);
    } else if taken.is_err()
        && let Ok(entries) = std::fs::read_dir(dest)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            let _ = if path.is_dir() && !path.is_symlink() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
        }
    }
    taken
}

async fn take(
    config: &Config,
    ownership: &Ownership,
    dest: &Path,
) -> Result<Manifest, BackupError> {
    // Paths are compared as the file system resolves them, so a file the
    // configuration spells differently is still left out.
    let data_dir = canonical(&config.data_dir)?;
    let tofu_root = tofu_root(config);
    let dest_dir = canonical(dest)?;
    if dest_dir.starts_with(&data_dir)
        || std::fs::canonicalize(&tofu_root).is_ok_and(|root| dest_dir.starts_with(root))
    {
        return Err(BackupError(format!(
            "{} is inside the installation; back it up somewhere else",
            dest.display()
        )));
    }
    let data = dest.join(DATA);
    create_private_dir(&data)?;
    let copy = openporch_store::copy_database(ownership, &data.join("openporch.db")).await?;
    let org = copy.org.clone().unwrap_or_else(|| config.org.clone());
    if org != config.org {
        return Err(BackupError(format!(
            "the data directory serves `{org}`, not the configured `{}`",
            config.org
        )));
    }
    let skip = skipped(config, &data_dir, &tofu_root);
    copy_tree(&data_dir, &data, &|p| skip.iter().any(|s| s == p))?;
    let state = tofu_root.join("state");
    if state.is_dir() {
        copy_tree(&state, &dest.join(OPENTOFU_STATE), &|_| false)?;
    }
    let manifest = Manifest {
        format: FORMAT,
        created_at: now_ms(),
        version: crate::VERSION.to_owned(),
        schema_version: copy.schema_version,
        org,
        key_ids: copy.key_ids,
        active_resources: copy.active_resources,
        state_backend: state_backend(config),
    };
    write_json(&dest.join(MANIFEST), &manifest)?;
    Ok(manifest)
}

/// Restores the backup in `from` into the configured data directory, which
/// must be absent or empty, as must the OpenTofu state directory. The next
/// start observes every resource before it accepts changes.
pub async fn restore(config: &Config, from: &Path) -> Result<Manifest, BackupError> {
    let raw = std::fs::read(from.join(MANIFEST))
        .map_err(io(format!("reading {}", from.join(MANIFEST).display())))?;
    let manifest: Manifest = serde_json::from_slice(&raw)
        .map_err(|e| BackupError(format!("{MANIFEST} is not a backup manifest: {e}")))?;
    if manifest.format != FORMAT {
        return Err(BackupError(format!(
            "the backup has format {}; this build reads format {FORMAT}",
            manifest.format
        )));
    }
    if manifest.schema_version > SCHEMA_VERSION {
        return Err(BackupError(format!(
            "the backup has schema {}, newer than this build's {SCHEMA_VERSION}: restore it with \
             openporch {} or later",
            manifest.schema_version, manifest.version
        )));
    }
    if manifest.org != config.org {
        return Err(BackupError(format!(
            "the backup is of `{}`, not the configured `{}`",
            manifest.org, config.org
        )));
    }
    if manifest.state_backend != state_backend(config) {
        return Err(BackupError(format!(
            "the backup's OpenTofu state is in {}, but the configuration keeps it in {}",
            manifest.state_backend,
            state_backend(config)
        )));
    }
    let data = from.join(DATA);
    if !data.join("openporch.db").is_file() {
        return Err(BackupError(format!("{} holds no database", data.display())));
    }
    prepare_empty(&config.data_dir)?;
    let state = tofu_root(config).join("state");
    let tofu_state = from.join(OPENTOFU_STATE);
    if tofu_state.is_dir() && !state.starts_with(&config.data_dir) {
        prepare_empty(&state)?;
    }
    let _ownership = Ownership::acquire(&config.data_dir)?;
    // The marker comes first: a restore that fails halfway still starts
    // read-only and observes whatever it restored.
    write_json(
        &config.data_dir.join(RESTORE_MARKER),
        &serde_json::json!({
            "restored_at": now_ms(),
            "backup_created_at": manifest.created_at,
            "backup_version": manifest.version,
        }),
    )?;
    copy_tree(&data, &config.data_dir, &|_| false)?;
    if tofu_state.is_dir() {
        copy_tree(&tofu_state, &state, &|_| false)?;
    }
    Ok(manifest)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn tofu_root(config: &Config) -> PathBuf {
    config
        .opentofu
        .root
        .clone()
        .unwrap_or_else(|| config.data_dir.join("opentofu"))
}

fn state_backend(config: &Config) -> String {
    match &config.opentofu.state {
        StateSettings::Local {} => "local".into(),
        StateSettings::S3 {
            bucket, key_prefix, ..
        } => format!("s3://{bucket}/{key_prefix}"),
    }
}

/// What the data directory's copy leaves out: the database, copied on its
/// own; the lock and the restore marker, which belong to a running server;
/// the master key and the bootstrap token, which are credentials; and the
/// OpenTofu root, whose state is copied on its own and whose runs and plugin
/// cache are rebuilt.
fn skipped(config: &Config, dir: &Path, tofu_root: &Path) -> Vec<PathBuf> {
    let mut pending = config.bootstrap_token_path().into_os_string();
    pending.push(".pending");
    let paths = [
        dir.join("openporch.db"),
        dir.join("openporch.db-wal"),
        dir.join("openporch.db-shm"),
        dir.join("openporch.db-journal"),
        dir.join("openporch.lock"),
        dir.join(RESTORE_MARKER),
        config.master_key_path(),
        config.bootstrap_token_path(),
        pending.into(),
        tofu_root.to_owned(),
    ];
    // A path that does not exist is never met while copying.
    paths
        .into_iter()
        .map(|p| std::fs::canonicalize(&p).unwrap_or(p))
        .collect()
}

fn canonical(path: &Path) -> Result<PathBuf, BackupError> {
    std::fs::canonicalize(path).map_err(io(format!("resolving {}", path.display())))
}

/// Creates `dir` with owner-only permissions unless it exists; refuses one
/// that holds anything.
fn prepare_empty(dir: &Path) -> Result<(), BackupError> {
    match std::fs::read_dir(dir) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                return Err(BackupError(format!("{} is not empty", dir.display())));
            }
            #[cfg(unix)]
            std::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))
                .map_err(io(format!("restricting {}", dir.display())))?;
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => create_private_dir(dir),
        Err(e) => Err(io(format!("reading {}", dir.display()))(e)),
    }
}

fn create_private_dir(dir: &Path) -> Result<(), BackupError> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(dir)
        .map_err(io(format!("creating {}", dir.display())))
}

/// Copies `from` into `to`, recursively, without following symbolic links,
/// leaving out the paths `skip` matches. Copies are readable by their owner
/// only, which keeps an executable file executable.
fn copy_tree(from: &Path, to: &Path, skip: &dyn Fn(&Path) -> bool) -> Result<(), BackupError> {
    create_private_dir(to)?;
    let entries = std::fs::read_dir(from).map_err(io(format!("reading {}", from.display())))?;
    for entry in entries {
        let entry = entry.map_err(io(format!("reading {}", from.display())))?;
        let path = entry.path();
        if skip(&path) {
            continue;
        }
        let target = to.join(entry.file_name());
        let kind = entry
            .file_type()
            .map_err(io(format!("reading {}", path.display())))?;
        if kind.is_dir() {
            copy_tree(&path, &target, skip)?;
        } else if kind.is_symlink() {
            let link =
                std::fs::read_link(&path).map_err(io(format!("reading {}", path.display())))?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(&link, &target)
                .map_err(io(format!("creating {}", target.display())))?;
            #[cfg(not(unix))]
            return Err(BackupError(format!(
                "{} is a symbolic link to {}",
                path.display(),
                link.display()
            )));
        } else if kind.is_file() {
            copy_file(&path, &target)?;
        }
    }
    Ok(())
}

fn copy_file(from: &Path, to: &Path) -> Result<(), BackupError> {
    let mut source =
        std::fs::File::open(from).map_err(io(format!("reading {}", from.display())))?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mode = source
            .metadata()
            .map_err(io(format!("reading {}", from.display())))?
            .permissions()
            .mode();
        options.mode(0o600 | (mode & 0o100));
    }
    let mut target = options
        .open(to)
        .map_err(io(format!("creating {}", to.display())))?;
    std::io::copy(&mut source, &mut target).map_err(io(format!("writing {}", to.display())))?;
    target
        .sync_all()
        .map_err(io(format!("writing {}", to.display())))
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<(), BackupError> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options
        .open(path)
        .map_err(io(format!("creating {}", path.display())))?;
    let body = serde_json::to_vec_pretty(value).map_err(|e| BackupError(e.to_string()))?;
    std::io::Write::write_all(&mut file, &body)
        .and_then(|()| file.sync_all())
        .map_err(io(format!("writing {}", path.display())))
}
