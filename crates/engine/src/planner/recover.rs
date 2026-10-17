//! Recovering a resource whose last operation has an unknown outcome, as after
//! a restart or a timeout: the resource is observed with the identity, driver
//! and inputs the operation recorded before its driver ran, and the outcome
//! the observation establishes is recorded. Nothing is mutated; an outcome
//! observation cannot establish needs an administrator.

use std::time::Duration;

use tracing::Instrument;

use openporch_core::ids::{AccountId, DefinitionId, Descriptor, Guresid};
use openporch_store::{
    AuditIntent, CheckpointResult, OperationKind, Provisioned, Resolution, Store, StoreError,
    UnknownOperation,
};
use serde_json::{Value, json};
use tokio::time::Instant;

use super::{
    ExecFailure, Executor, JobContext, JobInputs, JobLog, Observed, Planner, ProvisionJob,
};

/// How long observing a resource may take.
const OBSERVE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// How long a recovery waits for a driver slot.
const PERMIT_WAIT: Duration = Duration::from_secs(30);

/// How recovering a resource ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recovered {
    /// The creation or update took effect: the resource is provisioned.
    Provisioned,
    /// The deletion took effect: the resource is gone.
    Deleted,
    /// The operation can be repeated safely with the same identity, which the
    /// next deployment does.
    Repeatable(String),
    /// The operation is still running. Nothing changed; recover again later.
    InProgress,
    /// Observation could not establish the outcome. The resource stays
    /// blocked until an administrator resolves it.
    RecoveryRequired(String),
}

/// Why a resource could not be recovered.
#[derive(Debug)]
pub enum RecoverError {
    /// The resource has no operation with an unknown outcome.
    NothingToRecover(String),
    /// Observing failed in a way that may pass. Nothing changed.
    Observation(String),
    /// The store refused or failed.
    Store(StoreError),
}

impl std::fmt::Display for RecoverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NothingToRecover(d) => {
                write!(f, "`{d}` has no operation with an unknown outcome")
            }
            Self::Observation(e) => write!(f, "{e}; nothing changed, so recover again"),
            Self::Store(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for RecoverError {}

impl From<StoreError> for RecoverError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

impl<S: Store + 'static, E: Executor> Planner<S, E> {
    /// Observes `descriptor`, whose last operation has an unknown outcome,
    /// and records what the observation establishes, with `audit`.
    pub async fn recover(
        &self,
        org: &str,
        app: &str,
        env: &str,
        descriptor: &str,
        audit: AuditIntent,
    ) -> Result<Recovered, RecoverError> {
        // Everything the recovery logs, its resolution included, names the
        // resource and operation.
        let span = match Descriptor::parse(descriptor) {
            Ok(d) => super::resource_span(&d),
            Err(_) => tracing::Span::none(),
        };
        self.recover_in_span(org, app, env, descriptor, audit)
            .instrument(span)
            .await
    }

    async fn recover_in_span(
        &self,
        org: &str,
        app: &str,
        env: &str,
        descriptor: &str,
        audit: AuditIntent,
    ) -> Result<Recovered, RecoverError> {
        let op = self
            .store
            .unknown_operation(org, app, env, descriptor)
            .await?
            .ok_or_else(|| RecoverError::NothingToRecover(descriptor.to_owned()))?;
        super::record_operation_ids(&op.operation_id, &op.resource.guresid);
        let deleting = op.kind == OperationKind::Delete;
        // A restore's operation asks whether the resource still is what the
        // backup recorded, not whether an update took effect.
        let restored = op.recovery.get("restored") == Some(&Value::Bool(true));
        let (resolution, recovered) = match self.job(org, app, env, &op).await {
            Err(reason) => required(reason),
            Ok(job) => {
                let account = job.driver_account.as_ref().map(ToString::to_string);
                // A resource that existed before the operation says nothing,
                // by existing, about whether an update took effect.
                let updating = !deleting && job.existing && !restored;
                // Observing runs the driver too, so it waits for a slot
                // like any other driver operation.
                // A recovery request does not wait long: when every slot
                // stays busy it is refused and can be repeated.
                let permit =
                    tokio::time::timeout(PERMIT_WAIT, self.permits.clone().acquire_owned()).await;
                let Ok(Ok(permit)) = permit else {
                    return Err(RecoverError::Observation(
                        "every driver slot is busy; try the recovery again later".into(),
                    ));
                };
                let span = super::operation_span(&job, "observe");
                let executor = self.executor.clone();
                // The observation runs in its own task, which holds the
                // driver slot and is counted until the driver returns, even
                // when the request that started it goes away. A panic leaves
                // the outcome as unknown as before.
                let observing = async move {
                    let _permit = permit;
                    let started = tokio::time::Instant::now();
                    let running = crate::metrics::METRICS.driver_started();
                    let observed = tokio::spawn(
                        async move { executor.observe(job, deleting).await }.in_current_span(),
                    )
                    .await
                    .unwrap_or_else(|e| {
                        // A panic message could hold a secret, so it is not logged.
                        tracing::error!(
                            panicked = e.is_panic(),
                            "the executor stopped unexpectedly"
                        );
                        Err(ExecFailure::Uncertain(
                            "the executor stopped unexpectedly while observing the resource".into(),
                        ))
                    });
                    drop(running);
                    super::log_outcome("observe", started, &observed);
                    observed
                };
                let observed = tokio::spawn(observing.instrument(span))
                    .await
                    .unwrap_or_else(|_| {
                        Err(ExecFailure::Uncertain(
                            "the executor stopped unexpectedly while observing the resource".into(),
                        ))
                    });
                match observed {
                    Ok(Observed::InProgress) => return Ok(Recovered::InProgress),
                    Ok(Observed::Absent { .. }) if deleting => (
                        Resolution::Observed(CheckpointResult::Deleted),
                        Recovered::Deleted,
                    ),
                    Ok(Observed::Absent { .. }) if restored => required(
                        "the backup records the resource, but nothing exists for its identity: \
                         it was deleted after the backup was taken, or its state was not \
                         restored; check the cloud before resolving it"
                            .into(),
                    ),
                    Ok(Observed::Absent { repeatable: true }) => repeatable_reason(
                        "nothing exists for the resource's identity, so the next deployment \
                         provisions it with the same identity",
                    ),
                    Ok(Observed::Absent { repeatable: false }) => required(
                        "nothing exists for the resource's identity yet, but its driver cannot \
                         guarantee that a creation still on its way is not duplicated"
                            .into(),
                    ),
                    // It still exists: what the backup records stands.
                    Ok(Observed::Ready { .. }) if restored => {
                        (Resolution::Confirmed, Recovered::Provisioned)
                    }
                    Ok(Observed::Ready { repeatable, .. }) if deleting => {
                        if repeatable {
                            repeatable_reason(
                                "the resource still exists, so the next deployment that leaves \
                                 it out deletes it again",
                            )
                        } else {
                            required(
                                "the resource still exists, but its driver cannot guarantee \
                                 that repeating the deletion is safe"
                                    .into(),
                            )
                        }
                    }
                    Ok(Observed::Ready { repeatable, .. }) if updating => {
                        if repeatable {
                            repeatable_reason(
                                "the resource exists, but whether the update took effect is \
                                 unknown, so the next deployment repeats it with the same \
                                 identity",
                            )
                        } else {
                            required(
                                "the resource exists, but whether the update took effect is \
                                 unknown and its driver cannot guarantee that repeating it is \
                                 safe"
                                    .into(),
                            )
                        }
                    }
                    Ok(Observed::Ready { result, .. }) => (
                        Resolution::Observed(CheckpointResult::Provisioned(Box::new(provisioned(
                            &op, *result, account,
                        )))),
                        Recovered::Provisioned,
                    ),
                    Ok(Observed::Failed {
                        error,
                        repeatable: true,
                    }) => repeatable_reason(&format!(
                        "the driver reports that the operation failed and can be repeated: \
                         {error}"
                    )),
                    Ok(Observed::Failed {
                        error,
                        repeatable: false,
                    }) => required(format!(
                        "the driver reports that the operation failed, but cannot guarantee \
                         that repeating it is safe: {error}"
                    )),
                    Ok(Observed::Unknown(reason)) => required(reason),
                    Err(ExecFailure::Permanent(e)) => {
                        required(format!("the resource cannot be observed: {e}"))
                    }
                    Err(ExecFailure::Retryable { error, .. } | ExecFailure::Uncertain(error)) => {
                        return Err(RecoverError::Observation(error));
                    }
                }
            }
        };
        if let Err(e) = self
            .store
            .resolve_operation(&op.operation_id, resolution, audit)
            .await
        {
            tracing::error!(error = %e, "could not record a recovery");
            return Err(e.into());
        }
        Ok(recovered)
    }

    /// The job the operation ran, rebuilt from what it recorded before its
    /// driver ran; why it cannot be, otherwise.
    async fn job(
        &self,
        org: &str,
        app: &str,
        env: &str,
        op: &UnknownOperation,
    ) -> Result<ProvisionJob, String> {
        let r = &op.resource;
        let recorded = &op.recovery;
        let invalid = |what: &str| format!("the operation's recorded {what} is invalid");
        let inputs: JobInputs = match recorded.get("inputs") {
            Some(inputs) if !inputs.is_null() => {
                serde_json::from_value(inputs.clone()).map_err(|_| invalid("inputs"))?
            }
            _ => return Err("the operation recorded no inputs".into()),
        };
        let text = |key: &str| recorded.get(key).and_then(Value::as_str);
        let definition_id: DefinitionId = text("definition_id")
            .or(r.definition_id.as_deref())
            .ok_or_else(|| invalid("definition"))?
            .parse()
            .map_err(|_| invalid("definition"))?;
        let driver_type = text("driver_type")
            .or(r.driver_type.as_deref())
            .ok_or_else(|| invalid("driver"))?
            .to_owned();
        let driver_account = text("driver_account")
            .map(str::parse::<AccountId>)
            .transpose()
            .map_err(|_| invalid("cloud account"))?;
        let env_type = self
            .store
            .env(org, app, env)
            .await
            .map_err(|e| format!("the environment could not be read: {e}"))?
            .ok_or_else(|| format!("environment `{app}/{env}` no longer exists"))?
            .env_type;
        let invalid_id = |what: &str| format!("the stored {what} ID is invalid");
        Ok(ProvisionJob {
            operation_id: op.operation_id.clone(),
            attempt: op.attempt,
            descriptor: Descriptor::parse(&r.descriptor).map_err(|_| invalid_id("resource"))?,
            guresid: r
                .guresid
                .parse::<Guresid>()
                .map_err(|_| invalid_id("GUResID"))?,
            context: JobContext {
                org: org.parse().map_err(|_| invalid_id("organization"))?,
                app: app.parse().map_err(|_| invalid_id("application"))?,
                env: env.parse().map_err(|_| invalid_id("environment"))?,
                env_type: env_type
                    .parse()
                    .map_err(|_| invalid_id("environment type"))?,
                deployment_id: op.deployment_id.clone(),
                // Observing changes nothing, so it needs no permit.
                permits: Vec::new(),
            },
            definition_id,
            definition_version: recorded
                .get("definition_version")
                .and_then(Value::as_i64)
                .or(r.definition_version)
                .unwrap_or_default(),
            driver_type,
            driver_account,
            inputs,
            input_fingerprint: op.input_fingerprint.clone(),
            cookie: r.cookie.clone(),
            target: r.target.clone(),
            pin: None,
            existing: r.last_deploy_id.is_some() || r.cookie.is_some() || r.target.is_some(),
            deadline: Instant::now() + OBSERVE_TIMEOUT,
            log: JobLog::discard(),
        })
    }
}

fn repeatable_reason(reason: &str) -> (Resolution, Recovered) {
    (
        Resolution::Repeatable {
            reason: reason.to_owned(),
        },
        Recovered::Repeatable(reason.to_owned()),
    )
}

fn required(reason: String) -> (Resolution, Recovered) {
    (
        Resolution::RecoveryRequired {
            reason: reason.clone(),
        },
        Recovered::RecoveryRequired(reason),
    )
}

/// What a creation or update that took effect records, as the planner
/// records it after provisioning.
fn provisioned(
    op: &UnknownOperation,
    result: super::ProvisionResult,
    account: Option<String>,
) -> Provisioned {
    let recorded = &op.recovery;
    let dependencies = recorded
        .get("dependencies")
        .and_then(Value::as_object)
        .map(|deps| deps.keys().cloned().collect())
        .unwrap_or_default();
    let definition = recorded
        .get("definition")
        .or_else(|| op.resource.snapshot.get("definition"))
        .cloned()
        .unwrap_or(Value::Null);
    Provisioned {
        outputs: Value::Object(result.outputs),
        secret_outputs: result.secret_outputs,
        cookie: result.cookie,
        recovery: result.recovery,
        inputs: recorded.get("inputs").cloned().unwrap_or(Value::Null),
        dependencies,
        snapshot: json!({
            "deployment_id": op.deployment_id,
            "definition_version": recorded.get("definition_version"),
            "definition": definition,
        }),
        account,
        target: result.target,
        backend: result.backend,
        external_ids: result.external_ids,
        pin: result.pin,
    }
}
