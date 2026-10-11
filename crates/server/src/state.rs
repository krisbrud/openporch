//! Shared server state and startup.

use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;

use openporch_store::secrets::{Keyring, MasterKey, Sealed, SecretError};
use openporch_store::{AuditIntent, IdentityStore, Ownership, SqliteStore, Store, StoreError};

use crate::auth::{self, SessionSigner};
use crate::config::Config;
use crate::rate_limit::RateLimiter;
use openporch_engine::cloud::{AwsSts, CredentialResolver, Sts};

use crate::identity::{Github, IdentityProvider};

/// State shared by every request.
#[derive(Clone, Debug)]
pub struct AppState(Arc<Inner>);

/// The contents of [`AppState`].
#[derive(Debug)]
pub struct Inner {
    /// Configuration.
    pub config: Config,
    /// The store.
    pub store: SqliteStore,
    /// The master keys, when available.
    pub keyring: Option<Arc<Keyring>>,
    /// The session signer, when the keys are available.
    pub signer: Option<SessionSigner>,
    /// Why the server is not ready; empty when it is.
    pub not_ready: Vec<String>,
    /// The GitHub identity provider, when configured.
    pub github: Option<Arc<dyn IdentityProvider>>,
    /// Resolves cloud accounts into credentials.
    pub cloud: CredentialResolver,
    /// Login requests per client address.
    pub login_limit: RateLimiter,
    /// Token creation per principal.
    pub token_limit: RateLimiter,
}

impl std::ops::Deref for AppState {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}

impl AppState {
    /// The organization.
    pub fn org(&self) -> &str {
        &self.config.org
    }

    /// Whether the server can accept changes.
    pub fn ready(&self) -> bool {
        self.not_ready.is_empty()
    }
}

/// Startup failures.
#[derive(Debug, thiserror::Error)]
pub enum StartupError {
    /// The store failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A key failed to load.
    #[error(transparent)]
    Secret(#[from] SecretError),
    /// A file could not be written.
    #[error("{0}")]
    Io(String),
}

/// Writes `contents` to a new owner-only file. Never follows or replaces an existing
/// file or symlink.
fn write_private(path: &Path, contents: &str) -> Result<(), StartupError> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let io = |e: std::io::Error| StartupError::Io(format!("writing {}: {e}", path.display()));
    let mut f = opts.open(path).map_err(io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(io)?;
    }
    f.write_all(contents.as_bytes()).map_err(io)?;
    f.sync_all().map_err(io)?;
    sync_parent(path)
}

/// Makes a created or renamed directory entry durable.
fn sync_parent(path: &Path) -> Result<(), StartupError> {
    #[cfg(unix)]
    if let Some(dir) = path.parent() {
        std::fs::File::open(dir)
            .and_then(|d| d.sync_all())
            .map_err(|e| StartupError::Io(format!("syncing {}: {e}", dir.display())))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Loads the master keys. A new installation without configured keys gets a generated
/// key written to the key file; an existing installation never does, so a lost key is
/// reported instead of silently replaced.
async fn load_keyring(
    config: &Config,
    store: &SqliteStore,
    env_key: Option<String>,
) -> Result<Option<Keyring>, StartupError> {
    if let Some(text) = env_key {
        return Ok(Some(Keyring::from_text(&text)?));
    }
    let path = config.master_key_path();
    if path.exists() {
        return Ok(Some(Keyring::from_file(&path)?));
    }
    if store.is_new().await? && store.required_key_ids().await?.is_empty() {
        let key = MasterKey::generate();
        write_private(&path, &format!("{}\n", key.to_base64().as_str()))?;
        tracing::info!(path = %path.display(), key_id = key.id(), "generated a master key");
        return Ok(Some(Keyring::new(key, [])));
    }
    Ok(None)
}

/// Opens the installation and prepares the server: ownership, store, keys, the session
/// key, restart recovery and, on first start, the bootstrap administrator.
///
/// `env_key` is the value of `OPENPORCH_MASTER_KEY`, if set.
pub async fn start(config: Config, env_key: Option<String>) -> Result<AppState, StartupError> {
    start_with(config, env_key, Arc::new(AwsSts::new())).await
}

/// [`start`] with the given STS client.
pub async fn start_with(
    config: Config,
    env_key: Option<String>,
    sts: Arc<dyn Sts>,
) -> Result<AppState, StartupError> {
    let github = match &config.github {
        Some(c) => {
            Some(Arc::new(Github::new(c).map_err(StartupError::Io)?) as Arc<dyn IdentityProvider>)
        }
        None => None,
    };
    let ownership = Ownership::acquire(&config.data_dir)?;
    let store = SqliteStore::open(ownership, &config.org).await?;
    let keyring = load_keyring(&config, &store, env_key).await?.map(Arc::new);

    let mut not_ready = Vec::new();
    let required = store.required_key_ids().await?;
    match &keyring {
        None => match required.first() {
            Some(id) => not_ready.push(format!("master_key_missing:{id}")),
            None => not_ready.push("master_key_missing".to_owned()),
        },
        Some(k) => {
            if let Err(SecretError::MissingKey { key_id }) = k.ensure_keys(&required) {
                not_ready.push(format!("master_key_missing:{key_id}"));
            }
        }
    }

    let signer = match (&keyring, not_ready.is_empty()) {
        (Some(k), true) => Some(session_signer(&store, k).await?),
        _ => None,
    };

    let report = store.recover_interrupted().await?;
    if !report.failed_deployments.is_empty() || !report.uncertain.is_empty() {
        tracing::warn!(
            failed_deployments = report.failed_deployments.len(),
            uncertain_operations = report.uncertain.len(),
            "marked work interrupted by the previous shutdown"
        );
    }

    if store.is_new().await? {
        bootstrap(&config, &store).await?;
    } else {
        finish_bootstrap_file(&config)?;
    }

    let login_limit = RateLimiter::new(config.rate_limits.login_per_minute);
    let token_limit = RateLimiter::new(config.rate_limits.tokens_per_minute);
    let state = AppState(Arc::new(Inner {
        config,
        store,
        keyring,
        signer,
        not_ready,
        github,
        cloud: CredentialResolver::new(sts),
        login_limit,
        token_limit,
    }));
    prune_sessions(&state);
    Ok(state)
}

/// Drops expired cloud role sessions every minute while the state is in use.
fn prune_sessions(state: &AppState) {
    let weak = Arc::downgrade(&state.0);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            tick.tick().await;
            match weak.upgrade() {
                Some(inner) => inner.cloud.prune(),
                None => break,
            }
        }
    });
}

async fn session_signer(
    store: &SqliteStore,
    keyring: &Keyring,
) -> Result<SessionSigner, StartupError> {
    const AAD: &[u8] = b"openporch-signing-key:session:ed25519";
    let seed = SessionSigner::generate_seed();
    let stored: Sealed = store
        .signing_key("session", "ed25519", keyring.seal(AAD, seed.as_ref()))
        .await?;
    let plain = keyring
        .open(AAD, &stored)?
        .ok_or_else(|| StartupError::Io("the session signing key could not be decrypted".into()))?;
    let seed: [u8; 32] = plain
        .as_slice()
        .try_into()
        .map_err(|_| StartupError::Io("the session signing key is malformed".into()))?;
    Ok(SessionSigner::from_seed(&seed))
}

fn pending_bootstrap_path(config: &Config) -> std::path::PathBuf {
    let mut name = config.bootstrap_token_path().into_os_string();
    name.push(".pending");
    name.into()
}

/// Completes a bootstrap whose database commit succeeded but whose token file was not
/// yet moved into place, for example after a crash.
fn finish_bootstrap_file(config: &Config) -> Result<(), StartupError> {
    let pending = pending_bootstrap_path(config);
    let path = config.bootstrap_token_path();
    if pending.exists() && !path.exists() {
        std::fs::rename(&pending, &path)
            .map_err(|e| StartupError::Io(format!("moving {}: {e}", pending.display())))?;
        sync_parent(&path)?;
        tracing::info!(path = %path.display(), "wrote the bootstrap administrator token");
    }
    Ok(())
}

/// Creates the `bootstrap` administrator and its token in one transaction. The token
/// is written to an owner-only file before the commit and moved into place after it,
/// so a failure leaves either nothing (and the next start bootstraps again) or a
/// committed administrator whose token file is completed on the next start. The token
/// never reaches the logs.
async fn bootstrap(config: &Config, store: &SqliteStore) -> Result<(), StartupError> {
    let org = &config.org;
    let mut audit = AuditIntent::accepted(
        org,
        "bootstrap",
        "POST",
        &format!("/orgs/{org}/users"),
        "bootstrap",
    );
    audit.actor = None;
    audit.target = Some("bootstrap".into());
    let token = auth::new_token();
    let expires = openporch_store::now_ms() + i64::from(config.bootstrap_token_hours) * 3_600_000;

    // Refuse before committing anything, so the token always reaches the configured
    // path and a retry after removing the stale file bootstraps again.
    let path = config.bootstrap_token_path();
    if path.symlink_metadata().is_ok() {
        return Err(StartupError::Io(format!(
            "{} already exists in a new installation; remove the stale file and start again",
            path.display()
        )));
    }
    let pending = pending_bootstrap_path(config);
    match std::fs::remove_file(&pending) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(StartupError::Io(format!(
                "removing {}: {e}",
                pending.display()
            )));
        }
    }
    write_private(&pending, &format!("{}\n", token.as_str()))?;
    if let Err(e) = store
        .create_bootstrap_admin(org, &auth::hash_token(&token), expires, audit)
        .await
    {
        let _ = std::fs::remove_file(&pending);
        return Err(e.into());
    }
    finish_bootstrap_file(config)?;
    Ok(())
}
