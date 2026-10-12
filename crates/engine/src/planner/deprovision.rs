//! Deleting the resources a deployment leaves out: workloads it removed and
//! resources scheduled for deletion.
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

use openporch_core::ids::{AccountId, DefinitionId, Descriptor, Guresid};
use openporch_core::placeholder::Resolved;
use openporch_store::{
    ActiveResource, CheckpointResult, OperationKind, OperationStart, Store, StoreError,
};
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::Instant;

use super::{
    Deployment, ExecFailure, Executor, JobInputs, Limits, Outcome, Planner, ProvisionJob, backoff,
};

/// A destructive action that needs a permit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DestructiveAction {
    /// Deleting a resource.
    Delete,
    /// Detaching a resource from management.
    Detach,
}

impl DestructiveAction {
    /// The action's name in permits.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Delete => "delete",
            Self::Detach => "detach",
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
}

/// Decides whether presented permits allow a destructive action, and uses up
/// the permit that does.
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
    ) {
        let d = deployment;
        let active = match self
            .store
            .active_resources(d.org.as_str(), d.app.as_str(), d.env.as_str())
            .await
        {
            Ok(active) => active,
            Err(e) => {
                tracing::error!(deployment = %d.id, error = %e, "could not list active resources");
                return;
            }
        };
        let mut candidates: BTreeMap<Descriptor, ActiveResource> = BTreeMap::new();
        for r in &active {
            let Ok(descriptor) = Descriptor::parse(&r.descriptor) else {
                tracing::error!(resource = %r.descriptor, "an active resource has an invalid descriptor");
                continue;
            };
            // External resources are never deleted, and adopted ones only when
            // explicitly scheduled.
            let removed_workload = descriptor.ty.as_str() == "workload" && r.ownership == "owned";
            let deletable = match r.ownership.as_str() {
                "owned" | "adopted" => r.scheduled_deletion || removed_workload,
                _ => false,
            };
            if d.graph.nodes.contains_key(&descriptor) || !deletable {
                continue;
            }
            candidates.insert(descriptor, r.clone());
        }
        if candidates.is_empty() {
            return;
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
            return;
        }
        // Who uses each resource, by the dependencies recorded when they last
        // provisioned.
        let mut users: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for r in &active {
            for dep in &r.dependencies {
                users
                    .entry(dep.clone())
                    .or_default()
                    .insert(r.descriptor.clone());
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
                let ready = users
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
                    tracing::error!(error = %e, "a deletion task stopped unexpectedly");
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
        for (descriptor, r) in candidates {
            if outcomes.contains_key(&descriptor) {
                continue;
            }
            let blocker = users
                .get(&r.descriptor)
                .into_iter()
                .flatten()
                .find(|u| !deleted.contains(*u));
            let reason = match blocker {
                Some(u) if candidate_names.contains(u) => {
                    format!("waiting for `{u}` to be deleted")
                }
                // Kept rather than failed, so that scheduling something a
                // retained resource uses does not fail every later deployment.
                Some(u) => {
                    outcomes.insert(
                        descriptor,
                        Outcome::Kept(format!("scheduled for deletion, but still used by `{u}`")),
                    );
                    continue;
                }
                None if Instant::now() >= deadline => "the deployment ran out of time".into(),
                None => "deletion stopped unexpectedly".into(),
            };
            outcomes.insert(descriptor, Outcome::Pending(reason));
        }
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
        let check = self.checks.check(PermitRequest {
            org: d.org.as_str(),
            app: d.app.as_str(),
            env: d.env.as_str(),
            descriptor: &r.descriptor,
            action: DestructiveAction::Delete,
            permits: &d.permits,
        });
        match tokio::time::timeout_at(self.deadline, check).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(failed(format!("approval_required: {e}"))),
            Err(_) => return Err(Outcome::Pending("the deployment ran out of time".into())),
        }
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
        let inputs: JobInputs = if r.inputs.is_null() {
            JobInputs {
                values: Value::Null,
                secrets: Resolved::Value(Value::Null),
                secret_refs: Resolved::Value(Value::Null),
                resource: Resolved::Value(Value::Null),
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
            let job = ProvisionJob {
                operation_id: started.operation_id.clone(),
                attempt: started.attempt,
                descriptor: self.descriptor.clone(),
                guresid,
                definition_id: definition_id.clone(),
                definition_version: r.definition_version.unwrap_or_default(),
                driver_type: driver_type.clone(),
                driver_account: driver_account.clone(),
                inputs: inputs.clone(),
                input_fingerprint: "delete".into(),
                cookie: r.cookie.clone(),
                target: r.target.clone(),
                deadline: self.deadline,
            };
            let result = self.execute(job).await;
            drop(permit);
            let failure = match result {
                Ok(()) => {
                    return if self
                        .checkpoint(&started.operation_id, CheckpointResult::Deleted)
                        .await
                    {
                        Ok(())
                    } else {
                        let error = "the resource was deleted, but that could not be recorded";
                        self.checkpoint(
                            &started.operation_id,
                            CheckpointResult::Unknown {
                                error: error.into(),
                            },
                        )
                        .await;
                        Err(Outcome::Unknown(error.into()))
                    };
                }
                Err(f) => f,
            };
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
                    self.checkpoint(
                        &started.operation_id,
                        CheckpointResult::Failed {
                            error: error.clone(),
                        },
                    )
                    .await;
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

    /// Runs the executor until the deadline; a panic or the deadline leaves the
    /// outcome uncertain.
    async fn execute(&self, job: ProvisionJob) -> Result<(), ExecFailure> {
        let executor = self.executor.clone();
        let task = tokio::spawn(async move { executor.deprovision(job).await });
        let abort = task.abort_handle();
        match tokio::time::timeout_at(self.deadline, task).await {
            Ok(Ok(result)) => result,
            Ok(Err(e)) => {
                tracing::error!(error = %e, "the executor stopped unexpectedly");
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
