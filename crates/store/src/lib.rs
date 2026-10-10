//! Persistence for openporch behind the [`Store`] trait, implemented on SQLite first.
//!
//! One engine owns an installation at a time ([`Ownership`]). Every accepted mutation
//! commits its audit intent in the same transaction as the domain change, so a
//! mutation whose audit record cannot be written is never accepted. The built-in
//! secret store keeps AES-256-GCM ciphertext only ([`secrets`]).

use std::future::Future;

mod audit;
mod deploy;
mod error;
mod identity;
mod ownership;
pub mod secrets;
mod sqlite;
mod sqlite_identity;

pub use audit::{AuditEntry, AuditIntent, AuditOutcome};
pub use deploy::{
    Acceptance, ActiveResource, CheckpointResult, DeltaRecord, DeploymentRecord, DeploymentRequest,
    DeploymentStatus, DraftDelta, EnvRecord, OperationKind, OperationStart, OperationStarted,
    Provisioned, RecoveryReport, UncertainOperation,
};
pub use error::StoreError;
pub use identity::{
    AppRole, AuditFilter, ExternalIdentity, Invitation, NewInvitation, NewPrincipal, OrgRole,
    Principal, Roles, TokenInfo, covers,
};
pub use ownership::Ownership;
pub use sqlite::SqliteStore;
pub use sqlite_identity::{AppRecord, EnvTypeRecord, IdentityStore};

/// The result type of store operations.
pub type Result<T, E = StoreError> = std::result::Result<T, E>;

/// Milliseconds since the Unix epoch, the timestamp format of every stored record.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// A random lowercase hex identifier of `bytes` random bytes.
pub(crate) fn random_hex(bytes: usize) -> String {
    use rand::RngCore;
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// The persistence boundary of the engine and the API server.
///
/// Methods that accept a mutation take an [`AuditIntent`] and commit it atomically
/// with the change. Further methods are added as the API grows.
pub trait Store: Send + Sync {
    /// Records an audit event that has no domain change of its own, such as a
    /// denied privileged request or a failed login.
    fn record_audit(&self, intent: AuditIntent) -> impl Future<Output = Result<String>> + Send;

    /// Lists audit entries, newest first.
    fn audit_entries(
        &self,
        org: &str,
        limit: u32,
    ) -> impl Future<Output = Result<Vec<AuditEntry>>> + Send;

    /// Creates an environment type.
    fn create_env_type(
        &self,
        org: &str,
        id: &str,
        description: &str,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Creates an application, its `development` environment (creating the
    /// `development` type if needed) and makes `owner` its owner.
    fn create_app(
        &self,
        org: &str,
        id: &str,
        name: &str,
        owner: &str,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Creates an environment, empty or with a draft delta. The actor must hold
    /// `developer` or `owner` on the application and `deployer` on the type.
    fn create_env(
        &self,
        org: &str,
        app: &str,
        id: &str,
        name: &str,
        env_type: &str,
        draft: Option<DraftDelta>,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Lists an application's environments.
    fn envs(&self, org: &str, app: &str) -> impl Future<Output = Result<Vec<EnvRecord>>> + Send;

    /// Deletes an environment that has never been deployed, with its drafts and
    /// values. `development` stays while its application exists, and deployed
    /// environments are rejected until deprovisioning on delete exists.
    fn delete_env(
        &self,
        org: &str,
        app: &str,
        env: &str,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Deletes an environment type no environment uses.
    fn delete_env_type(
        &self,
        org: &str,
        id: &str,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<()>> + Send;

    /// The latest deployment attempt in an environment.
    fn latest_deployment(
        &self,
        org: &str,
        app: &str,
        env: &str,
    ) -> impl Future<Output = Result<Option<DeploymentRecord>>> + Send;

    /// Finds a deployment by ID in any environment of an application.
    fn app_deployment(
        &self,
        org: &str,
        app: &str,
        id: &str,
    ) -> impl Future<Output = Result<Option<DeploymentRecord>>> + Send;

    /// Reads a delta.
    fn delta(
        &self,
        org: &str,
        app: &str,
        id: &str,
    ) -> impl Future<Output = Result<Option<DeltaRecord>>> + Send;

    /// Reads a stored deployment set.
    fn set(
        &self,
        org: &str,
        app: &str,
        id: &str,
    ) -> impl Future<Output = Result<Option<openporch_core::sets::DeploymentSet>>> + Send;

    /// Reads an environment's runtime summary.
    fn env(
        &self,
        org: &str,
        app: &str,
        env: &str,
    ) -> impl Future<Output = Result<Option<EnvRecord>>> + Send;

    /// Stores a deployment set; storing identical content again is a no-op.
    fn put_set(
        &self,
        org: &str,
        app: &str,
        set: &openporch_core::sets::DeploymentSet,
    ) -> impl Future<Output = Result<String>> + Send;

    /// Stores or replaces a draft delta with its audit intent. A frozen delta cannot
    /// be replaced.
    fn put_delta(
        &self,
        org: &str,
        app: &str,
        id: &str,
        content: &serde_json::Value,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Atomically accepts a deployment: checks idempotency, generation and running
    /// work, freezes the delta, records the job, desired set and audit intent, and
    /// increments the environment generation.
    fn accept_deployment(
        &self,
        req: DeploymentRequest,
    ) -> impl Future<Output = Result<Acceptance>> + Send;

    /// Reads a deployment.
    fn deployment(
        &self,
        org: &str,
        app: &str,
        env: &str,
        id: &str,
    ) -> impl Future<Output = Result<Option<DeploymentRecord>>> + Send;

    /// Persists a driver operation, allocating the resource's GUResID if it has
    /// none, before the driver is called.
    fn begin_operation(
        &self,
        start: OperationStart,
    ) -> impl Future<Output = Result<OperationStarted>> + Send;

    /// Checkpoints an operation's result before dependent work runs.
    fn checkpoint_operation(
        &self,
        operation_id: &str,
        result: CheckpointResult,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Reads an active resource.
    fn active_resource(
        &self,
        org: &str,
        app: &str,
        env: &str,
        descriptor: &str,
    ) -> impl Future<Output = Result<Option<ActiveResource>>> + Send;

    /// Finishes a deployment and records its correlated audit outcome. Success also
    /// sets the environment's last successful deployment and set.
    fn finish_deployment(
        &self,
        org: &str,
        app: &str,
        env: &str,
        id: &str,
        error: Option<String>,
    ) -> impl Future<Output = Result<()>> + Send;

    /// After a restart: fails interrupted deployments and marks operations that were
    /// started but not checkpointed as unknown, so they are resolved before any
    /// conflicting mutation.
    fn recover_interrupted(&self) -> impl Future<Output = Result<RecoveryReport>> + Send;

    /// Key IDs that stored ciphertext needs. Readiness fails if any is unavailable.
    fn required_key_ids(&self) -> impl Future<Output = Result<Vec<String>>> + Send;
}
