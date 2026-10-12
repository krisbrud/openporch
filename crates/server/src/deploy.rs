//! Running accepted deployments in the background.
//!
//! Acceptance returns at once; a [`Runner`] then converges the environment and
//! finishes the deployment. Provisioning resources arrives with the planner, so
//! until then the default runner fails every deployment it is handed, which keeps
//! the environment free for the next one.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use openporch_engine::planner::{Deployment, Executor, NoPermits, PermitCheck, Planner};
use openporch_store::{SqliteStore, Store};
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

/// The runner until the planner exists: nothing is provisioned, so every
/// deployment fails with an explanation.
#[derive(Debug)]
pub struct Unimplemented;

impl Runner for Unimplemented {
    fn run(&self, state: AppState, job: Job) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(async move {
            finish(
                &state,
                &job,
                Some("provisioning resources is not implemented yet; nothing changed".into()),
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
                Err(error) => finish(&state, &job, Some(format!("{error}; nothing changed"))).await,
            }
        })
    }
}

/// The planner's view of `job`, from its execution snapshot.
async fn deployment(state: &AppState, job: &Job) -> Result<Deployment, String> {
    let org = state.org();
    let env = state
        .store
        .env(org, &job.app, &job.env)
        .await
        .map_err(|e| format!("the environment could not be read: {e}"))?
        .ok_or_else(|| format!("environment `{}/{}` no longer exists", job.app, job.env))?;
    let set_id = state
        .store
        .deployment(org, &job.app, &job.env, &job.id)
        .await
        .map_err(|e| format!("the deployment could not be read: {e}"))?
        .and_then(|d| d.set_id)
        .unwrap_or_default();
    let invalid = |what: &str| format!("the stored {what} ID is invalid");
    Deployment::from_snapshot(
        org.parse().map_err(|_| invalid("organization"))?,
        job.app.parse().map_err(|_| invalid("application"))?,
        job.env.parse().map_err(|_| invalid("environment"))?,
        env.env_type
            .parse()
            .map_err(|_| invalid("environment type"))?,
        job.id.clone(),
        set_id,
        &job.snapshot,
    )
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

/// Checks destructive-action permits presented to the API, such as for a
/// detach. Refuses everything until permits can be issued.
#[derive(Debug)]
pub struct Permits(RwLock<Arc<dyn PermitCheck>>);

impl Default for Permits {
    fn default() -> Self {
        Self(RwLock::new(Arc::new(NoPermits)))
    }
}

impl Permits {
    /// Replaces the check.
    pub fn set(&self, check: Arc<dyn PermitCheck>) {
        let mut current = self.0.write().unwrap_or_else(|e| e.into_inner());
        *current = check;
    }

    /// The current check.
    pub fn get(&self) -> Arc<dyn PermitCheck> {
        self.0.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
}
