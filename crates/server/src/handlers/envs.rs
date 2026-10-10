//! Environments: creation (empty or cloned from a deployment), listing, the runtime
//! summary and deletion.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use openporch_core::sets::{DeploymentSet, diff_sets};
use openporch_store::{
    DeploymentRecord, DeploymentStatus, DraftDelta, EnvRecord, IdentityStore, OrgRole, Store,
};
use serde::{Deserialize, Serialize};

use super::check_id;
use crate::api::{Ctx, JsonBody};
use crate::auth;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// The latest deployment attempt, as shown in the summary.
#[derive(Serialize)]
pub struct Attempt {
    id: String,
    status: DeploymentStatus,
    created_at: i64,
    finished_at: Option<i64>,
}

/// An environment with its runtime summary.
#[derive(Serialize)]
pub struct EnvView {
    app: String,
    id: String,
    name: String,
    #[serde(rename = "type")]
    env_type: String,
    state: String,
    generation: i64,
    desired_set_id: Option<String>,
    last_successful_set_id: Option<String>,
    last_successful_deploy_id: Option<String>,
    latest_deployment: Option<Attempt>,
    /// `never_deployed`, `converged`, `partial` or `unknown`.
    convergence: &'static str,
    /// The draft delta a clone created.
    #[serde(skip_serializing_if = "Option::is_none")]
    delta_id: Option<String>,
}

/// Convergence from the latest attempt: a failed attempt may have applied some
/// resources, so it is `partial`, never shown as deployed.
fn convergence(env: &EnvRecord, latest: Option<&DeploymentRecord>) -> &'static str {
    match latest.map(|d| d.status) {
        None => "never_deployed",
        Some(DeploymentStatus::InProgress) => "unknown",
        Some(DeploymentStatus::Failed) => "partial",
        Some(DeploymentStatus::Succeeded)
            if env.desired_set_id.is_some() && env.desired_set_id == env.last_successful_set_id =>
        {
            "converged"
        }
        Some(DeploymentStatus::Succeeded) => "unknown",
    }
}

async fn view(state: &AppState, env: EnvRecord) -> ApiResult<EnvView> {
    let latest = state
        .store
        .latest_deployment(state.org(), &env.app, &env.id)
        .await?;
    Ok(EnvView {
        convergence: convergence(&env, latest.as_ref()),
        latest_deployment: latest.map(|d| Attempt {
            id: d.id,
            status: d.status,
            created_at: d.created_at,
            finished_at: d.finished_at,
        }),
        app: env.app,
        id: env.id,
        name: env.name,
        env_type: env.env_type,
        state: env.state,
        generation: env.generation,
        desired_set_id: env.desired_set_id,
        last_successful_set_id: env.last_successful_set_id,
        last_successful_deploy_id: env.last_successful_deploy_id,
        delta_id: None,
    })
}

async fn env_or_404(state: &AppState, app: &str, env: &str) -> ApiResult<EnvRecord> {
    state
        .store
        .env(state.org(), app, env)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("environment `{app}/{env}` not found")))
}

pub async fn list(
    State(state): State<AppState>,
    Path((_, app)): Path<(String, String)>,
) -> ApiResult<Json<Vec<EnvView>>> {
    let mut out = Vec::new();
    for env in state.store.envs(state.org(), &app).await? {
        out.push(view(&state, env).await?);
    }
    Ok(Json(out))
}

pub async fn get(
    State(state): State<AppState>,
    Path((_, app, env)): Path<(String, String, String)>,
) -> ApiResult<Json<EnvView>> {
    let env = env_or_404(&state, &app, &env).await?;
    Ok(Json(view(&state, env).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateEnv {
    id: String,
    name: Option<String>,
    #[serde(rename = "type")]
    env_type: String,
    from_deploy_id: Option<String>,
}

/// Creates an environment. Needs `developer` or `owner` on the application (the
/// route) and `deployer` on the environment type. With `from_deploy_id`, a draft
/// delta that reproduces that deployment's set is created; nothing is deployed.
pub async fn create(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, app)): Path<(String, String)>,
    JsonBody(body): JsonBody<CreateEnv>,
) -> ApiResult<(StatusCode, Json<EnvView>)> {
    check_id("environment", &body.id)?;
    let org = state.org();
    if state.store.env_type(org, &body.env_type).await?.is_none() {
        return Err(ApiError::invalid(format!(
            "environment type `{}` does not exist",
            body.env_type
        )));
    }
    if ctx.roles.org != Some(OrgRole::Administrator) && !ctx.roles.deployer.contains(&body.env_type)
    {
        return Err(ApiError::forbidden(format!(
            "requires the `deployer` role on environment type `{}`",
            body.env_type
        )));
    }
    // Only deployments of this application can be cloned, so a foreign ID reveals
    // nothing.
    let draft = match &body.from_deploy_id {
        None => None,
        Some(deploy_id) => {
            let source = state
                .store
                .app_deployment(org, &app, deploy_id)
                .await?
                .ok_or_else(|| {
                    ApiError::not_found(format!("deployment `{deploy_id}` not found in `{app}`"))
                })?;
            let set_id = source.set_id.ok_or_else(|| {
                ApiError::conflict(format!("deployment `{deploy_id}` has no deployment set"))
            })?;
            let set = state
                .store
                .set(org, &app, &set_id)
                .await?
                .ok_or_else(|| ApiError::internal(format!("set `{set_id}` is missing")))?;
            let delta = diff_sets(&DeploymentSet::default(), &set);
            Some(DraftDelta {
                id: format!("clone-{}", auth::random_id(6)),
                content: serde_json::to_value(delta).map_err(ApiError::internal)?,
            })
        }
    };
    let delta_id = draft.as_ref().map(|d| d.id.clone());
    state
        .store
        .create_env(
            org,
            &app,
            &body.id,
            body.name.as_deref().unwrap_or(&body.id),
            &body.env_type,
            draft,
            ctx.audit(org, "env.create", format!("{app}/{}", body.id)),
        )
        .await?;
    let mut created = view(&state, env_or_404(&state, &app, &body.id).await?).await?;
    created.delta_id = delta_id;
    Ok((StatusCode::CREATED, Json(created)))
}

/// Deletes an environment that has never been deployed. The route requires the
/// deployer gate on the environment's type; `development` stays while its
/// application exists.
pub async fn delete(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, app, env)): Path<(String, String, String)>,
) -> ApiResult<StatusCode> {
    let org = state.org();
    state
        .store
        .delete_env(
            org,
            &app,
            &env,
            ctx.audit(org, "env.delete", format!("{app}/{env}")),
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
