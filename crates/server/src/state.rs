//! Shared server state and startup.

use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;

use openporch_store::secrets::{Keyring, MasterKey, Sealed, SecretError};
use openporch_store::{
    AuditIntent, IdentityStore, NewPrincipal, OrgRole, Ownership, SqliteStore, Store, StoreError,
};

use crate::auth::{self, SessionSigner};
use crate::config::Config;

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
    _ownership: Ownership,
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

/// Writes `contents` to a new or truncated owner-only file.
fn write_private(path: &Path, contents: &str, create_new: bool) -> Result<(), StartupError> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true);
    if create_new {
        opts.create_new(true);
    } else {
        opts.create(true).truncate(true);
    }
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
    f.sync_all().map_err(io)
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
        write_private(&path, &format!("{}\n", key.to_base64().as_str()), true)?;
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
    let ownership = Ownership::acquire(&config.data_dir)?;
    let store = SqliteStore::open(&ownership, &config.org).await?;
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
    }

    Ok(AppState(Arc::new(Inner {
        config,
        store,
        keyring,
        signer,
        not_ready,
        _ownership: ownership,
    })))
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

/// Creates the `bootstrap` administrator and writes its token to the owner-only
/// bootstrap file. The token never reaches the logs.
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
    store
        .create_principal(
            org,
            NewPrincipal {
                id: "bootstrap".into(),
                kind: "service".into(),
                name: "Bootstrap administrator".into(),
                email: None,
                role: OrgRole::Administrator,
            },
            audit.clone(),
        )
        .await?;
    let token = auth::new_token();
    let expires = openporch_store::now_ms() + i64::from(config.bootstrap_token_hours) * 3_600_000;
    store
        .create_token(
            org,
            "bootstrap",
            "bootstrap",
            Some("Created on first start"),
            Some(expires),
            &auth::hash_token(&token),
            AuditIntent {
                action: "token.create".into(),
                path: format!("/orgs/{org}/users/bootstrap/tokens"),
                ..audit
            },
        )
        .await?;
    let path = config.bootstrap_token_path();
    write_private(&path, &format!("{}\n", token.as_str()), false)?;
    tracing::info!(path = %path.display(), "wrote the bootstrap administrator token");
    Ok(())
}
