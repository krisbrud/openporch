//! Running accepted deployments in the background.
//!
//! Acceptance returns at once; a [`Runner`] then converges the environment and
//! finishes the deployment. Provisioning resources arrives with the planner, so
//! until then the default runner fails every deployment it is handed, which keeps
//! the environment free for the next one.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use openporch_store::Store;
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
                Some("provisioning resources is not implemented yet".into()),
            )
            .await;
        })
    }
}

/// Finishes `job`'s deployment; a failure to record it leaves the deployment
/// running until restart recovery fails it.
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
                    Some("the deployment stopped unexpectedly".into()),
                )
                .await;
            }
        });
    }
}
