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

use openporch_core::canonical::to_canonical_string;
use openporch_core::catalog::{ResourceUse, builtin_types};
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

mod deprovision;
mod log;
#[cfg(test)]
mod tests;

pub use deprovision::{DestructiveAction, NoPermits, PermitCheck, PermitRequest};
use log::DeploymentLog;
pub use log::{JobLog, RedactingLog};

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
        Ok(Self {
            org,
            app,
            env,
            env_type,
            id,
            set_id,
            graph,
            definitions,
            values: BTreeMap::new(),
            permits: snapshot["permits"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|p| p.as_str().map(str::to_owned))
                .collect(),
            teardown: false,
        })
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
    /// Whether the resource provisioned before. A change to an existing
    /// resource whose impact the driver cannot preview needs a permit; a
    /// first creation destroys nothing and does not.
    pub existing: bool,
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
    Uncertain(String),
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
        let mut tasks: BTreeMap<tokio::task::Id, Descriptor> = BTreeMap::new();
        loop {
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
                let task = running.spawn({
                    let d = d.clone();
                    async move { (d, ctx.run().await) }
                });
                tasks.insert(task.id(), d);
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
                    tracing::error!(
                        panicked = e.is_panic(),
                        "a provisioning task stopped unexpectedly"
                    );
                    let Some(d) = tasks.get(&e.id()).cloned() else {
                        continue;
                    };
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
        if let Err(e) = self
            .store
            .finish_deployment(
                deployment.org.as_str(),
                deployment.app.as_str(),
                deployment.env.as_str(),
                &deployment.id,
                error,
            )
            .await
        {
            tracing::error!(deployment = %deployment.id, error = %e, "could not finish a deployment");
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
                }
            }
        }
    }
    scope
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
            let recorded = self
                .store
                .record_operation_inputs(
                    &started.operation_id,
                    json!({
                        "definition_id": def.id,
                        "definition_version": version,
                        "driver_type": def.driver_type,
                        "driver_account": job.driver_account,
                        "guresid": job.guresid,
                        "inputs": job.inputs,
                        "dependencies": self.dependencies,
                    }),
                )
                .await
                .map_err(|e| format!("the operation's inputs were not recorded: {e}"));
            let result = match recorded {
                Err(error) => Err(ExecFailure::Retryable {
                    error,
                    retry_after: None,
                }),
                Ok(()) => match self.with_prior_state(job, node).await {
                    Ok(job) => self.provision(job).await,
                    Err(failure) => Err(failure),
                },
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
                ExecFailure::Uncertain(error) => {
                    self.checkpoint(
                        &started.operation_id,
                        CheckpointResult::Unknown {
                            error: error.clone(),
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
        let at = |what: &str| format!("definition `{}`: {what}", def.id);
        let scope = &self.scope;
        // Secret inputs record where each reference came from.
        let recording = Recording {
            scope,
            seen: std::sync::Mutex::new(BTreeSet::new()),
        };
        let values = resolve_value(
            &Value::Object(def.driver_inputs.values.clone()),
            Dialect::Definition,
            scope,
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
            &recording,
            Sink::Secret,
            "driver_inputs.secrets",
        )
        .map_err(|e| at(&e.to_string()))?;
        let secret_refs = references(&Value::Object(def.driver_inputs.secret_refs.clone()));
        let driver_account = match &def.driver_account {
            Some(account) => Some(
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
                .map_err(|e| at(&format!("driver_account: {e}")))?,
            ),
            None => None,
        };
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
            existing: false,
            deadline: self.deadline,
            log: log.clone(),
        })
    }

    /// Runs the executor until the deadline. A panic or the deadline leaves the
    /// outcome uncertain, since the driver may have changed something.
    async fn provision(&self, job: ProvisionJob) -> Result<ProvisionResult, ExecFailure> {
        let executor = self.executor.clone();
        let task = tokio::spawn(async move { executor.provision(job).await });
        let abort = task.abort_handle();
        match tokio::time::timeout_at(self.deadline, task).await {
            Ok(Ok(result)) => result,
            Ok(Err(e)) => {
                // A panic message could hold a secret, so it is not logged.
                tracing::error!(panicked = e.is_panic(), "the executor stopped unexpectedly");
                Err(ExecFailure::Uncertain(
                    "the executor stopped unexpectedly while the resource was provisioning".into(),
                ))
            }
            Err(_) => {
                abort.abort();
                Err(ExecFailure::Uncertain(
                    "the deployment ran out of time while the resource was provisioning".into(),
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
        job.cookie = resource.cookie;
        job.target = resource.target;
        job.existing = resource.last_deploy_id.is_some();
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
        .filter(|n| {
            !n.is_empty()
                && n.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        })
        .map(|name| (name.clone(), json!(format!("${{externals.{key}.{name}}}"))))
        .collect::<serde_json::Map<_, _>>()
        .into()
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
