//! Running accepted deployments in the background.
//!
//! Acceptance returns at once; a [`Runner`] then converges the environment and
//! finishes the deployment. A server that starts with its master keys runs
//! deployments through the planner and the local executor; until then the
//! default runner fails every deployment it is handed, which keeps the
//! environment free for the next one.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use openporch_drivers::DriverRegistry;
use openporch_engine::planner::{
    Deployment, DestructiveAction, Executor, PermitCheck, PermitRequest, Planner,
};
use openporch_engine::preview::DriverFacts;
use openporch_store::{
    ApprovalRequest, DeploymentKind, PermitStore, PermitUse, SqliteStore, Store, StoreError,
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
}

/// Runs accepted deployments to completion.
pub trait Runner: Send + Sync + std::fmt::Debug {
    /// Runs `job` and finishes its deployment.
    fn run(&self, state: AppState, job: Job) -> Pin<Box<dyn Future<Output = ()> + Send>>;
}

/// The runner without an executor: nothing is provisioned, so every
/// deployment fails with an explanation.
#[derive(Debug)]
pub struct Unimplemented;

impl Runner for Unimplemented {
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

/// Finishes `job`'s deployment, which ends its log with the outcome; a
/// failure to record it leaves the deployment running until restart recovery
/// fails it.
pub async fn finish(state: &AppState, job: &Job, error: Option<String>) {
    if let Err(e) = state
        .store
        .finish_deployment(state.org(), &job.app, &job.env, &job.id, error)
        .await
    {
        tracing::error!(deployment = %job.id, error = %e, "could not finish a deployment");
    }
}

/// The runner a server hands accepted deployments to.
#[derive(Debug)]
pub struct Runners(RwLock<Arc<dyn Runner>>);

impl Default for Runners {
    fn default() -> Self {
        Self(RwLock::new(Arc::new(Unimplemented)))
    }
}

impl Runners {
    /// Replaces the runner for deployments accepted from now on.
    pub fn set(&self, runner: Arc<dyn Runner>) {
        let mut current = self.0.write().unwrap_or_else(|e| e.into_inner());
        *current = runner;
    }

    /// Starts `job` in the background.
    pub fn spawn(&self, state: AppState, job: Job) {
        let runner = self.0.read().unwrap_or_else(|e| e.into_inner()).clone();
        let run = tokio::spawn(runner.run(state.clone(), job.clone()));
        // A runner that panics must not leave the environment blocked.
        tokio::spawn(async move {
            if run.await.is_err() {
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
        });
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
