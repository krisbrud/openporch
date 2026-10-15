//! Environments: creation (empty or cloned from a deployment), listing, the runtime
//! summary and deletion.

use std::collections::BTreeMap;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use openporch_core::graph::ResourceGraph;
use openporch_core::sets::{DeploymentSet, diff_sets};
use openporch_store::{
    Acceptance, ActiveResource, DeploymentKind, DeploymentRecord, DeploymentRequest,
    DeploymentStatus, DraftDelta, EnvRecord, IdentityStore, NewEnv, OrgRole, Store, now_ms,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{check_id, deploys, plans};
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
pub struct UpdateEnv {
    name: String,
}

/// `PATCH /orgs/{org}/apps/{app}/envs/{env}`: renames an environment. Needs
/// `developer` or `owner` on the application and `deployer` on its type.
pub async fn update(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, app, env)): Path<(String, String, String)>,
    JsonBody(body): JsonBody<UpdateEnv>,
) -> ApiResult<Json<EnvView>> {
    let name = super::apps::display_name("environment", &body.name)?;
    let org = state.org();
    state
        .store
        .update_env(
            org,
            &app,
            &env,
            name,
            ctx.audit(org, "env.update", format!("{app}/{env}")),
        )
        .await?;
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
    let name = match &body.name {
        Some(name) => super::apps::display_name("environment", name)?.to_owned(),
        None => body.id.clone(),
    };
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
                name,
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

/// `DELETE /orgs/{org}/apps/{app}/envs/{env}`, with the generation the
/// client read. A deployment deletes the environment's owned resources in
/// reverse dependency order and then the environment; one without resources
/// is deleted at once. If a deletion fails, the environment stays in
/// `deletion_failed` with what is left, and the request can be repeated. The
/// route requires the deployer gate on the environment's type; `development`
/// stays while its application exists.
pub async fn delete(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, app, env)): Path<(String, String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<EnvDeletion>)> {
    let org = state.org();
    if body.iter().all(u8::is_ascii_whitespace) {
        return Err(ApiError::invalid(
            "deleting an environment needs a body with `expected_generation` and the \
             permits for deleting its resources",
        ));
    }
    let mut req: DeleteEnvRequest =
        serde_json::from_slice(&body).map_err(|e| ApiError::invalid(e.to_string()))?;
    // Permits are a set; their order does not make a different request.
    req.permits.sort();
    req.permits.dedup();
    let key = deploys::idempotency_key(&headers)?;
    let fingerprint = {
        let mut body = serde_json::to_value(&req).map_err(ApiError::internal)?;
        body["kind"] = json!("delete_env");
        deploys::fingerprint(&body)
    };
    // A retry returns the original deletion while the environment exists;
    // once it is gone, the environment is `404`.
    if let Some(key) = &key
        && let Some((prior, prior_fingerprint)) = state
            .store
            .idempotent_deployment(org, &app, &env, &ctx.principal.id, key)
            .await?
    {
        if prior_fingerprint != fingerprint {
            return Err(ApiError::conflict(format!(
                "idempotency key `{key}` was used for a different request"
            )));
        }
        let retained = retained(&state, &prior).await?;
        return Ok((
            StatusCode::ACCEPTED,
            Json(EnvDeletion {
                deployment: prior,
                retained,
            }),
        ));
    }

    let env_record = env_or_404(&state, &app, &env).await?;
    let slot = state.runners.slots(1)?.pop();
    let snapshot = deletion_snapshot(&req.permits);
    let entries = plans::preflight(&state, &env_record, String::new(), &snapshot, true).await?;
    let acceptance = state
        .store
        .accept_deployment(DeploymentRequest {
            permits: plans::binding(&entries, &req.permits),
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
    let deletion = start_deletion(&state, &acceptance, snapshot, slot).await?;
    Ok((StatusCode::ACCEPTED, Json(deletion)))
}

/// The snapshot of an environment deletion: no graph, only the permits.
pub(super) fn deletion_snapshot(permits: &[String]) -> Value {
    json!({
        "version": deploys::SNAPSHOT_VERSION,
        "kind": "delete_env",
        "permits": permits,
        "graph": ResourceGraph { nodes: BTreeMap::new(), layers: Vec::new() },
        "definitions": {},
    })
}

/// Starts an accepted environment deletion and describes it. A replayed one
/// already runs, or has finished.
pub(super) async fn start_deletion(
    state: &AppState,
    acceptance: &Acceptance,
    snapshot: Value,
    slot: Option<crate::deploy::Slot>,
) -> ApiResult<EnvDeletion> {
    let org = state.org();
    let mut deployment = acceptance.deployment().clone();
    let Acceptance::Created(d) = acceptance else {
        let retained = retained(state, &deployment).await?;
        return Ok(EnvDeletion {
            deployment,
            retained,
        });
    };
    let (app, env) = (&d.app, &d.env);
    // Accepting the deletion stopped anything else from running in the
    // environment, so what it holds now is what the deletion works on. One
    // without resources has nothing to delete: it is finished here, which
    // removes it, and finishing checks again that nothing it must delete is
    // left. Otherwise, or if that cannot be done, the runner does the
    // deletion, so a store error never leaves it unstarted.
    let resources = state.store.active_resources(org, app, env).await;
    let finished = match &resources {
        Ok(resources) if resources.is_empty() => state
            .store
            .finish_deployment(org, app, env, &d.id, None)
            .await
            .is_ok(),
        _ => false,
    };
    if !finished {
        state.runners.spawn(
            state.clone(),
            Job {
                app: app.clone(),
                env: env.clone(),
                id: d.id.clone(),
                snapshot,
            },
            slot,
        );
    }
    let retained = retained_of(resources?);
    if finished {
        match state.store.deployment(org, app, env, &d.id).await? {
            // Something was left to delete after all.
            Some(failed) => deployment = failed,
            // The environment is gone with its deployments.
            None => {
                deployment.status = DeploymentStatus::Succeeded;
                deployment.finished_at = Some(now_ms());
            }
        }
    }
    Ok(EnvDeletion {
        deployment,
        retained,
    })
}

/// The resources `deletion` leaves in place: those of its environment it does
/// not delete.
async fn retained(state: &AppState, deletion: &DeploymentRecord) -> ApiResult<Vec<Retained>> {
    let resources = state
        .store
        .active_resources(state.org(), &deletion.app, &deletion.env)
        .await?;
    Ok(retained_of(resources))
}

/// What a deletion of an environment holding `resources` leaves in place.
fn retained_of(resources: Vec<ActiveResource>) -> Vec<Retained> {
    resources
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
        .collect()
}
