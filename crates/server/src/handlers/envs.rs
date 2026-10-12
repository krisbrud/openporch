//! Environments: creation (empty or cloned from a deployment), listing, the runtime
//! summary and deletion.

use std::collections::BTreeMap;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use openporch_core::graph::ResourceGraph;
use openporch_core::sets::{DeploymentSet, diff_sets};
use openporch_store::{
    Acceptance, DeploymentKind, DeploymentRecord, DeploymentRequest, DeploymentStatus, DraftDelta,
    EnvRecord, IdentityStore, NewEnv, OrgRole, Store, StoreError,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{check_id, deploys};
use crate::api::{Ctx, JsonBody};
use crate::auth;
use crate::deploy::Job;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// The latest deployment attempt, as shown in the summary.
#[derive(Serialize)]
pub struct Attempt {
    id: String,
    /// `deploy`, or `delete_env` for an environment deletion.
    kind: DeploymentKind,
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
            kind: d.kind,
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

pub(super) async fn env_or_404(state: &AppState, app: &str, env: &str) -> ApiResult<EnvRecord> {
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
            NewEnv {
                id: body.id.clone(),
                name: body.name.clone().unwrap_or_else(|| body.id.clone()),
                env_type: body.env_type.clone(),
                draft,
            },
            ctx.audit(org, "env.create", format!("{app}/{}", body.id)),
        )
        .await?;
    let mut created = view(&state, env_or_404(&state, &app, &body.id).await?).await?;
    created.delta_id = delta_id;
    Ok((StatusCode::CREATED, Json(created)))
}

/// The body of `DELETE .../envs/{env}`, required once the environment has
/// been deployed.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteEnvRequest {
    /// The environment generation the client read.
    expected_generation: i64,
    /// Destructive-action permits for deleting its resources.
    #[serde(default)]
    permits: Vec<String>,
    #[serde(default)]
    comment: String,
}

/// A resource the deletion leaves in place: external, or adopted and not
/// scheduled for deletion. It is released from management when the
/// environment is removed.
#[derive(Debug, Serialize)]
pub struct Retained {
    descriptor: String,
    guresid: String,
    ownership: String,
}

/// An accepted environment deletion.
#[derive(Debug, Serialize)]
pub struct EnvDeletion {
    /// The deployment that deletes the environment's resources and then the
    /// environment.
    deployment: DeploymentRecord,
    /// What it leaves in place.
    retained: Vec<Retained>,
}

/// `DELETE /orgs/{org}/apps/{app}/envs/{env}`. An environment that has never
/// been deployed is removed at once. Otherwise a deployment deletes its owned
/// resources in reverse dependency order and then the environment; if a
/// deletion fails, the environment stays in `deletion_failed` with what is
/// left, and the request can be repeated. The route requires the deployer gate
/// on the environment's type; `development` stays while its application exists.
pub async fn delete(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, app, env)): Path<(String, String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let org = state.org();
    let mut req: Option<DeleteEnvRequest> = if body.iter().all(u8::is_ascii_whitespace) {
        None
    } else {
        Some(serde_json::from_slice(&body).map_err(|e| ApiError::invalid(e.to_string()))?)
    };
    let key = deploys::idempotency_key(&headers)?;
    let fingerprint = match &mut req {
        Some(req) => {
            // Permits are a set; their order does not make a different request.
            req.permits.sort();
            req.permits.dedup();
            let mut body = serde_json::to_value(&*req).map_err(ApiError::internal)?;
            body["kind"] = json!("delete_env");
            Some(deploys::fingerprint(&body))
        }
        None => None,
    };
    // A retry returns the original deletion while the environment exists;
    // once it is gone, the environment is `404`.
    if let (Some(key), Some(fingerprint)) = (&key, &fingerprint)
        && let Some((prior, prior_fingerprint)) = state
            .store
            .idempotent_deployment(org, &app, &env, &ctx.principal.id, key)
            .await?
    {
        if &prior_fingerprint != fingerprint {
            return Err(ApiError::conflict(format!(
                "idempotency key `{key}` was used for a different request"
            )));
        }
        let retained = retained(&state, &app, &env).await?;
        return Ok((
            StatusCode::ACCEPTED,
            Json(EnvDeletion {
                deployment: prior,
                retained,
            }),
        )
            .into_response());
    }

    let record = env_or_404(&state, &app, &env).await?;
    let deployed = state
        .store
        .latest_deployment(org, &app, &env)
        .await?
        .is_some()
        || !state
            .store
            .active_resources(org, &app, &env)
            .await?
            .is_empty();
    if !deployed {
        if let Some(req) = &req
            && req.expected_generation != record.generation
        {
            return Err(StoreError::StaleGeneration {
                expected: req.expected_generation,
                actual: record.generation,
            }
            .into());
        }
        // The store checks again that nothing was deployed meanwhile.
        state
            .store
            .delete_env(
                org,
                &app,
                &env,
                ctx.audit(org, "env.delete", format!("{app}/{env}")),
            )
            .await?;
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    let (Some(req), Some(fingerprint)) = (req, fingerprint) else {
        return Err(ApiError::invalid(format!(
            "environment `{app}/{env}` has been deployed; deleting it deletes its resources and \
             needs a body with `expected_generation` and the permits for them"
        )));
    };
    let snapshot = json!({
        "version": deploys::SNAPSHOT_VERSION,
        "kind": "delete_env",
        "permits": req.permits,
        "graph": ResourceGraph { nodes: BTreeMap::new(), layers: Vec::new() },
        "definitions": {},
    });
    let acceptance = state
        .store
        .accept_deployment(DeploymentRequest {
            org: org.to_owned(),
            app: app.clone(),
            env: env.clone(),
            actor: ctx.principal.id.clone(),
            expected_generation: req.expected_generation,
            idempotency_key: key,
            fingerprint,
            kind: DeploymentKind::DeleteEnv,
            set_id: None,
            delta_id: None,
            delta_content: None,
            comment: req.comment,
            snapshot: snapshot.clone(),
            audit: ctx.audit(org, "env.delete", format!("{app}/{env}")),
        })
        .await?;
    let retained = retained(&state, &app, &env).await?;
    if let Acceptance::Created(d) = &acceptance {
        state.runners.spawn(
            state.clone(),
            Job {
                app,
                env,
                id: d.id.clone(),
                snapshot,
            },
        );
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(EnvDeletion {
            deployment: acceptance.deployment().clone(),
            retained,
        }),
    )
        .into_response())
}

/// The resources deleting `app/env` leaves in place.
async fn retained(state: &AppState, app: &str, env: &str) -> ApiResult<Vec<Retained>> {
    Ok(state
        .store
        .active_resources(state.org(), app, env)
        .await?
        .into_iter()
        .filter(|r| match r.ownership.as_str() {
            "owned" => false,
            "adopted" => !r.scheduled_deletion,
            _ => true,
        })
        .map(|r| Retained {
            descriptor: r.descriptor,
            guresid: r.guresid,
            ownership: r.ownership,
        })
        .collect())
}
