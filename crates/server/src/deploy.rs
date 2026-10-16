//! Running accepted deployments in the background.
//!
//! Acceptance returns at once; a [`Runner`] then converges the environment and
//! finishes the deployment. A server that starts with its master keys runs
//! deployments through the planner and the local executor; until then the
//! default runner fails every deployment it is handed, which keeps the
//! environment free for the next one.

use openporch_engine::metrics::METRICS;
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tracing::Instrument;

use openporch_drivers::DriverRegistry;
use openporch_engine::planner::{
    Deployment, DestructiveAction, Executor, PermitCheck, PermitRequest, PinError, Planner,
    RecoverError, Recovered,
};
use openporch_engine::preview::DriverFacts;
use openporch_store::{
    ApprovalRequest, AuditIntent, DeploymentKind, PermitStore, PermitUse, SqliteStore, Store,
    StoreError,
};
use serde_json::Value;

use crate::state::AppState;

/// An accepted deployment.
#[derive(Debug, Clone)]
pub struct Job {
    /// Application.
    pub app: String,
    /// Environment.
    pub env: String,
    /// Deployment ID.
    pub id: String,
    /// The execution snapshot, with the resource graph to converge.
    pub snapshot: Value,
    /// The request that accepted the deployment, for correlating its events.
    pub request_id: Option<String>,
}

/// A resource to recover.
#[derive(Debug, Clone)]
pub struct Recovery {
    /// Application.
    pub app: String,
    /// Environment.
    pub env: String,
    /// The resource.
    pub descriptor: String,
    /// The audit intent of the recovery.
    pub audit: AuditIntent,
}

/// What pinning artifacts returns: the pinned artifact URI of each workload
/// that named one without a fixed identity, by descriptor.
pub type PinFuture =
    Pin<Box<dyn Future<Output = Result<BTreeMap<String, String>, PinError>> + Send>>;

/// What pinning images returns: for each workload with an image named by
/// tag, the pinned image by its JSON pointer in the workload's node.
pub type ImagePinFuture = Pin<
    Box<dyn Future<Output = Result<BTreeMap<String, BTreeMap<String, String>>, PinError>> + Send>,
>;

/// How long pinning a deployment's artifacts, and then its images, may each
/// take at acceptance. Both together stay well within a client's request
/// timeout, so the client hears why.
pub const PIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// What recovering a resource returns.
pub type RecoverFuture = Pin<Box<dyn Future<Output = Result<Recovered, RecoverError>> + Send>>;

/// Runs accepted deployments to completion.
pub trait Runner: Send + Sync + std::fmt::Debug {
    /// Runs `job` and finishes its deployment.
    fn run(&self, state: AppState, job: Job) -> Pin<Box<dyn Future<Output = ()> + Send>>;

    /// Whether the runner has an executor to change resources with. The server
    /// is not ready without one.
    fn has_executor(&self) -> bool {
        true
    }

    /// Pins the artifacts `deployment`'s workloads name without a fixed
    /// identity. A runner without an executor cannot.
    fn pin_artifacts(&self, deployment: Deployment) -> PinFuture {
        let _ = deployment;
        Box::pin(async {
            Err(PinError::Refused(
                "artifacts cannot be pinned until the server runs with an executor; name each \
                 S3 object version as `s3://<bucket>/<key>?versionId=<version>`"
                    .into(),
            ))
        })
    }

    /// Pins the ECR images `deployment`'s workloads name by tag to their
    /// digests. A runner without an executor pins none; the server always
    /// runs with one.
    fn pin_images(&self, deployment: Deployment) -> ImagePinFuture {
        let _ = deployment;
        Box::pin(async { Ok(BTreeMap::new()) })
    }

    /// Observes a resource whose last operation has an unknown outcome and
    /// records what the observation establishes. A runner without an
    /// executor cannot.
    fn recover(&self, recovery: Recovery) -> RecoverFuture {
        let _ = recovery;
        Box::pin(async {
            Err(RecoverError::Observation(
                "resources cannot be observed until the server runs with an executor".into(),
            ))
        })
    }
}

/// The runner without an executor: nothing is provisioned, so every
/// deployment fails with an explanation.
#[derive(Debug)]
pub struct Unimplemented;

impl Runner for Unimplemented {
    fn has_executor(&self) -> bool {
        false
    }

    fn run(&self, state: AppState, job: Job) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(async move {
            finish(
                &state,
                &job,
                Some(
                    "provisioning resources is not implemented yet; nothing changed; \
                     retry once the server runs with an executor"
                        .into(),
                ),
            )
            .await;
        })
    }
}

/// Runs deployments through the planner with executor `E`.
pub struct PlannerRunner<E>(Arc<Planner<SqliteStore, E>>);

impl<E> PlannerRunner<E> {
    /// A runner over `planner`.
    pub fn new(planner: Arc<Planner<SqliteStore, E>>) -> Self {
        Self(planner)
    }
}

impl<E> std::fmt::Debug for PlannerRunner<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("PlannerRunner").field(&self.0).finish()
    }
}

impl<E: Executor> Runner for PlannerRunner<E> {
    fn run(&self, state: AppState, job: Job) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        let planner = self.0.clone();
        Box::pin(async move {
            match deployment(&state, &job).await {
                Ok(deployment) => {
                    planner.run(deployment).await;
                }
                Err(error) => {
                    let error = format!("{error}; nothing changed; fix the cause and deploy again");
                    finish(&state, &job, Some(error)).await;
                }
            }
        })
    }

    fn pin_artifacts(&self, deployment: Deployment) -> PinFuture {
        let planner = self.0.clone();
        Box::pin(async move {
            let pinned = planner.pin_artifacts(&deployment, PIN_TIMEOUT).await?;
            Ok(pinned
                .into_iter()
                .map(|(d, uri)| (d.to_string(), uri))
                .collect())
        })
    }

    fn pin_images(&self, deployment: Deployment) -> ImagePinFuture {
        let planner = self.0.clone();
        Box::pin(async move {
            let pinned = planner.pin_images(&deployment, PIN_TIMEOUT).await?;
            Ok(pinned
                .into_iter()
                .map(|(d, images)| (d.to_string(), images))
                .collect())
        })
    }

    fn recover(&self, r: Recovery) -> RecoverFuture {
        let planner = self.0.clone();
        Box::pin(async move {
            planner
                .recover(&r.audit.org, &r.app, &r.env, &r.descriptor, r.audit.clone())
                .await
        })
    }
}

/// The planner's view of `job`, from its execution snapshot.
async fn deployment(state: &AppState, job: &Job) -> Result<Deployment, String> {
    let record = state
        .store
        .deployment(state.org(), &job.app, &job.env, &job.id)
        .await
        .map_err(|e| format!("the deployment could not be read: {e}"))?
        .ok_or_else(|| format!("deployment `{}` no longer exists", job.id))?;
    planned(
        state,
        &job.app,
        &job.env,
        &job.id,
        record.set_id.unwrap_or_default(),
        &job.snapshot,
        record.kind == DeploymentKind::DeleteEnv,
    )
    .await
}

/// The planner's view of a deployment with `snapshot` in `app/env`. Whether it
/// deletes the environment comes from the caller, never from the snapshot, so
/// a replayed snapshot cannot turn into a deletion.
pub(crate) async fn planned(
    state: &AppState,
    app: &str,
    env: &str,
    id: &str,
    set_id: String,
    snapshot: &Value,
    teardown: bool,
) -> Result<Deployment, String> {
    let org = state.org();
    let env_record = state
        .store
        .env(org, app, env)
        .await
        .map_err(|e| format!("the environment could not be read: {e}"))?
        .ok_or_else(|| format!("environment `{app}/{env}` no longer exists"))?;
    let invalid = |what: &str| format!("the stored {what} ID is invalid");
    let mut deployment = Deployment::from_snapshot(
        org.parse().map_err(|_| invalid("organization"))?,
        app.parse().map_err(|_| invalid("application"))?,
        env.parse().map_err(|_| invalid("environment"))?,
        env_record
            .env_type
            .parse()
            .map_err(|_| invalid("environment type"))?,
        id.to_owned(),
        set_id,
        snapshot,
    )?;
    deployment.teardown = teardown;
    if deployment.teardown && !deployment.graph.nodes.is_empty() {
        return Err("an environment deletion has resources to provision".into());
    }
    Ok(deployment)
}

/// Runs deployment `run`, counting how long it took.
async fn timed(run: impl Future<Output = ()>) {
    let started = std::time::Instant::now();
    run.await;
    METRICS.deployment_ran(started.elapsed());
}

/// Finishes `job`'s deployment, which ends its log with the outcome; a
/// failure to record it leaves the deployment running until restart recovery
/// fails it.
pub async fn finish(state: &AppState, job: &Job, error: Option<String>) {
    let finished = state
        .store
        .finish_deployment(state.org(), &job.app, &job.env, &job.id, error.clone())
        .await;
    log_finished(
        &job.id,
        finished.map_err(|e| e.to_string()),
        error.as_deref(),
    );
}

/// Logs how deployment `id` ended once `finished` says whether its outcome
/// was recorded; an unrecorded outcome is logged as that, not as the outcome.
pub fn log_finished(id: &str, finished: Result<(), String>, error: Option<&str>) {
    match (finished, error) {
        (Err(e), _) => {
            tracing::error!(deployment = %id, error = %e, "could not finish a deployment");
        }
        (Ok(()), None) => {
            METRICS.deployment_finished(false);
            tracing::info!(outcome = "succeeded", "deployment finished");
        }
        (Ok(()), Some(e)) => {
            METRICS.deployment_finished(true);
            tracing::warn!(outcome = "failed", error = %e, "deployment finished");
        }
    }
}

/// The runner a server hands accepted deployments to, and the deployments it
/// runs.
#[derive(Debug)]
pub struct Runners {
    runner: RwLock<Arc<dyn Runner>>,
    /// Running deployments, as `app/env/id`.
    active: Mutex<std::collections::BTreeSet<String>>,
    /// Signaled whenever a deployment stops running.
    stopped: tokio::sync::Notify,
    /// One permit per deployment that may run or wait at once.
    slots: Arc<tokio::sync::Semaphore>,
    capacity: usize,
    /// Accepted deployments waiting for a slot.
    queued: Arc<std::sync::atomic::AtomicUsize>,
}

/// Room for one more running deployment, taken before it is accepted and
/// held until it stops.
#[derive(Debug)]
pub struct Slot {
    _permit: tokio::sync::OwnedSemaphorePermit,
}

/// How long a caller refused for lack of room is asked to wait.
const FULL_RETRY_SECS: u64 = 30;

impl Default for Runners {
    fn default() -> Self {
        Self::new(crate::config::WorkLimits::default().running_deployments)
    }
}

impl Runners {
    /// Runners for at most `capacity` deployments at once.
    pub fn new(capacity: usize) -> Self {
        Self {
            runner: RwLock::new(Arc::new(Unimplemented)),
            active: Mutex::default(),
            stopped: tokio::sync::Notify::new(),
            slots: Arc::new(tokio::sync::Semaphore::new(capacity.max(1))),
            capacity: capacity.max(1),
            queued: Arc::default(),
        }
    }

    /// Takes room for `n` more deployments, or refuses with `429` and
    /// `Retry-After` when there is not that much.
    pub fn slots(&self, n: usize) -> Result<Vec<Slot>, crate::error::ApiError> {
        let mut slots = Vec::with_capacity(n);
        for _ in 0..n {
            match self.slots.clone().try_acquire_owned() {
                Ok(permit) => slots.push(Slot { _permit: permit }),
                Err(_) => {
                    let mut e = crate::error::ApiError::rate_limited(FULL_RETRY_SECS);
                    e.message = format!(
                        "too many deployments are running; retry after {FULL_RETRY_SECS} seconds"
                    );
                    return Err(e);
                }
            }
        }
        Ok(slots)
    }

    /// Replaces the runner for deployments accepted from now on.
    pub fn set(&self, runner: Arc<dyn Runner>) {
        let mut current = self.runner.write().unwrap_or_else(|e| e.into_inner());
        *current = runner;
    }

    /// The runner for deployments accepted now.
    pub fn current(&self) -> Arc<dyn Runner> {
        self.runner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Whether the current runner has an executor.
    pub fn has_executor(&self) -> bool {
        self.current().has_executor()
    }

    /// Pins artifacts with the current runner.
    pub fn pin_artifacts(&self, deployment: Deployment) -> PinFuture {
        self.current().pin_artifacts(deployment)
    }

    /// Pins images with the current runner.
    pub fn pin_images(&self, deployment: Deployment) -> ImagePinFuture {
        self.current().pin_images(deployment)
    }

    /// Recovers a resource with the current runner.
    pub fn recover(&self, recovery: Recovery) -> RecoverFuture {
        self.current().recover(recovery)
    }

    /// The running deployments, as `app/env/id`.
    pub fn running(&self) -> Vec<String> {
        let active = self.active.lock().unwrap_or_else(|e| e.into_inner());
        active.iter().cloned().collect()
    }

    /// Waits up to `grace` for the running deployments to stop, and returns
    /// those still running.
    pub async fn wait_idle(&self, grace: Duration) -> Vec<String> {
        let deadline = tokio::time::Instant::now() + grace;
        loop {
            // Created before the check, so a deployment that stops in between
            // still wakes it.
            let stopped = self.stopped.notified();
            let running = self.running();
            if running.is_empty() {
                return running;
            }
            tokio::select! {
                () = stopped => {}
                () = tokio::time::sleep_until(deadline) => return self.running(),
            }
        }
    }

    /// How many accepted deployments wait for a slot.
    pub fn queued(&self) -> usize {
        self.queued.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// How many deployments may run at once.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Starts `job` in the background, in `slot`, or once a slot is free when
    /// it was accepted without one.
    pub fn spawn(&self, state: AppState, job: Job, slot: Option<Slot>) {
        let key = format!("{}/{}/{}", job.app, job.env, job.id);
        self.active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.clone());
        let runner = self.current();
        let slots = self.slots.clone();
        let queued = self.queued.clone();
        // Every event the deployment logs, the planner's and drivers'
        // included, names it and the request that accepted it.
        // Spans are at error level so they stay on whatever `RUST_LOG` hides.
        let span = tracing::error_span!(
            "deployment",
            app = %job.app,
            env = %job.env,
            deployment = %job.id,
            request_id = job.request_id.as_deref().unwrap_or_default(),
        );
        let inner = span.clone();
        tokio::spawn(
            async move {
                let waited = slot.is_none();
                let _slot = match slot {
                    Some(slot) => Some(slot),
                    // The semaphore is never closed; were it, the job runs anyway.
                    None => {
                        queued.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let slot = slots.acquire_owned().await.ok();
                        queued.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                        slot.map(|p| Slot { _permit: p })
                    }
                };
                // A job that waited for room does not start once the server is
                // stopping: it has changed nothing, so it fails and can be repeated.
                if waited && state.draining.load(std::sync::atomic::Ordering::SeqCst) {
                    finish(
                        &state,
                        &job,
                        Some(
                            "the server stopped before this started; nothing was changed, so \
                         repeat the request"
                                .into(),
                        ),
                    )
                    .await;
                } else if tokio::spawn(
                    timed(runner.run(state.clone(), job.clone())).instrument(inner),
                )
                .await
                .is_err()
                {
                    // A runner that panics must not leave the environment blocked.
                    finish(
                    &state,
                    &job,
                    Some(
                        "the deployment stopped unexpectedly; resources it was changing may have \
                         changed and need recovery before the environment is deployed again"
                            .into(),
                    ),
                )
                .await;
                }
                state
                    .runners
                    .active
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&key);
                state.runners.stopped.notify_waiters();
            }
            .instrument(span),
        );
    }
}

/// What previews know about the server's drivers.
#[derive(Debug)]
pub struct Drivers(RwLock<(Arc<DriverRegistry>, Option<String>)>);

impl Default for Drivers {
    fn default() -> Self {
        Self(RwLock::new((
            Arc::new(DriverRegistry::with_builtins()),
            None,
        )))
    }
}

impl Drivers {
    /// Replaces the drivers; `backend` is where the OpenTofu driver keeps
    /// state, as resources' footprints record it.
    pub fn set(&self, registry: Arc<DriverRegistry>, backend: Option<String>) {
        let mut current = self.0.write().unwrap_or_else(|e| e.into_inner());
        *current = (registry, backend);
    }

    /// The protocol version of driver `id`; `None` for a driver this server
    /// cannot run.
    pub fn protocol(&self, id: &str) -> Option<String> {
        let current = self.0.read().unwrap_or_else(|e| e.into_inner());
        Some(current.0.get(id)?.info().protocol_version.clone())
    }

    /// What a preview knows about driver `id`; `None` for a driver this
    /// server cannot run.
    pub fn facts(&self, id: &str) -> Option<DriverFacts> {
        let current = self.0.read().unwrap_or_else(|e| e.into_inner());
        let driver = current.0.get(id)?;
        Some(DriverFacts {
            preview: driver.info().capabilities.preview,
            backend: (id == "openporch/opentofu")
                .then(|| current.1.clone())
                .flatten(),
        })
    }
}

/// Checks destructive actions against the permits bound to their deployment,
/// recording a driver plan that no permit covers so the next preview shows it.
#[derive(Debug, Clone)]
pub struct StorePermits(pub SqliteStore);

impl PermitCheck for StorePermits {
    fn check<'a>(
        &'a self,
        req: PermitRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            let Some(deployment) = req.deployment else {
                return Err("a permit is used only by the deployment it is bound to".into());
            };
            let used = self
                .0
                .use_permit(PermitUse {
                    org: req.org,
                    app: req.app,
                    env: req.env,
                    deployment: Some(deployment),
                    presented: &[],
                    descriptor: req.descriptor,
                    guresid: req.guresid,
                    action: req.action.as_str(),
                    fingerprint: req.plan,
                })
                .await;
            let error = match used {
                Ok(()) => return Ok(()),
                Err(StoreError::ApprovalRequired(e)) => e,
                Err(e) => return Err(format!("the permit could not be checked: {e}")),
            };
            // A plan no permit covers shows in the next preview, so a permit
            // can be issued for it; an unknown impact has no fingerprint.
            let refused = match (req.plan, req.action) {
                (Some(fingerprint), _) => Some(fingerprint),
                (None, DestructiveAction::Unknown) => Some(""),
                (None, _) => None,
            };
            if let (Some(fingerprint), Some(guresid)) = (refused, req.guresid) {
                let request = ApprovalRequest {
                    descriptor: req.descriptor.to_owned(),
                    guresid: guresid.to_owned(),
                    action: req.action.as_str().to_owned(),
                    fingerprint: fingerprint.to_owned(),
                    actions: req.planned.to_vec(),
                    deployment_id: deployment.to_owned(),
                };
                if let Err(e) = self
                    .0
                    .record_approval_request(req.org, req.app, req.env, &request)
                    .await
                {
                    tracing::warn!(
                        descriptor = req.descriptor,
                        error = %e,
                        "could not record the plan that needs a permit"
                    );
                }
            }
            Err(format!(
                "{error}; preview the environment again and request a permit for this plan"
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_server_asks_callers_to_retry_later() {
        let runners = Runners::new(2);
        let mut held = runners.slots(2).unwrap();
        let e = runners.slots(1).unwrap_err();
        assert_eq!(e.status, axum::http::StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(e.retry_after, Some(FULL_RETRY_SECS));
        // Asking for more than is free takes nothing.
        drop(held.pop());
        assert!(runners.slots(2).is_err());
        assert_eq!(runners.slots(1).unwrap().len(), 1);
    }
}
