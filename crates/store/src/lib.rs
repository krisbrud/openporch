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
mod secret_use;
pub mod secrets;
mod sqlite;
mod sqlite_accounts;
mod sqlite_audit_retention;
mod sqlite_catalog;
mod sqlite_diagnostics;
mod sqlite_identity;
mod sqlite_permits;
mod sqlite_restore;
mod sqlite_values;

pub use audit::{AuditEntry, AuditIntent, AuditOutcome};
pub use deploy::{
    Acceptance, ActiveResource, AppDeletion, AppDeletionRequest, CheckpointResult, DeltaRecord,
    DeploymentKind, DeploymentRecord, DeploymentRequest, DeploymentResource, DeploymentStatus,
    Detach, DraftDelta, EnvRecord, LogLine, NewEnv, OperationKind, OperationStart,
    OperationStarted, Provisioned, RecoveryReport, Resolution, ResourceRunStatus, Tombstone,
    UncertainOperation, UnknownOperation,
};
pub use error::StoreError;
pub use identity::{
    AppRole, AuditFilter, ExternalIdentity, Invitation, NewInvitation, NewPrincipal, OrgRole,
    Principal, Roles, TokenInfo, covers,
};
pub use ownership::Ownership;
pub use secret_use::{SecretOwner, SecretUseStore};
pub use sqlite::{SCHEMA_VERSION, SqliteStore, StoreStats, check_schema};
pub use sqlite_accounts::{AccountRecord, AccountStore, GrantRecord, NewGrant, SecretStoreRecord};
pub use sqlite_audit_retention::{AuditRetention, EXPIRY_ACTION, Expiry, Retained};
pub use sqlite_catalog::{CatalogStore, DefinitionRecord, DefinitionUpdate, DriverRecord};
pub use sqlite_diagnostics::{AuditSummary, OperationSummary};
pub use sqlite_identity::{AppRecord, EnvTypeRecord, IdentityStore};
pub use sqlite_permits::{
    ApprovalRequest, NewPermit, NewPlan, PERMIT_TTL_MS, PermitBinding, PermitRecord, PermitStore,
    PermitUse, PlanRecord,
};
pub use sqlite_restore::{DatabaseCopy, RESTORED, copy_database};
pub use sqlite_values::{
    PutValue, ValueContent, ValueRecord, ValueSetVersion, ValueStore, value_secret_prefix,
};

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

    /// Creates an environment type, `protected` or not. The actor must be an
    /// administrator.
    fn create_env_type(
        &self,
        org: &str,
        id: &str,
        description: &str,
        protected: bool,
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
        env: NewEnv,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Lists an application's environments.
    fn envs(&self, org: &str, app: &str) -> impl Future<Output = Result<Vec<EnvRecord>>> + Send;

    /// Changes an environment type's description, whether it is protected,
    /// or both. The actor must be an administrator.
    fn update_env_type(
        &self,
        org: &str,
        id: &str,
        description: Option<&str>,
        protected: Option<bool>,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Renames an application. The actor must be an administrator or hold `owner`
    /// on it, and the application must not be being deleted.
    fn update_app(
        &self,
        org: &str,
        id: &str,
        name: &str,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Renames an environment. The actor must be an administrator, or hold
    /// `developer` or `owner` on the application and `deployer` on the
    /// environment's type; neither may be being deleted.
    fn update_env(
        &self,
        org: &str,
        app: &str,
        env: &str,
        name: &str,
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

    /// An environment's deployments, newest first and without environment
    /// deletions: at most `limit`, each older than attempt `before_seq` when given.
    fn deployments(
        &self,
        org: &str,
        app: &str,
        env: &str,
        limit: i64,
        before_seq: Option<i64>,
    ) -> impl Future<Output = Result<Vec<DeploymentRecord>>> + Send;

    /// The deployment `actor` submitted with idempotency key `key`, and the
    /// fingerprint of that request.
    fn idempotent_deployment(
        &self,
        org: &str,
        app: &str,
        env: &str,
        actor: &str,
        key: &str,
    ) -> impl Future<Output = Result<Option<(DeploymentRecord, String)>>> + Send;

    /// A deployment's execution snapshot.
    fn deployment_snapshot(
        &self,
        org: &str,
        app: &str,
        env: &str,
        id: &str,
    ) -> impl Future<Output = Result<Option<serde_json::Value>>> + Send;

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

    /// Lists an application's deltas, most recently created first.
    fn deltas(&self, org: &str, app: &str)
    -> impl Future<Output = Result<Vec<DeltaRecord>>> + Send;

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

    /// Stores a set derived through the API, with its audit intent, in one
    /// transaction. Like [`Store::put_set`], storing an existing set is a no-op, but
    /// the request is still audited.
    fn put_set_audited(
        &self,
        org: &str,
        app: &str,
        set: &openporch_core::sets::DeploymentSet,
        audit: AuditIntent,
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

    /// Creates a delta drafted for an environment against its desired set at a
    /// base generation (`draft`); deploying it requires that generation. Fails
    /// with [`StoreError::StaleGeneration`] when the environment has moved on.
    fn put_env_delta(
        &self,
        org: &str,
        app: &str,
        id: &str,
        draft: (&str, i64),
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

    /// Atomically accepts the deletion of an application: marks it deleting, so
    /// it takes no new environments, and accepts the deletion of each of its
    /// environments not already being deleted, under the same checks as
    /// [`Store::accept_deployment`]. The application is removed when the last
    /// of them finishes, or at once if it has none.
    fn accept_app_deletion(
        &self,
        req: AppDeletionRequest,
    ) -> impl Future<Output = Result<AppDeletion>> + Send;

    /// Reads a deployment.
    fn deployment(
        &self,
        org: &str,
        app: &str,
        env: &str,
        id: &str,
    ) -> impl Future<Output = Result<Option<DeploymentRecord>>> + Send;

    /// How each resource fared in a deployment, by descriptor. Resources the
    /// deployment has not reached yet are absent.
    fn deployment_resources(
        &self,
        org: &str,
        app: &str,
        env: &str,
        id: &str,
    ) -> impl Future<Output = Result<Vec<DeploymentResource>>> + Send;

    /// Appends redacted lines to a deployment's log.
    fn append_logs(
        &self,
        org: &str,
        app: &str,
        env: &str,
        deployment_id: &str,
        lines: Vec<LogLine>,
    ) -> impl Future<Output = Result<()>> + Send;

    /// A deployment's log lines in order: at most `limit`, each after line
    /// `after` when given.
    fn deployment_logs(
        &self,
        org: &str,
        app: &str,
        env: &str,
        deployment_id: &str,
        after: Option<i64>,
        limit: i64,
    ) -> impl Future<Output = Result<Vec<LogLine>>> + Send;

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

    /// Adds the resolved inputs to a started operation's recovery metadata
    /// before the driver runs, so recovery can observe or repeat it exactly.
    /// The inputs hold secret references, never secret values.
    fn record_operation_inputs(
        &self,
        operation_id: &str,
        recovery: serde_json::Value,
    ) -> impl Future<Output = Result<()>> + Send;

    /// The operation on `descriptor` whose outcome is unknown, if one is, with
    /// what recovering it needs.
    fn unknown_operation(
        &self,
        org: &str,
        app: &str,
        env: &str,
        descriptor: &str,
    ) -> impl Future<Output = Result<Option<UnknownOperation>>> + Send;

    /// Resolves an operation whose outcome is unknown, with its audit intent,
    /// in one transaction. Refused while a deployment runs in the
    /// environment.
    fn resolve_operation(
        &self,
        operation_id: &str,
        resolution: Resolution,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Records the status of a resource that no operation decided in a
    /// deployment, such as `pending` behind a failed dependency. A status an
    /// operation recorded is kept. A resource with no GUResID yet is recorded
    /// with an empty one.
    #[allow(clippy::too_many_arguments)]
    fn record_resource_status(
        &self,
        org: &str,
        app: &str,
        env: &str,
        deployment_id: &str,
        descriptor: &str,
        status: ResourceRunStatus,
        error: Option<String>,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Reads an active resource.
    fn active_resource(
        &self,
        org: &str,
        app: &str,
        env: &str,
        descriptor: &str,
    ) -> impl Future<Output = Result<Option<ActiveResource>>> + Send;

    /// Lists an environment's active resources by descriptor.
    fn active_resources(
        &self,
        org: &str,
        app: &str,
        env: &str,
    ) -> impl Future<Output = Result<Vec<ActiveResource>>> + Send;

    /// Lists the active resources a definition provisioned, in every
    /// environment.
    fn definition_resources(
        &self,
        org: &str,
        definition: &str,
    ) -> impl Future<Output = Result<Vec<ActiveResource>>> + Send;

    /// Schedules an active resource for deletion by the next deployment that
    /// leaves it out, and advances the environment's generation. Scheduling
    /// twice is harmless; external resources are never deleted and cannot be
    /// scheduled, and nothing is scheduled while a deployment runs.
    fn schedule_deletion(
        &self,
        org: &str,
        app: &str,
        env: &str,
        descriptor: &str,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Detaches an active resource: removes it from management without calling
    /// its driver and leaves a tombstone. Needs an administrator, and no
    /// running deployment or open or uncertain operation on the resource. The
    /// descriptor is not provisioned again until it is adopted. A resource
    /// replaced since the caller checked its permit is refused.
    fn detach_resource(
        &self,
        org: &str,
        app: &str,
        env: &str,
        detach: Detach<'_>,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<Tombstone>> + Send;

    /// Lists an environment's tombstones, oldest first.
    fn tombstones(
        &self,
        org: &str,
        app: &str,
        env: &str,
    ) -> impl Future<Output = Result<Vec<Tombstone>>> + Send;

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
