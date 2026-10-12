//! Deleting the resources a deployment leaves out: workloads it removed and
//! resources scheduled for deletion, or, when it deletes the environment,
//! every owned resource.
//!
//! A resource is deleted only after every resource that depends on it is gone,
//! so deletion runs in reverse dependency order. A resource something still
//! uses stays, and so do the dependencies of a resource whose deletion failed.
//! Every deletion needs a destructive-action permit. External resources are
//! never deleted; adopted ones only when scheduled for deletion.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use openporch_core::ids::{AccountId, DefinitionId, Descriptor, Guresid};
use openporch_core::placeholder::Resolved;
use openporch_store::{
    ActiveResource, CheckpointResult, OperationKind, OperationStart, Store, StoreError,
};
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::Instant;

use super::log::DeploymentLog;
use super::{
    Deployment, DeprovisionJob, ExecFailure, Executor, JobContext, JobInputs, JobLog, Limits,
    Outcome, Planner, ProvisionJob, backoff, log_retry,
};

/// A destructive action that needs a permit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DestructiveAction {
    /// Deleting a resource.
    Delete,
    /// Detaching a resource from management.
    Detach,
    /// Replacing a resource: the driver's plan destroys and recreates it.
    Replace,
    /// A change whose impact the driver could not determine.
    Unknown,
}

impl DestructiveAction {
    /// The action's name in permits.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Delete => "delete",
            Self::Detach => "detach",
            Self::Replace => "replace",
            Self::Unknown => "unknown",
        }
    }
}

/// A destructive action to check against the caller's permits.
#[derive(Debug, Clone)]
pub struct PermitRequest<'a> {
    /// Organization.
    pub org: &'a str,
    /// Application.
    pub app: &'a str,
    /// Environment.
    pub env: &'a str,
    /// The resource.
    pub descriptor: &'a str,
    /// The action.
    pub action: DestructiveAction,
    /// The permits presented.
    pub permits: &'a [String],
    /// Fingerprint of the driver's plan, when the action comes from one. A
    /// permit issued for one plan does not cover a different plan.
    pub plan: Option<&'a str>,
}

/// Decides whether presented permits allow a destructive action. A permit
/// authorizes one action on one resource and its retries: checking it again
/// for the same action, such as after a deletion that could not be recorded
/// or failed and is retried, must succeed.
pub trait PermitCheck: Send + Sync + std::fmt::Debug {
    /// `Ok` when a permit allows the action; otherwise why not.
    fn check<'a>(
        &'a self,
        req: PermitRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
}

/// Refuses every destructive action: permits are issued from previews, which
/// do not exist yet.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoPermits;

impl PermitCheck for NoPermits {
    fn check<'a>(
        &'a self,
        req: PermitRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            Err(format!(
                "{} `{}` needs a destructive-action permit, and permits cannot be issued yet",
                req.action.as_str(),
                req.descriptor
            ))
        })
    }
}

impl<S: Store + 'static, E: Executor> Planner<S, E> {
    /// Deletes the owned resources `deployment` leaves out that are workloads or
    /// scheduled for deletion, recording an outcome for each. Unless
    /// `converged`, nothing is deleted and they are left pending.
    pub(super) async fn deprovision(
        &self,
        deployment: &Arc<Deployment>,
        deadline: Instant,
        converged: bool,
        outcomes: &mut BTreeMap<Descriptor, Outcome>,
        log: &DeploymentLog,
    ) -> Result<(), String> {
        let d = deployment;
        let active = match self
            .store
            .active_resources(d.org.as_str(), d.app.as_str(), d.env.as_str())
            .await
        {
            Ok(active) => active,
            Err(e) => {
                tracing::error!(deployment = %d.id, error = %e, "could not list active resources");
                // Removed workloads and scheduled resources may remain, so the
                // deployment has not converged.
                return Err(format!("the resources to delete could not be listed: {e}"));
            }
        };
        let mut candidates: BTreeMap<Descriptor, ActiveResource> = BTreeMap::new();
        // Removals the deployment must complete to succeed: removed workloads
        // and what they own, or everything owned when the environment goes.
        let mut mandatory: BTreeSet<String> = BTreeSet::new();
        let mut left_out: Vec<(Descriptor, &ActiveResource)> = Vec::new();
        for r in &active {
            let Ok(descriptor) = Descriptor::parse(&r.descriptor) else {
                tracing::error!(resource = %r.descriptor, "an active resource has an invalid descriptor");
                continue;
            };
            // External resources are never deleted, and adopted ones only when
            // explicitly scheduled. Removed workloads go, and when the
            // environment is deleted, everything it owns and every adopted
            // resource scheduled for deletion.
            let must_go = match r.ownership.as_str() {
                "owned" => d.teardown || descriptor.ty.as_str() == "workload",
                "adopted" => d.teardown && r.scheduled_deletion,
                _ => false,
            };
            let deletable = match r.ownership.as_str() {
                "owned" | "adopted" => r.scheduled_deletion || must_go,
                _ => false,
            };
            // One whose deletion the driver already confirmed only needs its
            // record removed.
            if d.graph.nodes.contains_key(&descriptor) {
                continue;
            }
            if !(deletable || r.cleanup_pending) {
                left_out.push((descriptor, r));
                continue;
            }
            if must_go {
                mandatory.insert(r.descriptor.clone());
            }
            candidates.insert(descriptor, r.clone());
        }
        // A removed workload loses the owned resources provisioned with it,
        // such as its triggers: they depend on it and are left out with it.
        loop {
            let before = mandatory.len();
            left_out.retain(|(descriptor, r)| {
                let owned_by_removed = r.ownership == "owned"
                    && r.dependencies.iter().any(|dep| mandatory.contains(dep));
                if owned_by_removed {
                    mandatory.insert(r.descriptor.clone());
                    candidates.insert(descriptor.clone(), (*r).clone());
                }
                !owned_by_removed
            });
            if mandatory.len() == before {
                break;
            }
        }
        if candidates.is_empty() {
            return Ok(());
        }
        if !converged {
            for descriptor in candidates.into_keys() {
                outcomes.insert(
                    descriptor,
                    Outcome::Pending(
                        "deleted only once the deployment's other resources provision".into(),
                    ),
                );
            }
            return Ok(());
        }
        // Who uses each resource, by the dependencies recorded when they last
        // provisioned. A detached resource keeps running outside openporch, so
        // it still uses what it depended on.
        let tombstones = match self
            .store
            .tombstones(d.org.as_str(), d.app.as_str(), d.env.as_str())
            .await
        {
            Ok(tombstones) => tombstones,
            Err(e) => {
                tracing::error!(deployment = %d.id, error = %e, "could not list tombstones");
                return Err(format!("the resources to delete could not be listed: {e}"));
            }
        };
        let mut users: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for r in &active {
            for dep in &r.dependencies {
                users
                    .entry(dep.clone())
                    .or_default()
                    .insert(r.descriptor.clone());
            }
        }
        for t in tombstones.iter().filter(|t| t.reason == "detached") {
            for dep in t.body["dependencies"].as_array().into_iter().flatten() {
                if let Some(dep) = dep.as_str() {
                    users
                        .entry(dep.to_owned())
                        .or_default()
                        .insert(format!("{} (detached)", t.descriptor));
                }
            }
        }
        let candidate_names: BTreeSet<String> =
            candidates.values().map(|r| r.descriptor.clone()).collect();
        let mut deleted: BTreeSet<String> = BTreeSet::new();
        let mut started: BTreeSet<Descriptor> = BTreeSet::new();
        let mut running = JoinSet::new();
        let mut tasks = BTreeMap::new();
        loop {
            for (descriptor, r) in &candidates {
                // A resource already deleted in the cloud is gone for its users
                // too.
                let ready = r.cleanup_pending
                    || users
                        .get(&r.descriptor)
                        .is_none_or(|u| u.iter().all(|u| deleted.contains(u)));
                if started.contains(descriptor) || !ready {
                    continue;
                }
                started.insert(descriptor.clone());
                if Instant::now() >= deadline {
                    continue;
                }
                let run = DeleteRun {
                    store: self.store.clone(),
                    executor: self.executor.clone(),
                    permits: self.permits.clone(),
                    checks: self.checks.clone(),
                    limits: self.limits.clone(),
                    deployment: deployment.clone(),
                    resource: r.clone(),
                    descriptor: descriptor.clone(),
                    deadline,
                    log: log.resource(descriptor),
                };
                let id = running
                    .spawn(async move { (run.descriptor.clone(), run.run().await) })
                    .id();
                tasks.insert(id, descriptor.clone());
            }
            let Some(joined) = running.join_next_with_id().await else {
                break;
            };
            let (descriptor, outcome) = match joined {
                Ok((_, result)) => result,
                Err(e) => {
                    // The deletion may have started: only an observation can
                    // tell, and a restart marks its open operation unknown.
                    // A panic message could hold a secret, so it is not logged.
                    tracing::error!(
                        panicked = e.is_panic(),
                        "a deletion task stopped unexpectedly"
                    );
                    let Some(descriptor) = tasks.get(&e.id()).cloned() else {
                        continue;
                    };
                    (
                        descriptor,
                        Outcome::Unknown("the deletion stopped unexpectedly".into()),
                    )
                }
            };
            if outcome == Outcome::Deleted {
                deleted.insert(descriptor.to_string());
            }
            outcomes.insert(descriptor, outcome);
        }
        // A candidate stays when something that is kept uses it, directly or
        // through other candidates: kept rather than failed, so that scheduling
        // something a retained resource uses does not fail every later
        // deployment. Repeat until no more candidates turn out to be kept.
        let undecided: Vec<(Descriptor, ActiveResource)> = candidates
            .into_iter()
            .filter(|(d, _)| !outcomes.contains_key(d))
            .collect();
        let blockers = |r: &ActiveResource| -> Vec<String> {
            users
                .get(&r.descriptor)
                .into_iter()
                .flatten()
                .filter(|u| !deleted.contains(*u))
                .cloned()
                .collect()
        };
        let mut kept: BTreeMap<String, String> = BTreeMap::new();
        loop {
            let mut changed = false;
            for (_, r) in &undecided {
                // Removing a workload is not optional, so it is never kept.
                if kept.contains_key(&r.descriptor) || mandatory.contains(&r.descriptor) {
                    continue;
                }
                let keeper = blockers(r)
                    .into_iter()
                    .find(|u| !candidate_names.contains(u) || kept.contains_key(u));
                if let Some(u) = keeper {
                    kept.insert(r.descriptor.clone(), u);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        for (descriptor, r) in undecided {
            if let Some(u) = kept.get(&r.descriptor) {
                outcomes.insert(
                    descriptor,
                    Outcome::Kept(format!("scheduled for deletion, but still used by `{u}`")),
                );
                continue;
            }
            let reason = match blockers(&r).first() {
                // A detached object is no longer managed, so nothing tells
                // openporch it is gone: what it uses can only be detached too.
                Some(u) if u.ends_with(" (detached)") => format!(
                    "still used by `{u}`, which openporch no longer manages; detach this \
                     resource too to leave it in place, then retry"
                ),
                Some(u) if !candidate_names.contains(u) || kept.contains_key(u) => {
                    format!("still used by `{u}`, which stays; schedule it for deletion too")
                }
                Some(u) => format!("waiting for `{u}` to be deleted"),
                None if Instant::now() >= deadline => "the deployment ran out of time".into(),
                None => "deletion stopped unexpectedly".into(),
            };
            outcomes.insert(descriptor, Outcome::Pending(reason));
        }
        Ok(())
    }
}

/// One resource's deletion, with retries.
struct DeleteRun<S, E> {
    store: Arc<S>,
    executor: Arc<E>,
    permits: Arc<Semaphore>,
    checks: Arc<dyn PermitCheck>,
    limits: Limits,
    deployment: Arc<Deployment>,
    resource: ActiveResource,
    descriptor: Descriptor,
    deadline: Instant,
    log: JobLog,
}

impl<S: Store + 'static, E: Executor> DeleteRun<S, E> {
    async fn run(self) -> Outcome {
        match self.attempts().await {
            Ok(()) => Outcome::Deleted,
            Err(outcome) => outcome,
        }
    }

    async fn attempts(&self) -> Result<(), Outcome> {
        let d = &self.deployment;
        let r = &self.resource;
        let failed = |e: String| Outcome::Failed(e);
        let driver_type = r
            .driver_type
            .clone()
            .ok_or_else(|| failed("the resource has no recorded driver".into()))?;
        let definition_id: DefinitionId = r
            .definition_id
            .as_deref()
            .unwrap_or_default()
            .parse()
            .map_err(|_| failed("the resource has no valid recorded definition".into()))?;
        // A resource that never provisioned has no recorded inputs; its driver
        // deletes whatever an attempt left behind from the GUResID alone.
        let absent = r.inputs.is_null() || r.inputs.as_object().is_some_and(|o| o.is_empty());
        let inputs: JobInputs = if absent {
            JobInputs {
                values: Value::Null,
                secrets: Resolved::Value(Value::Null),
                secret_refs: Resolved::Value(Value::Null),
                resource: Resolved::Value(Value::Null),
                sources: Vec::new(),
            }
        } else {
            serde_json::from_value(r.inputs.clone()).map_err(|e| {
                failed(format!(
                    "the resource's recorded inputs are unreadable: {e}"
                ))
            })?
        };
        let driver_account = r
            .driver_account
            .as_deref()
            .map(str::parse::<AccountId>)
            .transpose()
            .map_err(|_| failed("the resource's recorded cloud account is invalid".into()))?;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let permit =
                tokio::time::timeout_at(self.deadline, self.permits.clone().acquire_owned()).await;
            let permit = match permit {
                Ok(Ok(permit)) => permit,
                Ok(Err(_)) => return Err(failed("the planner is shutting down".into())),
                Err(_) => return Err(Outcome::Pending("the deployment ran out of time".into())),
            };
            if Instant::now() >= self.deadline {
                return Err(Outcome::Pending("the deployment ran out of time".into()));
            }
            // The permit is used only once the deletion can start, so one that
            // waited for a slot until the deadline is still unused. Removing
            // the record of a confirmed deletion was already permitted.
            if attempt == 1 && !r.cleanup_pending {
                self.check_permit().await?;
            }
            let started = self
                .store
                .begin_operation(OperationStart {
                    org: d.org.to_string(),
                    app: d.app.to_string(),
                    env: d.env.to_string(),
                    deployment_id: d.id.clone(),
                    descriptor: self.descriptor.clone(),
                    definition_id: r.definition_id.clone(),
                    definition_version: r.definition_version,
                    driver_type: Some(driver_type.clone()),
                    kind: OperationKind::Delete,
                    input_fingerprint: "delete".into(),
                    recovery: json!({
                        "definition_id": r.definition_id,
                        "definition_version": r.definition_version,
                        "driver_type": driver_type,
                        "driver_account": r.driver_account,
                        "inputs": r.inputs,
                        "cleanup_only": r.cleanup_pending,
                    }),
                })
                .await
                .map_err(|e| match e {
                    StoreError::RecoveryRequired(_) => failed(format!(
                        "an earlier operation's outcome is unknown; recover the resource first ({e})"
                    )),
                    other => failed(format!("the deletion was not recorded: {other}")),
                })?;
            let guresid: Guresid = started.guresid.clone();
            let log = self
                .log
                .operation(&started.operation_id, started.guresid.as_str());
            if r.cleanup_pending {
                log.info("removing the record of a deletion the driver already confirmed");
            } else {
                log.info(&format!(
                    "deleting with `{driver_type}` (attempt {attempt})"
                ));
            }
            let job = ProvisionJob {
                operation_id: started.operation_id.clone(),
                attempt: started.attempt,
                descriptor: self.descriptor.clone(),
                guresid,
                context: JobContext::of(d),
                definition_id: definition_id.clone(),
                definition_version: r.definition_version.unwrap_or_default(),
                driver_type: driver_type.clone(),
                driver_account: driver_account.clone(),
                inputs: inputs.clone(),
                input_fingerprint: "delete".into(),
                cookie: r.cookie.clone(),
                target: r.target.clone(),
                deadline: self.deadline,
                log: log.clone(),
            };
            // The driver deletes against what was retained, not the current
            // definition.
            let job = DeprovisionJob {
                job,
                snapshot: r.snapshot.clone(),
                recovery: r.recovery.clone(),
            };
            let result = if r.cleanup_pending {
                Ok(())
            } else {
                self.execute(job).await
            };
            drop(permit);
            let failure = match result {
                Ok(()) => {
                    let recorded = self.record_deletion(&started.operation_id).await;
                    if recorded.is_ok() {
                        log.status("info", "deleted");
                    }
                    return recorded;
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
                    return Err(failed(error));
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
                    let recorded = self
                        .checkpoint(
                            &started.operation_id,
                            CheckpointResult::Failed {
                                error: error.clone(),
                            },
                        )
                        .await;
                    // The operation is still open, so another attempt would be
                    // refused; stop, and a restart leaves it for recovery.
                    if !recorded {
                        return Err(failed(format!(
                            "{error} (the failure could not be recorded, so it is not retried)"
                        )));
                    }
                    if attempt >= self.limits.attempts {
                        return Err(failed(format!("{error} (after {attempt} attempts)")));
                    }
                    let delay = retry_after.unwrap_or_else(|| backoff(&self.limits, attempt));
                    if Instant::now() + delay >= self.deadline {
                        return Err(failed(format!(
                            "{error} (no time left for another attempt)"
                        )));
                    }
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }

    /// Checks the deployment's permits for this deletion, within the deadline.
    async fn check_permit(&self) -> Result<(), Outcome> {
        let d = &self.deployment;
        let check = self.checks.check(PermitRequest {
            org: d.org.as_str(),
            app: d.app.as_str(),
            env: d.env.as_str(),
            descriptor: &self.resource.descriptor,
            action: DestructiveAction::Delete,
            permits: &d.permits,
            plan: None,
        });
        match tokio::time::timeout_at(self.deadline, check).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(Outcome::Failed(format!("approval_required: {e}"))),
            Err(_) => Err(Outcome::Pending("the deployment ran out of time".into())),
        }
    }

    /// Records a deletion the driver confirmed. When the record cannot be
    /// removed, the resource is marked so that a later attempt only removes
    /// it, without deleting in the cloud again.
    async fn record_deletion(&self, operation_id: &str) -> Result<(), Outcome> {
        for attempt in 0..3u32 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(50) * attempt).await;
            }
            if self
                .checkpoint(operation_id, CheckpointResult::Deleted)
                .await
            {
                return Ok(());
            }
        }
        let error = "the resource was deleted, but its record could not be removed";
        if self
            .checkpoint(
                operation_id,
                CheckpointResult::CleanupPending {
                    error: error.into(),
                },
            )
            .await
        {
            return Err(Outcome::CleanupPending(error.into()));
        }
        self.checkpoint(
            operation_id,
            CheckpointResult::Unknown {
                error: error.into(),
            },
        )
        .await;
        Err(Outcome::Unknown(error.into()))
    }

    /// Runs the executor until the deadline; a panic or the deadline leaves the
    /// outcome uncertain.
    async fn execute(&self, job: DeprovisionJob) -> Result<(), ExecFailure> {
        let executor = self.executor.clone();
        let task = tokio::spawn(async move { executor.deprovision(job).await });
        let abort = task.abort_handle();
        match tokio::time::timeout_at(self.deadline, task).await {
            Ok(Ok(result)) => result,
            Ok(Err(e)) => {
                // A panic message could hold a secret, so it is not logged.
                tracing::error!(panicked = e.is_panic(), "the executor stopped unexpectedly");
                Err(ExecFailure::Uncertain(
                    "the executor stopped unexpectedly while the resource was being deleted".into(),
                ))
            }
            Err(_) => {
                abort.abort();
                Err(ExecFailure::Uncertain(
                    "the deployment ran out of time while the resource was being deleted".into(),
                ))
            }
        }
    }

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
