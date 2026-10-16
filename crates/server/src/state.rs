//! Shared server state and startup.

use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use openporch_drivers::DriverRegistry;
use openporch_engine::executor::LocalExecutor;
use openporch_engine::planner::{Limits, PermitCheck, Planner};
use openporch_engine::registry::HttpRegistry;
use openporch_store::secrets::{BuiltinSecretStore, Keyring, MasterKey, Sealed, SecretError};
use openporch_store::{
    AuditIntent, IdentityStore, Ownership, SqliteStore, Store, StoreError, UncertainOperation,
};

use crate::auth::{self, SessionSigner};
use crate::config::Config;
use crate::deploy::{PlannerRunner, Recovery, StorePermits};
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
    pub cloud: Arc<CredentialResolver>,
    /// Browser logins in progress.
    pub browser_logins: crate::browser_login::BrowserLogins,
    /// Login requests per client address.
    pub login_limit: RateLimiter,
    /// Token creation per principal.
    pub token_limit: RateLimiter,
    /// Runs accepted deployments.
    pub runners: crate::deploy::Runners,
    /// What previews know about the drivers.
    pub drivers: crate::deploy::Drivers,
    /// Set once shutdown begins: no new changes are accepted.
    pub draining: std::sync::atomic::AtomicBool,
    /// Set while resources restored from a backup are being observed: no
    /// changes are accepted until each has been.
    pub restoring: std::sync::atomic::AtomicBool,
    /// Changes admitted and still being handled.
    changes: std::sync::atomic::AtomicUsize,
    /// Signaled whenever an admitted change has been handled.
    changes_done: tokio::sync::Notify,
}

/// An admitted change, held until its request has been answered.
#[derive(Debug)]
pub struct Admission(AppState);

impl Drop for Admission {
    fn drop(&mut self) {
        self.0.changes.fetch_sub(1, Ordering::SeqCst);
        self.0.changes_done.notify_waiters();
    }
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

    /// Whether the server started with its keys. [`Self::not_ready_reasons`]
    /// also covers the executor and shutdown.
    pub fn ready(&self) -> bool {
        self.not_ready.is_empty()
    }

    /// Why the server cannot accept changes now, as reason codes that name key
    /// IDs only; empty when it can.
    pub fn not_ready_reasons(&self) -> Vec<String> {
        let mut reasons = self.not_ready.clone();
        if reasons.is_empty() && !self.runners.has_executor() {
            reasons.push("executor_unavailable".to_owned());
        }
        if self.restoring.load(Ordering::SeqCst) {
            reasons.push("restore_reconciling".to_owned());
        }
        if self.draining.load(Ordering::SeqCst) {
            reasons.push("shutting_down".to_owned());
        }
        reasons
    }

    /// Admits a change, or returns why the server cannot accept one. Shutdown
    /// waits for each admission to be dropped.
    pub fn admit_change(&self) -> Result<Admission, Vec<String>> {
        self.admit(Self::not_ready_reasons)
    }

    /// Admits a login, which needs no executor, or returns why the server
    /// cannot accept one. Logins check the keys they need themselves.
    pub fn admit_login(&self) -> Result<Admission, Vec<String>> {
        self.admit(|state| {
            if state.draining.load(Ordering::SeqCst) {
                vec!["shutting_down".to_owned()]
            } else {
                Vec::new()
            }
        })
    }

    fn admit(&self, reasons: impl Fn(&Self) -> Vec<String>) -> Result<Admission, Vec<String>> {
        // Counted before the check, so a shutdown that begins in between
        // either is seen here or waits for this change.
        self.changes.fetch_add(1, Ordering::SeqCst);
        let admission = Admission(self.clone());
        let reasons = reasons(self);
        if reasons.is_empty() {
            Ok(admission)
        } else {
            Err(reasons)
        }
    }

    /// Waits until no admitted change is being handled, or `deadline`. Returns
    /// whether none is.
    async fn wait_for_changes(&self, deadline: tokio::time::Instant) -> bool {
        loop {
            // Created before the check, so a change that ends in between still
            // wakes it.
            let done = self.changes_done.notified();
            if self.changes.load(Ordering::SeqCst) == 0 {
                return true;
            }
            tokio::select! {
                () = done => {}
                () = tokio::time::sleep_until(deadline) => {
                    return self.changes.load(Ordering::SeqCst) == 0;
                }
            }
        }
    }

    /// Begins shutdown: refuses new changes, then waits up to `grace` for the
    /// changes being handled and the running deployments, and returns the
    /// deployments still running. Their operations are already recorded, so a
    /// restart marks them interrupted and observes each resource whose outcome
    /// is unknown before anything changes it again.
    pub async fn drain(&self, grace: Duration) -> Vec<String> {
        let deadline = tokio::time::Instant::now() + grace;
        self.draining.store(true, Ordering::SeqCst);
        tracing::info!(
            grace_seconds = grace.as_secs(),
            "shutting down: refusing changes and waiting for running deployments"
        );
        // A change admitted before the flag may still start a deployment.
        if !self.wait_for_changes(deadline).await {
            tracing::warn!("shutting down while changes are still being handled");
        }
        let left = self
            .runners
            .wait_idle(deadline.saturating_duration_since(tokio::time::Instant::now()))
            .await;
        if !left.is_empty() {
            tracing::warn!(
                deployments = ?left,
                "shutting down with deployments still running: their OpenTofu processes are \
                 stopped and the resources they were changing may be partly changed; on \
                 restart the deployments are marked failed and resources with an unknown \
                 outcome are observed before any deployment changes them"
            );
        }
        left
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
    start_with_drivers(config, env_key, sts, None).await
}

/// [`start_with`] with the drivers in `drivers` instead of the built-in ones
/// and OpenTofu.
pub(crate) async fn start_with_drivers(
    config: Config,
    env_key: Option<String>,
    sts: Arc<dyn Sts>,
    drivers: Option<Arc<DriverRegistry>>,
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
    for _ in &report.failed_deployments {
        openporch_engine::metrics::METRICS.deployment_finished(true);
    }
    if !report.failed_deployments.is_empty() || !report.uncertain.is_empty() {
        tracing::warn!(
            failed_deployments = report.failed_deployments.len(),
            uncertain_operations = report.uncertain.len(),
            "marked work interrupted by the previous shutdown"
        );
    }

    // A restored installation's resources may have changed, or gone, since
    // the backup: each is blocked until it has been observed.
    let marker = config.data_dir.join(crate::backup::RESTORE_MARKER);
    let restoring = marker.exists();
    let restored = if restoring {
        let restored = store.mark_restored().await?;
        tracing::warn!(
            resources = restored.len(),
            "restored from a backup; observing every resource before accepting changes"
        );
        restored
    } else {
        Vec::new()
    };

    if store.is_new().await? {
        bootstrap(&config, &store).await?;
    } else {
        finish_bootstrap_file(&config)?;
    }

    let runners = crate::deploy::Runners::new(config.limits.running_deployments);
    let login_limit = RateLimiter::new(config.rate_limits.login_per_minute);
    let token_limit = RateLimiter::new(config.rate_limits.tokens_per_minute);
    let state = AppState(Arc::new(Inner {
        config,
        store,
        keyring,
        signer,
        not_ready,
        github,
        cloud: Arc::new(CredentialResolver::new(sts)),
        browser_logins: Default::default(),
        login_limit,
        token_limit,
        runners,
        drivers: crate::deploy::Drivers::default(),
        draining: Default::default(),
        restoring: restoring.into(),
        changes: Default::default(),
        changes_done: tokio::sync::Notify::new(),
    }));
    prune_sessions(&state);
    // Images are not left unpinned because the registry client failed.
    let images = &state.config.images;
    let image_registry = if images.pin_other_registries {
        let registry = HttpRegistry::new(
            images.insecure_registries.iter().cloned(),
            images.private_registries.iter().cloned(),
        )
        .map_err(|e| StartupError::Io(format!("the container registry client: {e}")))?;
        Some(Arc::new(registry))
    } else {
        None
    };
    if let (Some(keyring), true) = (state.keyring.clone(), state.ready()) {
        match drivers {
            Some(registry) => run_with_drivers(&state, keyring, image_registry, registry, None),
            None => run_deployments(&state, keyring, image_registry),
        }
        recover_uncertain(
            &state,
            report.uncertain,
            restored,
            restoring.then_some(marker),
        );
    }
    Ok(state)
}

/// Observes, in the background, each resource whose operation the restart
/// left with an unknown outcome, and then each resource restored from a
/// backup, and records what the observation establishes. Nothing is mutated;
/// a resource observation cannot settle stays blocked for an operator.
///
/// Once every restored resource has been observed, or could not be after a
/// few attempts, the restore `marker` is removed and changes are accepted.
fn recover_uncertain(
    state: &AppState,
    uncertain: Vec<UncertainOperation>,
    restored: Vec<UncertainOperation>,
    marker: Option<std::path::PathBuf>,
) {
    if uncertain.is_empty() && marker.is_none() {
        return;
    }
    let state = state.clone();
    tokio::spawn(async move {
        for op in uncertain {
            recover_one(&state, &op, "restart").await;
        }
        let Some(marker) = marker else {
            return;
        };
        let mut pending = restored;
        for delay in RESTORE_RETRIES {
            let mut left = Vec::new();
            for op in pending {
                if !recover_one(&state, &op, "restore").await {
                    left.push(op);
                }
            }
            pending = left;
            if pending.is_empty() {
                break;
            }
            tokio::time::sleep(*delay).await;
        }
        if let Err(e) = std::fs::remove_file(&marker)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::error!(
                error = %e,
                path = %marker.display(),
                "could not remove the restore marker; changes stay refused"
            );
            return;
        }
        state.restoring.store(false, Ordering::SeqCst);
        if pending.is_empty() {
            tracing::info!("every restored resource has been observed; accepting changes");
        } else {
            tracing::warn!(
                unobserved = pending.len(),
                "accepting changes; restored resources that could not be observed stay blocked \
                 until they are recovered explicitly"
            );
        }
    });
}

/// How long to wait before observing again the restored resources that could
/// not be observed.
const RESTORE_RETRIES: &[Duration] = &[
    Duration::from_secs(1),
    Duration::from_secs(5),
    Duration::from_secs(30),
];

/// Observes the resource of `op`; whether its outcome is now known.
async fn recover_one(state: &AppState, op: &UncertainOperation, reason: &str) -> bool {
    let org = state.org();
    let mut audit = AuditIntent::accepted(
        org,
        "system",
        "POST",
        &format!(
            "/orgs/{org}/apps/{}/envs/{}/resources/{}/recover",
            op.app, op.env, op.descriptor
        ),
        "resource.recover",
    );
    audit.actor = None;
    audit.target = Some(format!("{}/{}/{}", op.app, op.env, op.descriptor));
    audit.reason = Some(reason.into());
    let recovered = state
        .runners
        .recover(Recovery {
            app: op.app.clone(),
            env: op.env.clone(),
            descriptor: op.descriptor.clone(),
            audit,
        })
        .await;
    match recovered {
        Ok(outcome) => {
            tracing::info!(
                resource = %op.descriptor,
                guresid = %op.guresid,
                operation = %op.operation_id,
                ?outcome,
                reason,
                "recovered a resource"
            );
            !matches!(outcome, openporch_engine::planner::Recovered::InProgress)
        }
        Err(e) => {
            tracing::warn!(
                resource = %op.descriptor,
                guresid = %op.guresid,
                operation = %op.operation_id,
                error = %e,
                reason,
                "could not recover a resource; recover it explicitly"
            );
            false
        }
    }
}

/// Runs accepted deployments with the built-in drivers and the OpenTofu
/// driver, in this process. Without the master keys nothing is accepted, so
/// nothing runs.
fn run_deployments(
    state: &AppState,
    keyring: Arc<Keyring>,
    image_registry: Option<Arc<HttpRegistry>>,
) {
    let tofu = state.config.opentofu();
    let backend = tofu.state.location();
    let registry = Arc::new(DriverRegistry::with_builtins().with_opentofu(tofu));
    run_with_drivers(state, keyring, image_registry, registry, Some(backend));
}

/// Runs accepted deployments with the drivers in `registry`, in this process.
pub(crate) fn run_with_drivers(
    state: &AppState,
    keyring: Arc<Keyring>,
    image_registry: Option<Arc<HttpRegistry>>,
    registry: Arc<DriverRegistry>,
    backend: Option<String>,
) {
    state.drivers.set(registry.clone(), backend);
    let store = state.store.clone();
    let checks: Arc<dyn PermitCheck> = Arc::new(StorePermits(store.clone()));
    let secrets = Arc::new(BuiltinSecretStore::new(
        &store,
        keyring.clone(),
        state.org(),
    ));
    let mut executor = LocalExecutor::new(
        Arc::new(store.clone()),
        registry,
        secrets,
        keyring,
        state.cloud.clone(),
        checks.clone(),
    );
    if let Some(image_registry) = image_registry {
        executor = executor.with_registry(image_registry);
    }
    let configured = &state.config.limits;
    let limits = Limits {
        concurrency: configured.driver_concurrency,
        deadline: std::time::Duration::from_secs(configured.deployment_minutes * 60),
        log_bytes: configured.deployment_log_bytes,
        ..Limits::default()
    };
    let planner =
        Planner::new(Arc::new(store), Arc::new(executor), limits).with_permit_check(checks);
    state
        .runners
        .set(Arc::new(PlannerRunner::new(Arc::new(planner))));
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
