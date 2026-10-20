//! The planner: walks a deployment's resource graph, resolves each resource's
//! non-secret inputs, hands provisioning to an [`Executor`] and records every
//! outcome before dependents run.
//!
//! Secret values never reach the planner. Inputs that involve secrets stay
//! [`Resolved`] expressions over [`SecretRef`]s, and only the executor reads them.
//! Every operation is persisted before the executor is called and checkpointed
//! after, so an interrupted deployment leaves a record of what may have changed.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tracing::Instrument;

use openporch_core::canonical::to_canonical_string;
use openporch_core::catalog::{ResourceType, ResourceUse, builtin_types};
use openporch_core::graph::{Node, ResourceGraph};
use openporch_core::ids::{
    AccountId, AppId, DefinitionId, Descriptor, EnvId, EnvTypeId, Guresid, OrgId, ResourceTypeId,
};
use openporch_core::model::{ResourceDefinition, SecretRef};
use openporch_core::placeholder::{
    ContextKey, Dialect, Expr, Lookup, MapScope, Resolved, ResourceOutputs, Scope, SecretExpr,
    SharedValue, Sink, resolve_value,
};
use openporch_store::secrets::Sealed;
use openporch_store::{
    CheckpointResult, OperationKind, OperationStart, Provisioned, ResourceRunStatus, Store,
    StoreError,
};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::Instant;

use crate::cloud::{EcrDigest, EcrImage, S3Object, S3Version, in_ecr, pinned_image};
use crate::registry::{OciImage, pinned_from_tag};

mod deprovision;
mod log;
mod recover;
#[cfg(test)]
mod tests;

pub use deprovision::{
    DestructiveAction, NoPermits, PermitCheck, PermitRequest, deletion_candidates,
};
use log::DeploymentLog;
pub use log::{JobLog, RedactingLog};
pub use recover::{RecoverError, Recovered};

/// A deployment as the planner runs it: the accepted configuration, never the
/// current one.
#[derive(Debug, Clone)]
pub struct Deployment {
    /// Organization.
    pub org: OrgId,
    /// Application.
    pub app: AppId,
    /// Environment.
    pub env: EnvId,
    /// Environment type.
    pub env_type: EnvTypeId,
    /// Deployment ID.
    pub id: String,
    /// The deployed set.
    pub set_id: String,
    /// The resource graph to converge.
    pub graph: ResourceGraph,
    /// The definitions the graph matched, with their versions.
    pub definitions: BTreeMap<DefinitionId, (i64, ResourceDefinition)>,
    /// Shared values, environment overrides applied.
    pub values: BTreeMap<String, SharedValue>,
    /// The destructive-action permits the deployment was submitted with.
    pub permits: Vec<String>,
    /// The version of the drivers' code each resource ran, by descriptor, as
    /// recorded when it provisioned: a replay runs the same versions.
    pub pins: BTreeMap<String, Value>,
    /// The resource types the graph uses, as the snapshot recorded them.
    /// Snapshots from before types were recorded have none, and their outputs
    /// are not checked.
    pub types: BTreeMap<ResourceTypeId, ResourceType>,
    /// Whether this deletes the environment: the graph is empty, and every
    /// owned resource is deleted rather than only removed workloads and those
    /// scheduled for deletion.
    pub teardown: bool,
}

impl Deployment {
    /// Reads the graph and definitions from an execution snapshot.
    pub fn from_snapshot(
        org: OrgId,
        app: AppId,
        env: EnvId,
        env_type: EnvTypeId,
        id: String,
        set_id: String,
        snapshot: &Value,
    ) -> Result<Self, String> {
        let graph: ResourceGraph = serde_json::from_value(snapshot["graph"].clone())
            .map_err(|e| format!("the snapshot's graph is unreadable: {e}"))?;
        let mut definitions = BTreeMap::new();
        for (id, entry) in snapshot["definitions"].as_object().into_iter().flatten() {
            let version = entry["version"]
                .as_i64()
                .ok_or_else(|| format!("the snapshot has no version for definition `{id}`"))?;
            let def: ResourceDefinition = serde_json::from_value(entry["definition"].clone())
                .map_err(|e| format!("the snapshot's definition `{id}` is unreadable: {e}"))?;
            definitions.insert(def.id.clone(), (version, def));
        }
        let mut values = BTreeMap::new();
        for (key, entry) in snapshot["values"]["resolved"]
            .as_object()
            .into_iter()
            .flatten()
        {
            let value = match (entry.get("value"), entry.get("secret_ref")) {
                (Some(Value::String(text)), None) => {
                    SharedValue::Value(Value::String(text.clone()))
                }
                (None, Some(reference)) => SharedValue::Secret(
                    serde_json::from_value(reference.clone())
                        .map_err(|e| format!("the snapshot's value `{key}` is unreadable: {e}"))?,
                ),
                _ => return Err(format!("the snapshot's value `{key}` is unreadable")),
            };
            values.insert(key.clone(), value);
        }
        Ok(Self {
            org,
            app,
            env,
            env_type,
            id,
            set_id,
            graph,
            definitions,
            values,
            permits: snapshot["permits"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|p| p.as_str().map(str::to_owned))
                .collect(),
            pins: snapshot["pins"]
                .as_object()
                .map(|pins| pins.clone().into_iter().collect())
                .unwrap_or_default(),
            types: snapshot["types"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(id, ty)| {
                    let ty: ResourceType = serde_json::from_value(ty.clone())
                        .map_err(|e| format!("the snapshot's type `{id}` is unreadable: {e}"))?;
                    Ok((ty.id.clone(), ty))
                })
                .collect::<Result<_, String>>()?,
            teardown: false,
        })
    }

    /// The type of the resources of type `ty`, whose output schema their
    /// driver's outputs must satisfy.
    pub fn output_schema(&self, ty: &ResourceTypeId) -> Option<ResourceType> {
        self.types.get(ty).cloned()
    }
}

/// Everything the executor needs to provision one resource. It carries secret
/// references, never secret values.
#[derive(Debug, Clone)]
pub struct ProvisionJob {
    /// The durable operation ID of this attempt; each retry gets a new one. The
    /// GUResID is the identity that stays the same across attempts.
    pub operation_id: String,
    /// The attempt within the deployment, from 1.
    pub attempt: i64,
    /// The resource.
    pub descriptor: Descriptor,
    /// Its permanent identity.
    pub guresid: Guresid,
    /// The deployment the job belongs to.
    pub context: JobContext,
    /// The matched definition.
    pub definition_id: DefinitionId,
    /// The definition's version in the snapshot.
    pub definition_version: i64,
    /// The driver.
    pub driver_type: String,
    /// The cloud account the driver gets credentials for.
    pub driver_account: Option<AccountId>,
    /// The resolved inputs.
    pub inputs: JobInputs,
    /// The digest of the inputs, so retries can be recognized.
    pub input_fingerprint: String,
    /// The driver's cookie from the last successful operation, encrypted.
    pub cookie: Option<Sealed>,
    /// What the cloud account resolved to when the resource last provisioned,
    /// such as an AWS account number. Before calling the driver, the executor
    /// refuses the job with a permanent failure if the account now resolves to
    /// another target: moving a resource is an explicit migration.
    pub target: Option<String>,
    /// The version of the driver's code to run, as the deployment this one
    /// replays recorded it; none to resolve the definition's version.
    pub pin: Option<Value>,
    /// Whether the resource provisioned before. A change to an existing
    /// resource whose impact the driver cannot preview needs a permit; a
    /// first creation destroys nothing and does not.
    pub existing: bool,
    /// The resource's type, whose output schema the driver's outputs must
    /// satisfy; none when nothing is provisioned, such as a check or a deletion.
    pub output_schema: Option<ResourceType>,
    /// When the deployment's time runs out.
    pub deadline: Instant,
    /// Where the executor writes the resource's log. Before writing, it
    /// registers every secret value it resolves with [`JobLog::secret`], so
    /// that no line or error carries one.
    pub log: JobLog,
}

/// The deployment a job belongs to: the scope its secret references are
/// authorized for, and the permits it was submitted with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobContext {
    /// Organization.
    pub org: OrgId,
    /// Application.
    pub app: AppId,
    /// Environment.
    pub env: EnvId,
    /// Environment type.
    pub env_type: EnvTypeId,
    /// Deployment ID.
    pub deployment_id: String,
    /// The destructive-action permits the deployment was submitted with.
    pub permits: Vec<String>,
}

impl JobContext {
    /// The context of a job in `deployment`.
    pub fn of(deployment: &Deployment) -> Self {
        Self {
            org: deployment.org.clone(),
            app: deployment.app.clone(),
            env: deployment.env.clone(),
            env_type: deployment.env_type.clone(),
            deployment_id: deployment.id.clone(),
            permits: deployment.permits.clone(),
        }
    }
}

/// Where a secret reference in a job's inputs came from, which decides whose
/// use of it the executor authorizes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, serde::Deserialize)]
#[serde(tag = "from", rename_all = "snake_case")]
pub enum SecretSource {
    /// A shared value of the application or environment.
    Values,
    /// A secret output of the resource with this GUResID.
    Output {
        /// The resource.
        guresid: Guresid,
    },
}

/// Everything the executor needs to delete one resource: the job it was
/// provisioned with, plus what its last successful operation retained, since
/// the definition may have changed since.
#[derive(Debug, Clone)]
pub struct DeprovisionJob {
    /// The resource's identity, driver, account, inputs and cookie as last
    /// provisioned.
    pub job: ProvisionJob,
    /// The configuration of the last successful operation, including the
    /// definition as it was then.
    pub snapshot: Value,
    /// The driver's non-secret recovery metadata from that operation.
    pub recovery: Value,
}

/// A resource's resolved inputs.
#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
pub struct JobInputs {
    /// Non-secret driver inputs.
    pub values: Value,
    /// Driver inputs given as templates that may contain secrets.
    pub secrets: Resolved,
    /// Driver inputs given as secret references.
    pub secret_refs: Resolved,
    /// The resource inputs (`params`), which may contain secrets.
    pub resource: Resolved,
    /// Where each secret reference in `secrets` and `resource` came from. The
    /// executor refuses a reference it finds none for; those in `secret_refs`
    /// belong to the definition.
    #[serde(default)]
    pub sources: Vec<(SecretRef, SecretSource)>,
}

/// What a successful provisioning returns: non-secret outputs and references.
#[derive(Debug, Clone, Default)]
pub struct ProvisionResult {
    /// Non-secret outputs.
    pub outputs: serde_json::Map<String, Value>,
    /// Secret outputs, already written to a secret store.
    pub secret_outputs: BTreeMap<String, SecretRef>,
    /// The new cookie, encrypted.
    pub cookie: Option<Sealed>,
    /// Non-secret recovery metadata.
    pub recovery: Value,
    /// What the cloud account resolved to, such as an AWS account number.
    pub target: Option<String>,
    /// Where the driver keeps the resource's state, such as an S3 location. A
    /// detached resource's tombstone keeps it for a later adoption.
    pub backend: Option<String>,
    /// The cloud objects the resource consists of, such as ARNs.
    pub external_ids: Vec<String>,
    /// The version of the code the driver ran, which a replay runs again.
    pub pin: Option<Value>,
}

/// Why provisioning failed. Messages are already redacted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecFailure {
    /// Nothing changed, and retrying cannot help.
    Permanent(String),
    /// Nothing changed; retry later. A driver that cannot tell whether something
    /// changed returns [`ExecFailure::Uncertain`] instead.
    Retryable {
        /// The error.
        error: String,
        /// A delay the driver asked for.
        retry_after: Option<Duration>,
    },
    /// Infrastructure may have changed; recovery must observe it first.
    Uncertain {
        /// The error.
        error: String,
        /// The driver's newest cookie, sealed as a provisioned resource's is,
        /// when the mutation returned one before its outcome became unknown.
        /// The planner stores it on the resource, so recovery observes with it.
        cookie: Option<Sealed>,
    },
}

impl ExecFailure {
    /// An uncertain outcome that leaves the resource's cookie as it is.
    pub fn uncertain(error: impl Into<String>) -> Self {
        Self::Uncertain {
            error: error.into(),
            cookie: None,
        }
    }
}

/// The span an operation's events are logged in: they name the operation, the
/// resource's GUResID and descriptor, and, through the enclosing deployment
/// span, the request that accepted the deployment. The deployment is named
/// here too, for recovery, which runs outside it. It is at error
/// level so it is kept whatever `RUST_LOG` hides.
pub(crate) fn operation_span(job: &ProvisionJob, action: &'static str) -> tracing::Span {
    tracing::error_span!(
        "operation",
        deployment = %job.context.deployment_id,
        operation = %job.operation_id,
        guresid = %job.guresid.as_str(),
        resource = %job.descriptor,
        driver = %job.driver_type,
        attempt = job.attempt,
        action,
    )
}

/// The span a resource's work in a deployment is logged in, so events outside
/// the driver call, such as a failed checkpoint, name the resource and, once
/// [`record_operation`] runs, its current operation and GUResID.
pub(crate) fn resource_span(descriptor: &Descriptor) -> tracing::Span {
    tracing::error_span!(
        "resource",
        resource = %descriptor,
        operation = tracing::field::Empty,
        guresid = tracing::field::Empty,
    )
}

/// Records the operation just started on the current [`resource_span`].
pub(crate) fn record_operation(started: &openporch_store::OperationStarted) {
    record_operation_ids(&started.operation_id, started.guresid.as_str());
}

/// Records operation `operation` on GUResID `guresid` on the current
/// [`resource_span`].
pub(crate) fn record_operation_ids(operation: &str, guresid: &str) {
    let span = tracing::Span::current();
    span.record("operation", operation);
    span.record("guresid", guresid);
}

/// Logs what the executor returned for an operation. This is the driver's
/// result, not the recorded one: a checkpoint that fails afterwards logs
/// "could not checkpoint an operation" and leaves the outcome unknown.
/// Executor failures are already redacted.
/// The call, for `action`, began at `started`; it is counted in
/// [`crate::metrics::METRICS`].
pub(crate) fn log_outcome<T>(
    action: &'static str,
    started: Instant,
    result: &Result<T, ExecFailure>,
) {
    let outcome = match result {
        Ok(_) => {
            tracing::info!(outcome = "succeeded", "driver finished");
            "succeeded"
        }
        Err(ExecFailure::Permanent(e)) => {
            tracing::warn!(outcome = "failed", error = %e, "driver finished");
            "failed"
        }
        Err(ExecFailure::Retryable { error, .. }) => {
            tracing::warn!(outcome = "retryable", error = %error, "driver finished");
            "retryable"
        }
        Err(ExecFailure::Uncertain { error: e, .. }) => {
            tracing::warn!(outcome = "unknown", error = %e, "driver finished");
            "unknown"
        }
    };
    crate::metrics::METRICS.driver_finished(action, outcome, started.elapsed());
}

/// Provisions resources: resolves secrets, obtains credentials and calls drivers.
///
/// Secret values stay in the executor. It writes a job's log through
/// [`JobLog::redacting`], registers every value it resolves there, and passes
/// every failure it returns through [`RedactingLog::failure`].
pub trait Executor: Send + Sync + 'static {
    /// Provisions one resource.
    fn provision(
        &self,
        job: ProvisionJob,
    ) -> impl Future<Output = Result<ProvisionResult, ExecFailure>> + Send;

    /// Deletes one resource with the driver, definition and state it was
    /// provisioned with, as retained in the job, not with the current
    /// definition. `Ok` means the deletion is confirmed.
    fn deprovision(
        &self,
        job: DeprovisionJob,
    ) -> impl Future<Output = Result<(), ExecFailure>> + Send;

    /// Observes one resource without changing it, to resolve an operation
    /// whose outcome is unknown. `deleting` is set when that operation was a
    /// deletion: a resource found then is only reported, its outputs are not
    /// kept. An executor that cannot observe reports [`Observed::Unknown`].
    fn observe(
        &self,
        job: ProvisionJob,
        deleting: bool,
    ) -> impl Future<Output = Result<Observation, ExecFailure>> + Send {
        let _ = (job, deleting);
        async { Ok(Observed::Unknown("this executor cannot observe resources".into()).into()) }
    }

    /// Checks that the artifacts a workload deploys still exist, before the
    /// deployment changes anything. An executor that cannot check accepts
    /// them, and a missing artifact fails when the workload deploys.
    fn check_artifacts(
        &self,
        check: ArtifactCheck,
    ) -> impl Future<Output = Result<(), ExecFailure>> + Send {
        let _ = check;
        async { Ok(()) }
    }

    /// Checks, without changing anything, that the version of the driver's
    /// code `job.pin` names can still run, such as a module commit that can
    /// still be fetched. The job carries the definition's driver inputs but no
    /// resource inputs. An executor that cannot check accepts it, and a
    /// missing version fails when the resource's turn comes.
    fn check_pin(&self, job: ProvisionJob) -> impl Future<Output = Result<(), ExecFailure>> + Send {
        let _ = job;
        async { Ok(()) }
    }

    /// Checks, without changing anything, that the version of the driver's
    /// code a resource was last applied with, which deleting it runs, can
    /// still run. An executor that cannot check accepts it, and a missing
    /// version fails when the deletion runs.
    fn check_retained(
        &self,
        job: DeprovisionJob,
    ) -> impl Future<Output = Result<(), ExecFailure>> + Send {
        let _ = job;
        async { Ok(()) }
    }

    /// Pins `pin.artifact` to the identity it has now, such as an S3 object
    /// version. An artifact without a fixed identity, such as an object in an
    /// unversioned bucket, is refused.
    fn pin_artifact(
        &self,
        pin: ArtifactPin,
    ) -> impl Future<Output = Result<String, ExecFailure>> + Send {
        async move {
            Err(ExecFailure::Permanent(format!(
                "the artifact of `{}` cannot be pinned by this executor; name each S3 object \
                 version as `s3://<bucket>/<key>?versionId=<version>`",
                pin.descriptor
            )))
        }
    }

    /// Pins `image`, an image named by tag in a registry other than ECR, to
    /// the digest its tag points at now, as `<image>@sha256:<digest>`. An
    /// executor that does not read such registries returns it unchanged.
    fn pin_registry_image(
        &self,
        descriptor: Descriptor,
        image: String,
    ) -> impl Future<Output = Result<String, ExecFailure>> + Send {
        let _ = descriptor;
        async { Ok(image) }
    }

    /// Checks that `images`, named by digest in registries other than ECR,
    /// still exist, before the deployment changes anything. An executor that
    /// cannot check accepts them, and a missing one fails when its workload
    /// deploys.
    fn check_registry_images(
        &self,
        descriptor: Descriptor,
        images: Vec<String>,
    ) -> impl Future<Output = Result<(), ExecFailure>> + Send {
        let _ = (descriptor, images);
        async { Ok(()) }
    }

    /// Pins `pin.image` to the digest its tag points at now, such as
    /// `<registry>/<repository>@sha256:<digest>`.
    fn pin_image(&self, pin: ImagePin) -> impl Future<Output = Result<String, ExecFailure>> + Send {
        async move {
            Err(ExecFailure::Permanent(format!(
                "image `{}` of `{}` cannot be pinned by this executor; name it by digest as \
                 `<image>@sha256:<digest>`",
                pin.image, pin.descriptor
            )))
        }
    }
}

/// Why artifacts could not be pinned.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PinError {
    /// An artifact has no fixed identity, or cannot be read; asking again
    /// does not help.
    #[error("{0}")]
    Refused(String),
    /// The cloud could not be asked; asking again later may work.
    #[error("{0}")]
    Unavailable(String),
}

/// An artifact named without a fixed identity, to pin before a deployment is
/// accepted.
#[derive(Debug, Clone)]
pub struct ArtifactPin {
    /// The deployment the workload belongs to.
    pub context: JobContext,
    /// The workload.
    pub descriptor: Descriptor,
    /// The definition it matched.
    pub definition_id: DefinitionId,
    /// The cloud account the workload's driver gets credentials for, which
    /// reads the artifact.
    pub account: AccountId,
    /// The artifact URI, such as `s3://bucket/key`.
    pub artifact: String,
}

/// An image named by a mutable tag, to pin to a digest before a deployment
/// is accepted.
#[derive(Debug, Clone)]
pub struct ImagePin {
    /// The deployment the workload belongs to.
    pub context: JobContext,
    /// The workload.
    pub descriptor: Descriptor,
    /// The definition it matched.
    pub definition_id: DefinitionId,
    /// The cloud account the workload's driver gets credentials for, which
    /// reads the image.
    pub account: AccountId,
    /// The image, such as `<account>.dkr.ecr.<region>.amazonaws.com/web:1`.
    pub image: String,
}

/// The artifacts a workload deploys, checked before its deployment changes
/// anything.
#[derive(Debug, Clone)]
pub struct ArtifactCheck {
    /// The deployment the workload belongs to.
    pub context: JobContext,
    /// The workload.
    pub descriptor: Descriptor,
    /// The definition it matched.
    pub definition_id: DefinitionId,
    /// The cloud account the workload's driver gets credentials for, which
    /// reads the artifacts.
    pub account: AccountId,
    /// The artifact URIs, such as `s3://bucket/key?versionId=v`, and the
    /// images pinned to digests in ECR.
    pub artifacts: Vec<String>,
}

/// What observing a resource found, with the cookie the driver returned
/// alongside, sealed, when it differs from the one it was given. Recovery
/// stores that cookie whatever it concludes, so the next call to the driver
/// carries the newest state.
#[derive(Debug, Clone)]
pub struct Observation {
    /// What the driver reported.
    pub observed: Observed,
    /// The newer cookie, if any.
    pub cookie: Option<Sealed>,
}

impl From<Observed> for Observation {
    fn from(observed: Observed) -> Self {
        Self {
            observed,
            cookie: None,
        }
    }
}

/// What observing a resource found.
#[derive(Debug, Clone)]
pub enum Observed {
    /// Nothing exists for the GUResID.
    Absent {
        /// Whether the driver guarantees that repeating the operation with
        /// the same identity converges, so that a creation still on its way
        /// cannot be duplicated.
        repeatable: bool,
    },
    /// The resource exists. After a creation, with its outputs, secret
    /// outputs already stored and cookie sealed, as after provisioning. The
    /// state of a resource that existed before says nothing about whether an
    /// update or deletion took effect, so its result is left empty.
    Ready {
        /// What the creation provisioned.
        result: Box<ProvisionResult>,
        /// Whether repeating the operation with the same identity is safe.
        repeatable: bool,
    },
    /// An operation is still running; observe again later.
    InProgress,
    /// The driver's last operation failed.
    Failed {
        /// The redacted error.
        error: String,
        /// Whether the driver guarantees that repeating the operation with
        /// the same identity converges instead of duplicating effects.
        repeatable: bool,
    },
    /// The driver cannot tell; an operator must establish the outcome.
    Unknown(String),
}

/// Planner limits.
#[derive(Debug, Clone)]
pub struct Limits {
    /// Driver operations at once, across all deployments.
    pub concurrency: usize,
    /// Attempts per resource, the first included.
    pub attempts: u32,
    /// The first retry delay, doubled for each further retry.
    pub backoff: Duration,
    /// The longest retry delay.
    pub max_backoff: Duration,
    /// How long one deployment may run.
    pub deadline: Duration,
    /// The most log text kept per deployment, in bytes; later lines are
    /// dropped after a line saying so.
    pub log_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            concurrency: 4,
            attempts: 3,
            backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(30),
            deadline: Duration::from_secs(30 * 60),
            log_bytes: 16 * 1024 * 1024,
        }
    }
}

/// How one resource ended in a deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Provisioned.
    Provisioned,
    /// Failed with a known outcome.
    Failed(String),
    /// The outcome is unknown; the resource needs recovery.
    Unknown(String),
    /// Never started.
    Pending(String),
    /// Deleted, because the deployment no longer includes it.
    Deleted,
    /// The driver confirmed the deletion, but the resource's record could not
    /// be removed; the next deployment finishes the cleanup. This fails the
    /// deployment.
    CleanupPending(String),
    /// Scheduled for deletion but kept, because a resource that stays still
    /// uses it. This does not fail the deployment.
    Kept(String),
}

/// The planner, shared by all deployments of a server.
pub struct Planner<S, E> {
    store: Arc<S>,
    executor: Arc<E>,
    limits: Limits,
    permits: Arc<Semaphore>,
    checks: Arc<dyn PermitCheck>,
}

impl<S, E> std::fmt::Debug for Planner<S, E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Planner")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

/// What a finished node contributes to the scopes of later nodes.
#[derive(Debug, Clone)]
struct Done {
    outputs: ResourceOutputs,
}

impl<S: Store + 'static, E: Executor> Planner<S, E> {
    /// A planner over `store` and `executor`.
    pub fn new(store: Arc<S>, executor: Arc<E>, limits: Limits) -> Self {
        let permits = Arc::new(Semaphore::new(limits.concurrency.max(1)));
        Self {
            store,
            executor,
            limits,
            permits,
            checks: Arc::new(NoPermits),
        }
    }

    /// Checks destructive actions against `checks` instead of refusing them.
    pub fn with_permit_check(mut self, checks: Arc<dyn PermitCheck>) -> Self {
        self.checks = checks;
        self
    }

    /// Checks every artifact the deployment's workloads name as an S3 object
    /// version, and every image they name as an ECR digest, with their
    /// definitions' cloud accounts, and every image they name by digest in
    /// another registry; why each workload with a missing one cannot deploy.
    /// What needs a cloud account a workload's definition does not name, or
    /// comes from resources not yet provisioned, is left to its driver.
    async fn check_artifacts(
        &self,
        deployment: &Deployment,
        deadline: Instant,
    ) -> BTreeMap<Descriptor, String> {
        let mut missing = BTreeMap::new();
        for node in deployment.graph.nodes.values() {
            // Images in other registries need no cloud account.
            let elsewhere: Vec<String> = images(node)
                .into_iter()
                .map(|(_, image)| image)
                .filter(|i| pinned_image(i) && !in_ecr(i))
                .collect();
            if !elsewhere.is_empty() {
                let what = format!("the images of `{}`", node.descriptor);
                if let Err(reason) = self
                    .check_with_retries(&what, deadline, || {
                        self.executor
                            .check_registry_images(node.descriptor.clone(), elsewhere.clone())
                    })
                    .await
                {
                    missing.insert(node.descriptor.clone(), reason);
                    continue;
                }
            }
            let artifacts: Vec<String> = artifacts(node)
                .into_iter()
                .filter(|a| S3Version::parse(a).is_some())
                .chain(
                    images(node)
                        .into_iter()
                        .map(|(_, image)| image)
                        .filter(|i| EcrDigest::parse(i).is_some()),
                )
                .collect();
            if artifacts.is_empty() {
                continue;
            }
            let Some((_, def)) = deployment.definitions.get(&node.definition) else {
                continue;
            };
            let Ok(Some(account)) =
                driver_account(def, &scope_for(deployment, node, &BTreeMap::new()))
            else {
                continue;
            };
            let check = ArtifactCheck {
                context: JobContext::of(deployment),
                descriptor: node.descriptor.clone(),
                definition_id: def.id.clone(),
                account,
                artifacts,
            };
            let what = format!("the artifacts of `{}`", node.descriptor);
            if let Err(reason) = self
                .check_with_retries(&what, deadline, || {
                    self.executor.check_artifacts(check.clone())
                })
                .await
            {
                missing.insert(node.descriptor.clone(), reason);
            }
        }
        missing
    }

    /// Pins every artifact the deployment's workloads name without a fixed
    /// identity: the pinned URI of each such workload, with shared values
    /// resolved. Artifacts already pinned to an S3 object version are kept;
    /// anything else that cannot be pinned, such as another kind of URI or a
    /// workload whose definition has no cloud account, is refused.
    pub async fn pin_artifacts(
        &self,
        deployment: &Deployment,
        timeout: Duration,
    ) -> Result<BTreeMap<Descriptor, String>, PinError> {
        let deadline = Instant::now() + timeout;
        let mut pinned = BTreeMap::new();
        for node in deployment.graph.nodes.values() {
            let Some(named) = artifacts(node).into_iter().next() else {
                continue;
            };
            let refused = |why: String| {
                PinError::Refused(format!("artifact `{named}` of `{}` {why}", node.descriptor))
            };
            let scope = scope_for(deployment, node, &BTreeMap::new());
            // Shared values are known now; outputs of other resources are not.
            let artifact = if named.contains("${") {
                resolve_value(
                    &Value::String(named.clone()),
                    Dialect::Set,
                    &scope,
                    Sink::Plain,
                    "artifact",
                )
                .ok()
                .and_then(|r| r.as_plain().and_then(Value::as_str).map(str::to_owned))
                .ok_or_else(|| {
                    refused(
                        "must be known before the deployment runs to be pinned: name it \
                         literally or through shared values"
                            .into(),
                    )
                })?
            } else {
                named.clone()
            };
            if S3Version::parse(&artifact).is_some() {
                if artifact != named {
                    pinned.insert(node.descriptor.clone(), artifact);
                }
                continue;
            }
            if artifact == "." {
                return Err(refused(
                    "was not supplied: pass `--artifact main=<uri>`".into(),
                ));
            }
            if S3Object::parse(&artifact).is_none() {
                return Err(refused(
                    "has no fixed identity: name an S3 object (`s3://<bucket>/<key>` in a \
                     versioned bucket, or with `?versionId=`)"
                        .into(),
                ));
            }
            let (def, account) = pin_account(
                deployment,
                node,
                &scope,
                &refused,
                "name the version with `?versionId=`",
            )?;
            let pin = ArtifactPin {
                context: JobContext::of(deployment),
                descriptor: node.descriptor.clone(),
                definition_id: def.id.clone(),
                account,
                artifact: artifact.clone(),
            };
            let version = self
                .with_retries("artifacts", &pin.descriptor, deadline, || {
                    self.executor.pin_artifact(pin.clone())
                })
                .await?;
            pinned.insert(node.descriptor.clone(), version);
        }
        Ok(pinned)
    }

    /// Pins every image the deployment's workloads name by tag to the digest
    /// the tag points at now: for each such workload, the pinned image by its
    /// JSON pointer in the workload's node, with shared values resolved. ECR
    /// images are read with the workload definition's cloud account, and
    /// images in other registries as the executor can. Images already pinned
    /// by digest are kept, and images built from other resources' outputs
    /// outside ECR are not pinned.
    pub async fn pin_images(
        &self,
        deployment: &Deployment,
        timeout: Duration,
    ) -> Result<BTreeMap<Descriptor, BTreeMap<String, String>>, PinError> {
        let deadline = Instant::now() + timeout;
        let mut pinned: BTreeMap<Descriptor, BTreeMap<String, String>> = BTreeMap::new();
        for node in deployment.graph.nodes.values() {
            for (pointer, named) in images(node) {
                let refused = |why: String| {
                    PinError::Refused(format!("image `{named}` of `{}` {why}", node.descriptor))
                };
                let scope = scope_for(deployment, node, &BTreeMap::new());
                // An image built from other resources' outputs is known only
                // when the deployment runs; only one in ECR must be pinned.
                let Some(image) = resolve_plain(&named, &scope) else {
                    if in_ecr(&named) {
                        return Err(refused(
                            "must be known before the deployment runs to be pinned: name it \
                             literally or through shared values"
                                .into(),
                        ));
                    }
                    continue;
                };
                if pinned_image(&image) {
                    // A tag kept before the digest marks an image pinned
                    // here from a public tag, whose digest must stay readable;
                    // one named by digest outside ECR drops its tag.
                    let image = match OciImage::parse(&image) {
                        Some(parsed) if !in_ecr(&image) && pinned_from_tag(&image) => {
                            parsed.to_string()
                        }
                        _ => image,
                    };
                    if image != named {
                        pinned
                            .entry(node.descriptor.clone())
                            .or_default()
                            .insert(pointer, image);
                    }
                    continue;
                }
                if image == "." {
                    return Err(refused(
                        "was not supplied: pass `--image <container>=<image>`".into(),
                    ));
                }
                if image.contains('@') {
                    return Err(refused(format!(
                        "names a digest that is not `sha256:` and 64 hex digits: `{image}`"
                    )));
                }
                // Only ECR images are read with the workload's account; images
                // in other registries are read without credentials.
                if EcrImage::parse(&image).is_none() {
                    let image = self
                        .with_retries("images", &node.descriptor, deadline, || {
                            self.executor
                                .pin_registry_image(node.descriptor.clone(), image.clone())
                        })
                        .await?;
                    if image != named {
                        pinned
                            .entry(node.descriptor.clone())
                            .or_default()
                            .insert(pointer, image);
                    }
                    continue;
                }
                let (def, account) = pin_account(
                    deployment,
                    node,
                    &scope,
                    &refused,
                    "name it by digest as `<image>@sha256:<digest>`",
                )?;
                let pin = ImagePin {
                    context: JobContext::of(deployment),
                    descriptor: node.descriptor.clone(),
                    definition_id: def.id.clone(),
                    account,
                    image,
                };
                let digest = self
                    .with_retries("images", &pin.descriptor, deadline, || {
                        self.executor.pin_image(pin.clone())
                    })
                    .await?;
                pinned
                    .entry(node.descriptor.clone())
                    .or_default()
                    .insert(pointer, digest);
            }
        }
        Ok(pinned)
    }

    /// Runs `call`, retrying failures that may pass with backoff, within the
    /// deployment's attempts and `deadline`.
    async fn with_retries<T, F, Fut>(
        &self,
        what: &str,
        descriptor: &Descriptor,
        deadline: Instant,
        call: F,
    ) -> Result<T, PinError>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<T, ExecFailure>>,
    {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let (error, retry_after) = match tokio::time::timeout_at(deadline, call()).await {
                Ok(Ok(v)) => return Ok(v),
                Ok(Err(ExecFailure::Permanent(e) | ExecFailure::Uncertain { error: e, .. })) => {
                    return Err(PinError::Refused(e));
                }
                Ok(Err(ExecFailure::Retryable { error, retry_after })) => (error, retry_after),
                Err(_) => {
                    return Err(PinError::Unavailable(format!(
                        "ran out of time pinning the {what} of `{descriptor}`"
                    )));
                }
            };
            let delay = retry_after.unwrap_or_else(|| backoff(&self.limits, attempt));
            if attempt >= self.limits.attempts || Instant::now() + delay >= deadline {
                return Err(PinError::Unavailable(format!(
                    "{error} (after {attempt} attempts)"
                )));
            }
            tokio::time::sleep(delay).await;
        }
    }

    /// Checks that the version of the drivers' code each resource of a
    /// replay ran can still run; why each resource whose version cannot
    /// cannot deploy.
    async fn check_pins(
        &self,
        deployment: &Deployment,
        deadline: Instant,
    ) -> BTreeMap<Descriptor, String> {
        let mut missing = BTreeMap::new();
        for node in deployment.graph.nodes.values() {
            let Some(pin) = deployment.pins.get(&node.descriptor.to_string()) else {
                continue;
            };
            let Some((version, def)) = deployment.definitions.get(&node.definition) else {
                continue;
            };
            let mut scope = scope_for(deployment, node, &BTreeMap::new());
            // A resource the replay finds removed gets a new identity only
            // when it provisions.
            let guresid = match self
                .store
                .active_resource(
                    deployment.org.as_str(),
                    deployment.app.as_str(),
                    deployment.env.as_str(),
                    &node.descriptor.to_string(),
                )
                .await
            {
                Ok(Some(active)) => Guresid::new(active.guresid).ok(),
                Ok(None) => None,
                Err(e) => {
                    missing.insert(
                        node.descriptor.clone(),
                        format!("the resource could not be read to check its pinned version: {e}"),
                    );
                    continue;
                }
            };
            if let Some(guresid) = &guresid {
                scope
                    .context
                    .insert(ContextKey::ResGuresid, json!(guresid.as_str()));
            }
            let recording = Recording {
                scope: &scope,
                seen: std::sync::Mutex::new(BTreeSet::new()),
            };
            // What is not known before the deployment runs, such as other
            // resources' outputs, is checked as unknown: a version that
            // depends on it, such as a source URL, cannot be checked and is
            // refused.
            let (values, secrets, secret_refs) =
                match driver_inputs(def, &Unknown(recording.scope), &Unknown(&recording)) {
                    Ok(inputs) => inputs,
                    Err(e) => {
                        missing.insert(node.descriptor.clone(), e);
                        continue;
                    }
                };
            let job = ProvisionJob {
                operation_id: format!("{}-check", deployment.id),
                attempt: 0,
                descriptor: node.descriptor.clone(),
                // The check changes nothing, so a removed resource needs no
                // identity.
                guresid: guresid.unwrap_or_else(|| Guresid::from_bytes([0; 16])),
                context: JobContext::of(deployment),
                definition_id: def.id.clone(),
                definition_version: *version,
                driver_type: def.driver_type.clone(),
                // Modules can be fetched with the cloud credentials, such as
                // from S3; an account that comes from another resource is
                // not known yet.
                driver_account: driver_account(def, &Unknown(&scope)).ok().flatten(),
                inputs: JobInputs {
                    values,
                    secrets,
                    secret_refs,
                    resource: Resolved::Value(Value::Null),
                    sources: recording.into_sources(),
                },
                input_fingerprint: String::new(),
                cookie: None,
                target: None,
                pin: Some(pin.clone()),
                existing: false,
                output_schema: None,
                deadline,
                log: JobLog::discard(),
            };
            let unknown = serde_json::to_string(&job.inputs).is_ok_and(|s| s.contains(UNKNOWN));
            let what = format!("the pinned version of `{}`", node.descriptor);
            if let Err(mut reason) = self
                .check_with_retries(&what, deadline, || self.executor.check_pin(job.clone()))
                .await
            {
                if unknown {
                    reason.push_str(
                        " (some of its driver inputs come from other resources, which are not \
                         known before the deployment runs; deploy the current configuration \
                         instead)",
                    );
                }
                missing.insert(node.descriptor.clone(), reason);
            }
        }
        missing
    }

    /// Checks that the version of the drivers' code each resource the
    /// deployment deletes was last applied with can still run; why each
    /// whose version cannot stops the deployment.
    async fn check_deletions(
        &self,
        deployment: &Deployment,
        deadline: Instant,
    ) -> BTreeMap<Descriptor, String> {
        let d = deployment;
        let mut missing = BTreeMap::new();
        // Without the inventory nothing can be checked, so nothing runs.
        let unlisted = |e: &dyn std::fmt::Display| -> BTreeMap<Descriptor, String> {
            let reason =
                format!("the resources the deployment deletes could not be listed to check: {e}");
            d.graph
                .nodes
                .keys()
                .map(|k| (k.clone(), reason.clone()))
                .collect()
        };
        let active = match self
            .store
            .active_resources(d.org.as_str(), d.app.as_str(), d.env.as_str())
            .await
        {
            Ok(active) => active,
            Err(e) => return unlisted(&e),
        };
        let (candidates, mandatory) = deletion_candidates(&d.graph, d.teardown, &active);
        if candidates.is_empty() {
            return missing;
        }
        let kept = if candidates
            .values()
            .all(|r| r.cleanup_pending || mandatory.contains(&r.descriptor))
        {
            BTreeSet::new()
        } else {
            match self
                .store
                .tombstones(d.org.as_str(), d.app.as_str(), d.env.as_str())
                .await
            {
                Ok(tombstones) => deprovision::kept_candidates(
                    &d.graph,
                    &active,
                    &tombstones,
                    &candidates,
                    &mandatory,
                ),
                Err(e) => return unlisted(&e),
            }
        };
        for (descriptor, r) in candidates {
            // A confirmed deletion only has its record left to remove, and a
            // kept resource is not deleted.
            if r.cleanup_pending || kept.contains(&r.descriptor) {
                continue;
            }
            let (recorded, guresid) =
                match (deprovision::Recorded::of(&r), r.guresid.parse::<Guresid>()) {
                    (Ok(recorded), Ok(guresid)) => (recorded, guresid),
                    (Err(e), _) => {
                        missing.insert(
                            descriptor,
                            format!("the deployment deletes this resource, but {e}"),
                        );
                        continue;
                    }
                    (_, Err(_)) => {
                        missing.insert(
                            descriptor,
                            "the deployment deletes this resource, but its recorded GUResID is \
                             invalid"
                                .into(),
                        );
                        continue;
                    }
                };
            let job = DeprovisionJob {
                job: ProvisionJob {
                    operation_id: format!("{}-check", d.id),
                    attempt: 0,
                    descriptor: descriptor.clone(),
                    guresid,
                    context: JobContext::of(d),
                    definition_id: recorded.definition_id,
                    definition_version: r.definition_version.unwrap_or_default(),
                    driver_type: recorded.driver_type,
                    driver_account: recorded.driver_account,
                    inputs: recorded.inputs,
                    input_fingerprint: "delete".into(),
                    cookie: r.cookie.clone(),
                    target: r.target.clone(),
                    pin: None,
                    existing: true,
                    output_schema: None,
                    deadline,
                    log: JobLog::discard(),
                },
                snapshot: r.snapshot.clone(),
                recovery: r.recovery.clone(),
            };
            let what = format!("the retained version of `{descriptor}`");
            if let Err(reason) = self
                .check_with_retries(&what, deadline, || {
                    self.executor.check_retained(job.clone())
                })
                .await
            {
                missing.insert(
                    descriptor,
                    format!(
                        "the deployment deletes this resource, but the version it was applied \
                         with, which the deletion runs, is not available: {reason}"
                    ),
                );
            }
        }
        missing
    }

    /// Runs `check`, retrying failures that may pass with backoff, within the
    /// deployment's attempts and deadline.
    async fn check_with_retries<F>(
        &self,
        what: &str,
        deadline: Instant,
        check: impl Fn() -> F,
    ) -> Result<(), String>
    where
        F: Future<Output = Result<(), ExecFailure>>,
    {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let checked = tokio::time::timeout_at(deadline, check()).await;
            let (error, retry_after) = match checked {
                Ok(Ok(())) => return Ok(()),
                Ok(Err(ExecFailure::Permanent(e) | ExecFailure::Uncertain { error: e, .. })) => {
                    return Err(e);
                }
                Ok(Err(ExecFailure::Retryable { error, retry_after })) => (error, retry_after),
                Err(_) => {
                    return Err(format!("the deployment ran out of time checking {what}"));
                }
            };
            let delay = retry_after.unwrap_or_else(|| backoff(&self.limits, attempt));
            if attempt >= self.limits.attempts || Instant::now() + delay >= deadline {
                return Err(format!("{error} (after {attempt} attempts)"));
            }
            tokio::time::sleep(delay).await;
        }
    }

    /// Runs `deployment` to the end and finishes its record: succeeded when
    /// every resource provisioned, failed otherwise. Returns each resource's
    /// outcome.
    pub async fn run(&self, deployment: Deployment) -> BTreeMap<Descriptor, Outcome> {
        let deadline = Instant::now() + self.limits.deadline;
        let deployment = Arc::new(deployment);
        let log = DeploymentLog::start(self.store.clone(), &deployment, self.limits.log_bytes);
        log.deployment().info(&format!(
            "deployment `{}` started with {} resources",
            deployment.id,
            deployment.graph.nodes.len()
        ));
        let mut outcomes: BTreeMap<Descriptor, Outcome> = BTreeMap::new();
        let mut done: BTreeMap<Descriptor, Done> = BTreeMap::new();
        let mut started: BTreeSet<Descriptor> = BTreeSet::new();
        let mut running = JoinSet::new();
        let mut tasks: BTreeMap<tokio::task::Id, (Descriptor, tracing::Span)> = BTreeMap::new();
        // Nothing runs unless every artifact the deployment names exists and,
        // in a replay, every version of the drivers' code it ran can run again.
        let mut missing = self.check_artifacts(&deployment, deadline).await;
        let artifacts_missing = !missing.is_empty();
        if !artifacts_missing {
            missing = self.check_pins(&deployment, deadline).await;
        }
        let pins_missing = !artifacts_missing && !missing.is_empty();
        if missing.is_empty() {
            missing = self.check_deletions(&deployment, deadline).await;
        }
        let why = if artifacts_missing {
            "not started: an artifact the deployment needs is missing"
        } else if pins_missing {
            "not started: a version of the code the deployment replays is missing"
        } else {
            "not started: a resource the deployment deletes cannot be deleted"
        };
        for (d, reason) in &missing {
            outcomes.insert(d.clone(), Outcome::Failed(reason.clone()));
        }
        for node in deployment.graph.nodes.values() {
            if !missing.is_empty() && !missing.contains_key(&node.descriptor) {
                outcomes.insert(node.descriptor.clone(), Outcome::Pending(why.into()));
            }
        }
        while missing.is_empty() {
            // Start every node whose dependencies provisioned, unless time is up.
            for node in deployment.graph.nodes.values() {
                if started.contains(&node.descriptor)
                    || !node.depends_on.iter().all(|d| done.contains_key(d))
                {
                    continue;
                }
                started.insert(node.descriptor.clone());
                if Instant::now() >= deadline {
                    continue;
                }
                let ctx = NodeRun {
                    store: self.store.clone(),
                    executor: self.executor.clone(),
                    permits: self.permits.clone(),
                    limits: self.limits.clone(),
                    deployment: deployment.clone(),
                    scope: scope_for(&deployment, node, &done),
                    dependencies: node
                        .depends_on
                        .iter()
                        .filter_map(|d| done.get(d).map(|x| (d.clone(), x.outputs.clone())))
                        .collect(),
                    deadline,
                    log: log.resource(&node.descriptor),
                };
                let d = node.descriptor.clone();
                let span = resource_span(&d);
                let task = running.spawn({
                    let d = d.clone();
                    async move { (d, ctx.run().await) }.instrument(span.clone())
                });
                tasks.insert(task.id(), (d, span));
            }
            let Some(joined) = running.join_next_with_id().await else {
                break;
            };
            let (d, result) = match joined {
                Ok((_, r)) => r,
                Err(e) => {
                    // The executor's own panics are caught; this is the planner's.
                    // Its operation, if one started, stays open for restart
                    // recovery, and the resource may have changed.
                    // A panic message could hold a secret, so it is not logged.
                    let Some((d, span)) = tasks.get(&e.id()).cloned() else {
                        continue;
                    };
                    // Logged in the resource's span, which names the operation.
                    span.in_scope(|| {
                        tracing::error!(
                            panicked = e.is_panic(),
                            "a provisioning task stopped unexpectedly"
                        );
                    });
                    (
                        d,
                        Err(Outcome::Unknown(
                            "provisioning stopped unexpectedly; recover the resource".into(),
                        )),
                    )
                }
            };
            match result {
                Ok(outputs) => {
                    outcomes.insert(d.clone(), Outcome::Provisioned);
                    done.insert(d, Done { outputs });
                }
                Err(outcome) => {
                    outcomes.insert(d, outcome);
                }
            }
        }

        // Whatever did not run waits behind a failed dependency or ran out of time.
        for node in deployment.graph.nodes.values() {
            if outcomes.contains_key(&node.descriptor) {
                continue;
            }
            let reason = match node
                .depends_on
                .iter()
                .find(|d| !matches!(outcomes.get(*d), Some(Outcome::Provisioned)))
            {
                Some(dep) => format!("waiting for `{dep}`"),
                None if Instant::now() >= deadline => "the deployment ran out of time".into(),
                None => "not started".into(),
            };
            outcomes.insert(node.descriptor.clone(), Outcome::Pending(reason));
        }
        // Resources the deployment leaves out are deleted only once everything
        // it includes provisioned.
        let converged = outcomes.values().all(|o| *o == Outcome::Provisioned);
        let listed = self
            .deprovision(&deployment, deadline, converged, &mut outcomes, &log)
            .await;

        // Every resource gets a record, also those that ended before an
        // operation was recorded; the store keeps any status an operation set.
        for (d, outcome) in &outcomes {
            // Terminal lines say what is known about the effects and what to
            // do next.
            let (status, reason, level, line) = match outcome {
                Outcome::Provisioned | Outcome::Deleted => continue,
                Outcome::Pending(r) => (
                    ResourceRunStatus::Pending,
                    r.clone(),
                    "warn",
                    format!("not attempted: {r}"),
                ),
                Outcome::Kept(r) => (
                    ResourceRunStatus::Skipped,
                    r.clone(),
                    "info",
                    format!("kept: {r}"),
                ),
                Outcome::Failed(r) => (
                    ResourceRunStatus::Failed,
                    r.clone(),
                    "error",
                    format!("failed, nothing changed; fix the cause and deploy again: {r}"),
                ),
                Outcome::CleanupPending(r) => (
                    ResourceRunStatus::Failed,
                    format!("cleanup pending: {r}"),
                    "error",
                    format!(
                        "deleted, but its record remains; the next deployment finishes the \
                         cleanup: {r}"
                    ),
                ),
                Outcome::Unknown(r) => (
                    ResourceRunStatus::Unknown,
                    r.clone(),
                    "error",
                    format!(
                        "outcome unknown, the resource may have changed; recover it before \
                         deploying again: {r}"
                    ),
                ),
            };
            log.resource(d).status(level, &line);
            if let Err(e) = self
                .store
                .record_resource_status(
                    deployment.org.as_str(),
                    deployment.app.as_str(),
                    deployment.env.as_str(),
                    &deployment.id,
                    &d.to_string(),
                    status,
                    Some(reason.clone()),
                )
                .await
            {
                tracing::error!(resource = %d, error = %e, "could not record a resource's status");
            }
        }

        let error = match (summary(&outcomes), listed) {
            (error, Ok(())) => error,
            (None, Err(e)) => Some(e),
            (Some(error), Err(e)) => Some(format!("{error}; {e}")),
        };
        // Every line is written before the deployment finishes, which adds
        // its outcome as the last line. A log with lines missing is reported,
        // not presented as complete.
        let error = with_lost_lines(error, log.close().await);
        // The outcome is logged once it is recorded, so the log never
        // contradicts the deployment and its audit entry.
        match self
            .store
            .finish_deployment(
                deployment.org.as_str(),
                deployment.app.as_str(),
                deployment.env.as_str(),
                &deployment.id,
                error.clone(),
            )
            .await
        {
            Err(e) => {
                tracing::error!(deployment = %deployment.id, error = %e, "could not finish a deployment");
            }
            Ok(()) => {
                crate::metrics::METRICS.deployment_finished(error.is_some());
                match &error {
                    None => tracing::info!(outcome = "succeeded", "deployment finished"),
                    Some(e) => {
                        tracing::warn!(outcome = "failed", error = %e, "deployment finished");
                    }
                }
            }
        }
        outcomes
    }
}

/// `error`, noting that `lost` log lines could not be stored.
fn with_lost_lines(error: Option<String>, lost: usize) -> Option<String> {
    match (error, lost) {
        (error, 0) => error,
        (None, n) => Some(format!("{n} log lines could not be stored")),
        (Some(e), n) => Some(format!("{e}; {n} log lines could not be stored")),
    }
}

/// Logs a failure the executor returned, already redacted, when another
/// attempt may follow; a final failure is logged with the resource's status at
/// the end of the run, or when its deletion ends.
fn log_retry(failure: &ExecFailure, log: &JobLog) {
    if let ExecFailure::Retryable { error, .. } = failure {
        log.warn(&format!("failed, retryable: {error}"));
    }
}

/// The deployment's error, or `None` when every resource provisioned.
fn summary(outcomes: &BTreeMap<Descriptor, Outcome>) -> Option<String> {
    let mut failed = Vec::new();
    let mut unknown = Vec::new();
    let mut pending = 0;
    for (d, o) in outcomes {
        match o {
            Outcome::Provisioned | Outcome::Deleted | Outcome::Kept(_) => {}
            Outcome::Failed(e) => failed.push(format!("`{d}` failed: {e}")),
            Outcome::CleanupPending(e) => {
                failed.push(format!("`{d}` failed: cleanup pending: {e}"));
            }
            Outcome::Unknown(e) => unknown.push(format!(
                "`{d}` has an unknown outcome and needs recovery: {e}"
            )),
            Outcome::Pending(_) => pending += 1,
        }
    }
    if failed.is_empty() && unknown.is_empty() && pending == 0 {
        return None;
    }
    let mut parts: Vec<String> = unknown.into_iter().chain(failed).collect();
    if pending > 0 {
        parts.push(format!("{pending} resource(s) not started"));
    }
    Some(parts.join("; "))
}

/// The placeholder scope of `node`: context, the outputs of finished
/// resources, its workload's dependencies and shared resources, and values.
/// The cloud account `def` gets credentials for, resolved in `scope`.
fn driver_account(
    def: &ResourceDefinition,
    scope: &dyn Scope,
) -> Result<Option<AccountId>, String> {
    let at = |what: &str| format!("definition `{}`: {what}", def.id);
    let Some(account) = &def.driver_account else {
        return Ok(None);
    };
    resolve_value(
        &json!(account),
        Dialect::DriverAccount,
        scope,
        Sink::Plain,
        "driver_account",
    )
    .map_err(|e| at(&e.to_string()))?
    .as_plain()
    .and_then(Value::as_str)
    .ok_or_else(|| at("driver_account must resolve to a string"))?
    .parse::<AccountId>()
    .map(Some)
    .map_err(|e| at(&format!("driver_account: {e}")))
}

/// The artifacts a workload node deploys, as its function package names them.
fn artifacts(node: &Node) -> Vec<String> {
    if node.descriptor.ty.as_str() != "workload" {
        return Vec::new();
    }
    node.params
        .as_ref()
        .and_then(|p| p.pointer("/spec/function/package/artifact"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .into_iter()
        .collect()
}

/// The images a workload deploys, by their JSON pointer in its node: each
/// container's, and an image function's.
fn images(node: &Node) -> Vec<(String, String)> {
    if node.descriptor.ty.as_str() != "workload" {
        return Vec::new();
    }
    let Some(spec) = node.params.as_ref().and_then(|p| p.get("spec")) else {
        return Vec::new();
    };
    let escape = |s: &str| s.replace('~', "~0").replace('/', "~1");
    let mut out: Vec<(String, String)> = spec["containers"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(id, c)| {
            let image = c["image"].as_str()?;
            Some((
                format!("/params/spec/containers/{}/image", escape(id)),
                image.to_owned(),
            ))
        })
        .collect();
    if let Some(image) = spec
        .pointer("/function/package/image")
        .and_then(Value::as_str)
    {
        out.push((
            "/params/spec/function/package/image".into(),
            image.to_owned(),
        ));
    }
    out
}

/// `named` with shared values resolved, when it can be resolved before the
/// deployment runs.
fn resolve_plain(named: &str, scope: &MapScope) -> Option<String> {
    if !named.contains("${") {
        return Some(named.to_owned());
    }
    resolve_value(
        &Value::String(named.to_owned()),
        Dialect::Set,
        scope,
        Sink::Plain,
        "image",
    )
    .ok()
    .and_then(|r| r.as_plain().and_then(Value::as_str).map(str::to_owned))
}

/// The definition `node` matched and the cloud account it reads with, which
/// must be known before the deployment runs. The server's own identity is
/// never used. `pinned_form` says how to name the input pinned instead.
fn pin_account<'d>(
    deployment: &'d Deployment,
    node: &Node,
    scope: &MapScope,
    refused: &dyn Fn(String) -> PinError,
    pinned_form: &str,
) -> Result<(&'d ResourceDefinition, AccountId), PinError> {
    let def = deployment
        .definitions
        .get(&node.definition)
        .map(|(_, def)| def)
        .ok_or_else(|| {
            refused(format!(
                "cannot be pinned: definition `{}` is not in the deployment",
                node.definition
            ))
        })?;
    match driver_account(def, scope) {
        Ok(Some(account)) => Ok((def, account)),
        Ok(None) => Err(refused(format!(
            "cannot be pinned: definition `{}` has no cloud account to read it with; give it \
             one, or {pinned_form}",
            def.id
        ))),
        Err(_) => Err(refused(format!(
            "cannot be pinned: the cloud account of definition `{}` comes from another \
             resource, which is not known before the deployment runs; {pinned_form}",
            def.id
        ))),
    }
}

fn scope_for(deployment: &Deployment, node: &Node, done: &BTreeMap<Descriptor, Done>) -> MapScope {
    let mut scope = MapScope {
        referrer: Some(node.descriptor.clone()),
        implicit_types: builtin_types()
            .into_iter()
            .filter(|t| t.use_ == ResourceUse::Implicit)
            .map(|t| t.id)
            .collect::<Vec<ResourceTypeId>>(),
        values: deployment.values.clone(),
        ..MapScope::default()
    };
    let d = &node.descriptor;
    for (key, value) in [
        (ContextKey::OrgId, deployment.org.as_str()),
        (ContextKey::AppId, deployment.app.as_str()),
        (ContextKey::EnvId, deployment.env.as_str()),
        (ContextKey::EnvType, deployment.env_type.as_str()),
        (ContextKey::ResType, d.ty.as_str()),
        (ContextKey::ResClass, d.class.as_str()),
        (ContextKey::ResId, d.id.as_str()),
        (ContextKey::DeployId, deployment.id.as_str()),
        (ContextKey::DeploySetId, deployment.set_id.as_str()),
    ] {
        scope.context.insert(key, json!(value));
    }
    for (descriptor, finished) in done {
        scope
            .resources
            .insert(descriptor.clone(), finished.outputs.clone());
    }
    // A workload sees its own dependencies as `externals`, and everyone sees
    // shared resources under `shared`.
    let own: Vec<&str> = node
        .requests
        .iter()
        .filter(|r| r.path.starts_with("modules.") && !r.path.contains(".externals."))
        .map(|r| r.path.as_str())
        .collect();
    let mut keyed = BTreeSet::new();
    for other in deployment.graph.nodes.values() {
        let Some(finished) = done.get(&other.descriptor) else {
            continue;
        };
        for r in &other.requests {
            if let Some(id) = r.path.strip_prefix("shared.") {
                scope.shared.insert(id.to_owned(), finished.outputs.clone());
            }
            for w in &own {
                if let Some(key) = r
                    .path
                    .strip_prefix(*w)
                    .and_then(|rest| rest.strip_prefix(".externals."))
                {
                    scope
                        .externals
                        .insert(key.to_owned(), finished.outputs.clone());
                    keyed.insert(other.descriptor.clone());
                }
            }
        }
    }
    // A resource co-provisioned with one of the workload's dependencies and
    // `match_dependents`, such as a policy for its table, is a dependency of
    // the workload too, keyed by descriptor. Nothing else the workload
    // depends on is added, so no other resource's secrets reach its job.
    if !own.is_empty() {
        for dep in &node.depends_on {
            if keyed.contains(dep) {
                continue;
            }
            let Some(other) = deployment.graph.nodes.get(dep) else {
                continue;
            };
            let coprovisioned = other.requests.iter().any(|r| {
                coprovisioned_by(&r.path)
                    .is_some_and(|source| node.depends_on.iter().any(|d| d.to_string() == source))
            });
            if !coprovisioned {
                continue;
            }
            if let Some(finished) = done.get(dep) {
                scope
                    .externals
                    .insert(dep.to_string(), finished.outputs.clone());
            }
        }
    }
    scope
}

/// The definition's driver inputs resolved in `recording`'s scope: values,
/// secrets and secret references.
fn driver_inputs(
    def: &ResourceDefinition,
    plain: &dyn Scope,
    recording: &dyn Scope,
) -> Result<(Value, Resolved, Resolved), String> {
    let at = |what: &str| format!("definition `{}`: {what}", def.id);
    let values = resolve_value(
        &Value::Object(def.driver_inputs.values.clone()),
        Dialect::Definition,
        plain,
        Sink::Plain,
        "driver_inputs.values",
    )
    .map_err(|e| at(&e.to_string()))?
    .as_plain()
    .cloned()
    .ok_or_else(|| at("driver_inputs.values contains a secret"))?;
    let secrets = resolve_value(
        &Value::Object(def.driver_inputs.secrets.clone()),
        Dialect::Definition,
        recording,
        Sink::Secret,
        "driver_inputs.secrets",
    )
    .map_err(|e| at(&e.to_string()))?;
    let secret_refs = references(&Value::Object(def.driver_inputs.secret_refs.clone()));
    Ok((values, secrets, secret_refs))
}

/// What a placeholder not known before the deployment runs resolves to when a
/// replay's pinned versions are checked.
const UNKNOWN: &str = "(not known before the deployment runs)";

/// A scope that resolves every placeholder its inner scope cannot to
/// [`UNKNOWN`].
struct Unknown<'a, S: ?Sized>(&'a S);

impl<S: Scope + ?Sized> Scope for Unknown<'_, S> {
    fn lookup(&self, expr: &Expr) -> Result<Lookup, String> {
        Ok(self
            .0
            .lookup(expr)
            .unwrap_or_else(|_| Lookup::Value(json!(UNKNOWN))))
    }
}

/// A scope that records where each secret reference it hands out came from.
struct Recording<'a> {
    scope: &'a MapScope,
    seen: std::sync::Mutex<BTreeSet<(SecretRef, SecretSource)>>,
}

impl Recording<'_> {
    fn into_sources(self) -> Vec<(SecretRef, SecretSource)> {
        self.seen
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .into_iter()
            .collect()
    }

    /// The resource whose outputs `expr` reads.
    fn output_of(&self, expr: &Expr) -> Option<Guresid> {
        let outputs = match expr {
            Expr::Resource { target, .. } => {
                let d = self.scope.descriptor_for(target).ok()?;
                self.scope.resources.get(&d)
            }
            Expr::Externals { key, .. } => self.scope.externals.get(key),
            Expr::Shared { id, .. } => self.scope.shared.get(id),
            _ => None,
        };
        outputs.and_then(|o| o.guresid.clone())
    }
}

impl Scope for Recording<'_> {
    fn lookup(&self, expr: &Expr) -> Result<Lookup, String> {
        let found = self.scope.lookup(expr)?;
        if let Lookup::Secret(r) = &found {
            // A reference whose source is unknown is left out, and the
            // executor refuses it.
            let source = match expr {
                Expr::Values { .. } => Some(SecretSource::Values),
                _ => self
                    .output_of(expr)
                    .map(|guresid| SecretSource::Output { guresid }),
            };
            if let Some(source) = source {
                self.seen
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert((r.clone(), source));
            }
        }
        Ok(found)
    }
}

/// One node's provisioning, with retries.
struct NodeRun<S, E> {
    store: Arc<S>,
    executor: Arc<E>,
    permits: Arc<Semaphore>,
    limits: Limits,
    deployment: Arc<Deployment>,
    scope: MapScope,
    dependencies: BTreeMap<Descriptor, ResourceOutputs>,
    deadline: Instant,
    log: JobLog,
}

impl<S: Store + 'static, E: Executor> NodeRun<S, E> {
    async fn run(mut self) -> Result<ResourceOutputs, Outcome> {
        let deployment = self.deployment.clone();
        let node = deployment
            .graph
            .nodes
            .get(
                self.scope
                    .referrer
                    .as_ref()
                    .ok_or_else(|| Outcome::Failed("the resource has no descriptor".into()))?,
            )
            .ok_or_else(|| Outcome::Failed("the resource is not in the graph".into()))?;
        let (version, def) = deployment
            .definitions
            .get(&node.definition)
            .ok_or_else(|| {
                Outcome::Failed(format!(
                    "definition `{}` is missing from the deployment's snapshot",
                    node.definition
                ))
            })?;
        let fingerprint = fingerprint(node, *version, def, &self.dependencies);
        let mut attempt = 0;
        loop {
            attempt += 1;
            let permit =
                tokio::time::timeout_at(self.deadline, self.permits.clone().acquire_owned()).await;
            let permit = match permit {
                Ok(Ok(permit)) => permit,
                Ok(Err(_)) => return Err(Outcome::Failed("the planner is shutting down".into())),
                Err(_) => return Err(Outcome::Pending("the deployment ran out of time".into())),
            };
            if Instant::now() >= self.deadline {
                return Err(Outcome::Pending("the deployment ran out of time".into()));
            }
            let started = self
                .store
                .begin_operation(OperationStart {
                    org: deployment.org.to_string(),
                    app: deployment.app.to_string(),
                    env: deployment.env.to_string(),
                    deployment_id: deployment.id.clone(),
                    descriptor: node.descriptor.clone(),
                    definition_id: Some(def.id.to_string()),
                    definition_version: Some(*version),
                    driver_type: Some(def.driver_type.clone()),
                    kind: OperationKind::Put,
                    input_fingerprint: fingerprint.clone(),
                    recovery: json!({
                        "definition_id": def.id,
                        "definition_version": version,
                        "driver_type": def.driver_type,
                    }),
                })
                .await
                .map_err(|e| match e {
                    StoreError::RecoveryRequired(_) => Outcome::Failed(format!(
                        "an earlier operation's outcome is unknown; recover the resource first ({e})"
                    )),
                    StoreError::MigrationRequired { .. } | StoreError::Detached(_) => {
                        Outcome::Failed(e.to_string())
                    }
                    other => Outcome::Failed(format!("the operation was not recorded: {other}")),
                })?;
            record_operation(&started);
            self.scope
                .context
                .insert(ContextKey::ResGuresid, json!(started.guresid.as_str()));
            let log = self
                .log
                .operation(&started.operation_id, started.guresid.as_str());
            log.info(&format!(
                "provisioning with `{}` (attempt {attempt})",
                def.driver_type
            ));
            let job = match self.job(node, *version, def, &fingerprint, &started, &log) {
                Ok(job) => job,
                Err(error) => {
                    log.error(&error);
                    self.checkpoint(
                        &started.operation_id,
                        CheckpointResult::Failed {
                            error: error.clone(),
                        },
                    )
                    .await;
                    return Err(Outcome::Failed(error));
                }
            };
            let account = job.driver_account.clone();
            let inputs = serde_json::to_value(&job.inputs).unwrap_or(Value::Null);
            // The resource's prior state is read before the inputs are
            // recorded, so the record says whether the resource existed
            // before this operation: recovery must not take a cookie that
            // this operation left behind for an earlier provisioning.
            let result = match self.with_prior_state(job, node).await {
                Err(failure) => Err(failure),
                Ok(job) => {
                    let recorded = self
                        .store
                        .record_operation_inputs(
                            &started.operation_id,
                            json!({
                                "definition_id": def.id,
                                "definition_version": version,
                                "definition": def,
                                "driver_type": def.driver_type,
                                "driver_account": job.driver_account,
                                "guresid": job.guresid,
                                "inputs": job.inputs,
                                "dependencies": self.dependencies,
                                "existing": job.existing,
                            }),
                        )
                        .await;
                    match recorded {
                        Err(e) => Err(ExecFailure::Retryable {
                            error: format!("the operation's inputs were not recorded: {e}"),
                            retry_after: None,
                        }),
                        Ok(()) => self.provision(job).await,
                    }
                }
            };
            drop(permit);
            let failure = match result {
                Ok(provisioned) => {
                    let outputs = ResourceOutputs {
                        values: provisioned.outputs.clone(),
                        secrets: provisioned.secret_outputs.clone(),
                        guresid: Some(started.guresid.clone()),
                        account: account.clone(),
                    };
                    let recorded = self
                        .checkpoint(
                            &started.operation_id,
                            CheckpointResult::Provisioned(Box::new(Provisioned {
                                outputs: Value::Object(provisioned.outputs),
                                secret_outputs: provisioned.secret_outputs,
                                cookie: provisioned.cookie,
                                recovery: provisioned.recovery,
                                inputs: inputs.clone(),
                                dependencies: node
                                    .depends_on
                                    .iter()
                                    .map(ToString::to_string)
                                    .collect(),
                                account: account.as_ref().map(ToString::to_string),
                                target: provisioned.target,
                                backend: provisioned.backend,
                                external_ids: provisioned.external_ids,
                                pin: provisioned.pin,
                                snapshot: json!({
                                    "deployment_id": deployment.id,
                                    "definition_version": version,
                                    "definition": def,
                                }),
                            })),
                        )
                        .await;
                    if recorded {
                        log.status("info", "provisioned");
                        return Ok(outputs);
                    }
                    let error = "the resource provisioned, but its result could not be recorded";
                    log.error(error);
                    self.checkpoint(
                        &started.operation_id,
                        CheckpointResult::Unknown {
                            error: error.into(),
                            cookie: None,
                        },
                    )
                    .await;
                    return Err(Outcome::Unknown(error.into()));
                }
                Err(f) => f,
            };
            log_retry(&failure, &log);
            match failure {
                ExecFailure::Permanent(error) => {
                    self.checkpoint(
                        &started.operation_id,
                        CheckpointResult::Failed {
                            error: error.clone(),
                        },
                    )
                    .await;
                    return Err(Outcome::Failed(error));
                }
                ExecFailure::Uncertain { error, cookie } => {
                    // The driver's newest cookie is kept, so recovery
                    // observes with it.
                    self.checkpoint(
                        &started.operation_id,
                        CheckpointResult::Unknown {
                            error: error.clone(),
                            cookie,
                        },
                    )
                    .await;
                    return Err(Outcome::Unknown(error));
                }
                ExecFailure::Retryable { error, retry_after } => {
                    self.checkpoint(
                        &started.operation_id,
                        CheckpointResult::Failed {
                            error: error.clone(),
                        },
                    )
                    .await;
                    if attempt >= self.limits.attempts {
                        return Err(Outcome::Failed(format!(
                            "{error} (after {attempt} attempts)"
                        )));
                    }
                    let delay = retry_after.unwrap_or_else(|| backoff(&self.limits, attempt));
                    if Instant::now() + delay >= self.deadline {
                        return Err(Outcome::Failed(format!(
                            "{error} (no time left for another attempt)"
                        )));
                    }
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }

    /// Resolves the node's inputs into a job.
    fn job(
        &self,
        node: &Node,
        version: i64,
        def: &ResourceDefinition,
        fingerprint: &str,
        started: &openporch_store::OperationStarted,
        log: &JobLog,
    ) -> Result<ProvisionJob, String> {
        let scope = &self.scope;
        // Secret inputs record where each reference came from.
        let recording = Recording {
            scope,
            seen: std::sync::Mutex::new(BTreeSet::new()),
        };
        let (values, secrets, secret_refs) = driver_inputs(def, scope, &recording)?;
        let driver_account = driver_account(def, scope)?;
        // Params from a set use the set's placeholders; co-provisioning params
        // use the definition's.
        let dialect = if node.requests.iter().any(|r| {
            r.params.is_some() && (r.path.starts_with("modules.") || r.path.starts_with("shared."))
        }) {
            Dialect::Set
        } else {
            Dialect::Definition
        };
        let mut params = node.params.clone().unwrap_or(Value::Null);
        let mut literal: Vec<(Vec<String>, Value)> = Vec::new();
        if node.descriptor.ty.as_str() == "workload" {
            add_dependency_outputs(&mut params, scope);
            if let Some(spec) = params.get_mut("spec") {
                add_trigger_outputs(spec, scope);
                literal = set_aside_no_expand(spec)
                    .into_iter()
                    .map(|(path, content)| {
                        (
                            std::iter::once("spec".to_owned()).chain(path).collect(),
                            content,
                        )
                    })
                    .collect();
            }
        }
        let mut resource = resolve_value(&params, dialect, &recording, Sink::Secret, "params")
            .map_err(|e| format!("`{}`: {e}", node.descriptor))?;
        for (path, content) in literal {
            put(&mut resource, &path, content);
        }
        Ok(ProvisionJob {
            operation_id: started.operation_id.clone(),
            attempt: started.attempt,
            descriptor: node.descriptor.clone(),
            guresid: started.guresid.clone(),
            context: JobContext::of(&self.deployment),
            definition_id: def.id.clone(),
            definition_version: version,
            driver_type: def.driver_type.clone(),
            driver_account,
            inputs: JobInputs {
                values,
                secrets,
                secret_refs,
                resource,
                sources: recording.into_sources(),
            },
            input_fingerprint: fingerprint.to_owned(),
            cookie: None,
            target: None,
            pin: self
                .deployment
                .pins
                .get(&node.descriptor.to_string())
                .cloned(),
            existing: false,
            output_schema: self.deployment.output_schema(&node.descriptor.ty),
            deadline: self.deadline,
            log: log.clone(),
        })
    }

    /// Runs the executor until the deadline. A panic or the deadline leaves the
    /// outcome uncertain, since the driver may have changed something.
    async fn provision(&self, job: ProvisionJob) -> Result<ProvisionResult, ExecFailure> {
        let span = operation_span(&job, "provision");
        let (started, _running) = (Instant::now(), crate::metrics::METRICS.driver_started());
        let result = self.provision_in_span(job).instrument(span.clone()).await;
        span.in_scope(|| log_outcome("provision", started, &result));
        result
    }

    async fn provision_in_span(&self, job: ProvisionJob) -> Result<ProvisionResult, ExecFailure> {
        let executor = self.executor.clone();
        let task = tokio::spawn(async move { executor.provision(job).await }.in_current_span());
        let abort = task.abort_handle();
        match tokio::time::timeout_at(self.deadline, task).await {
            Ok(Ok(result)) => result,
            Ok(Err(e)) => {
                // A panic message could hold a secret, so it is not logged.
                tracing::error!(panicked = e.is_panic(), "the executor stopped unexpectedly");
                Err(ExecFailure::uncertain(
                    "the executor stopped unexpectedly while the resource was provisioning",
                ))
            }
            Err(_) => {
                abort.abort();
                Err(ExecFailure::uncertain(
                    "the deployment ran out of time while the resource was provisioning",
                ))
            }
        }
    }

    /// Adds the cookie of the resource's last successful operation, and refuses
    /// to move a provisioned resource to another cloud account.
    async fn with_prior_state(
        &self,
        mut job: ProvisionJob,
        node: &Node,
    ) -> Result<ProvisionJob, ExecFailure> {
        let d = &self.deployment;
        let resource = self
            .store
            .active_resource(
                d.org.as_str(),
                d.app.as_str(),
                d.env.as_str(),
                &node.descriptor.to_string(),
            )
            .await
            // Without the cookie the driver might create a duplicate.
            .map_err(|e| ExecFailure::Retryable {
                error: format!("the resource's driver state could not be read: {e}"),
                retry_after: None,
            })?;
        let Some(resource) = resource else {
            return Ok(job);
        };
        let account = job.driver_account.as_ref().map(ToString::to_string);
        if resource.last_deploy_id.is_some() && resource.driver_account != account {
            let name = |a: &Option<String>| match a {
                Some(a) => format!("cloud account `{a}`"),
                None => "no cloud account".to_owned(),
            };
            return Err(ExecFailure::Permanent(format!(
                "`{}` was provisioned in {}, but its definition now uses {}; migrate or \
                 replace the resource explicitly",
                node.descriptor,
                name(&resource.driver_account),
                name(&account),
            )));
        }
        // A failed first attempt that left driver state behind counts too.
        job.existing = resource.last_deploy_id.is_some()
            || resource.cookie.is_some()
            || resource.target.is_some();
        job.cookie = resource.cookie;
        job.target = resource.target;
        Ok(job)
    }

    /// Records an operation's outcome; whether it was recorded.
    async fn checkpoint(&self, operation_id: &str, result: CheckpointResult) -> bool {
        match self.store.checkpoint_operation(operation_id, result).await {
            Ok(()) => true,
            Err(e) => {
                tracing::error!(operation = operation_id, error = %e, "could not checkpoint an operation");
                false
            }
        }
    }
}

/// The delay before attempt `attempt + 1`: exponential from the base, capped,
/// with jitter in its upper half.
fn backoff(limits: &Limits, attempt: u32) -> Duration {
    let exp = limits
        .backoff
        .saturating_mul(1 << attempt.saturating_sub(1).min(16))
        .min(limits.max_backoff);
    exp.mul_f64(0.5 + rand::random::<f64>() * 0.5)
}

/// Secret references in `value`, which mirrors the structure of plain inputs,
/// as secret expressions. Anything that is not a reference stays plain.
fn references(value: &Value) -> Resolved {
    match value {
        Value::Object(map) => {
            if let Ok(r) = serde_json::from_value::<SecretRef>(value.clone()) {
                return Resolved::Secret(SecretExpr::whole(r));
            }
            let items: Vec<(String, Resolved)> = map
                .iter()
                .map(|(k, v)| (k.clone(), references(v)))
                .collect();
            if items.iter().all(|(_, r)| !r.is_tainted()) {
                Resolved::Value(value.clone())
            } else {
                Resolved::Object(items)
            }
        }
        Value::Array(items) => {
            let items: Vec<Resolved> = items.iter().map(references).collect();
            if items.iter().all(|r| !r.is_tainted()) {
                Resolved::Value(value.clone())
            } else {
                Resolved::Array(items)
            }
        }
        other => Resolved::Value(other.clone()),
    }
}

/// Placeholders for the outputs of the workload dependency `key`, by name,
/// resolved with the rest of the inputs so secret outputs stay references.
/// Only names a placeholder path holds plainly are passed.
fn output_placeholders(key: &str, outputs: &ResourceOutputs) -> Value {
    outputs
        .values
        .keys()
        .chain(outputs.secrets.keys())
        .filter(|n| is_name(n))
        .map(|name| {
            let placeholder = if is_name(key) {
                format!("${{externals.{key}.{name}}}")
            } else {
                format!("${{externals['{key}'].{name}}}")
            };
            (name.clone(), json!(placeholder))
        })
        .collect::<serde_json::Map<_, _>>()
        .into()
}

/// The resource that co-provisioned a node, from the path of the request the
/// graph records for it: ``definition `<id>` provision.<key> for `<source>` ``.
fn coprovisioned_by(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("definition `")?;
    let (_, rest) = rest.split_once("` provision.")?;
    let (_, source) = rest.rsplit_once(" for `")?;
    source.strip_suffix('`')
}

/// Whether `s` can follow a dot in a placeholder.
fn is_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Gives a workload's inputs its direct dependencies' outputs by key.
fn add_dependency_outputs(params: &mut Value, scope: &MapScope) {
    let Some(params) = params.as_object_mut() else {
        return;
    };
    let dependencies = scope
        .externals
        .iter()
        .map(|(key, outputs)| (key.clone(), output_placeholders(key, outputs)))
        .collect::<serde_json::Map<_, _>>();
    params.insert("dependencies".into(), dependencies.into());
}

/// Gives each trigger of a workload spec that names one of its resources
/// that resource's outputs under `outputs`.
fn add_trigger_outputs(spec: &mut Value, scope: &MapScope) {
    let Some(triggers) = spec.get_mut("triggers").and_then(Value::as_object_mut) else {
        return;
    };
    // Each trigger is `{<kind>: {...}}`.
    for trigger in triggers.values_mut().filter_map(Value::as_object_mut) {
        for body in trigger.values_mut().filter_map(Value::as_object_mut) {
            let Some(key) = body.get("resource").and_then(Value::as_str) else {
                continue;
            };
            let Some(outputs) = scope.externals.get(key) else {
                continue;
            };
            let outputs = output_placeholders(key, outputs);
            body.insert("outputs".into(), outputs);
        }
    }
}

/// Takes the content of files marked `noExpand` out of a workload spec, so
/// that it is passed on literally rather than resolved, with the path of each.
fn set_aside_no_expand(spec: &mut Value) -> Vec<(Vec<String>, Value)> {
    let mut out = Vec::new();
    let Some(containers) = spec.get_mut("containers").and_then(Value::as_object_mut) else {
        return out;
    };
    for (id, container) in containers.iter_mut() {
        let Some(files) = container.get_mut("files").and_then(Value::as_object_mut) else {
            continue;
        };
        for (target, file) in files.iter_mut() {
            let Some(file) = file.as_object_mut() else {
                continue;
            };
            if file.get("noExpand") != Some(&Value::Bool(true)) {
                continue;
            }
            if let Some(content) = file.get_mut("content") {
                let path = ["containers", id, "files", target, "content"].map(str::to_owned);
                out.push((path.to_vec(), content.take()));
            }
        }
    }
    out
}

/// Puts `value` back at `path` in `resolved`, where it was set aside.
fn put(resolved: &mut Resolved, path: &[String], value: Value) {
    let Some((first, rest)) = path.split_first() else {
        *resolved = Resolved::Value(value);
        return;
    };
    match resolved {
        Resolved::Value(v) => {
            if let Some(at) = path.iter().try_fold(v, |at, key| at.get_mut(key.as_str())) {
                *at = value;
            }
        }
        Resolved::Object(fields) => {
            if let Some((_, r)) = fields.iter_mut().find(|(k, _)| k == first) {
                put(r, rest, value);
            }
        }
        Resolved::Array(_) | Resolved::Secret(_) => {}
    }
}

/// A digest of everything a resource's inputs derive from: its definition and
/// params, and its dependencies' outputs (secret ones as references).
fn fingerprint(
    node: &Node,
    version: i64,
    def: &ResourceDefinition,
    dependencies: &BTreeMap<Descriptor, ResourceOutputs>,
) -> String {
    let doc = json!({
        "descriptor": node.descriptor,
        "definition": def,
        "definition_version": version,
        "params": node.params,
        "dependencies": dependencies,
    });
    let digest = Sha256::digest(to_canonical_string(&doc).as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// The built-in type `ty`, if it is one.
pub(crate) fn builtin_type(ty: &ResourceTypeId) -> Option<ResourceType> {
    builtin_types().into_iter().find(|t| &t.id == ty)
}
