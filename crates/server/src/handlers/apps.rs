//! Applications, environment types and their role assignments.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use openporch_store::{
    AppDeletionRequest, AppRecord, AppRole, EnvTypeRecord, IdentityStore, Store,
};
use serde::{Deserialize, Serialize};

use super::check_id;
use super::envs::{self, EnvDeletion};
use crate::api::{Ctx, JsonBody};
use crate::error::{ApiError, ApiResult};
use crate::rbac;
use crate::state::AppState;

async fn app_or_404(state: &AppState, app: &str) -> ApiResult<AppRecord> {
    state
        .store
        .app(state.org(), app)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("application `{app}` not found")))
}

async fn env_type_or_404(state: &AppState, id: &str) -> ApiResult<EnvTypeRecord> {
    state
        .store
        .env_type(state.org(), id)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("environment type `{id}` not found")))
}

/// Applications the caller may see.
pub async fn list(State(state): State<AppState>, ctx: Ctx) -> ApiResult<Json<Vec<AppRecord>>> {
    let apps = state.store.apps(state.org()).await?;
    Ok(Json(
        apps.into_iter()
            .filter(|a| rbac::can_see_app(&ctx.roles, &a.id))
            .collect(),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateApp {
    id: String,
    name: Option<String>,
}

/// Creates an application with its `development` environment; the creator becomes
/// its owner.
pub async fn create(
    State(state): State<AppState>,
    ctx: Ctx,
    JsonBody(body): JsonBody<CreateApp>,
) -> ApiResult<(StatusCode, Json<AppRecord>)> {
    check_id("application", &body.id)?;
    let org = state.org();
    let name = body.name.unwrap_or_else(|| body.id.clone());
    state
        .store
        .create_app(
            org,
            &body.id,
            &name,
            &ctx.principal.id,
            ctx.audit(org, "app.create", &body.id),
        )
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(app_or_404(&state, &body.id).await?),
    ))
}

pub async fn get(
    State(state): State<AppState>,
    Path((_, app)): Path<(String, String)>,
) -> ApiResult<Json<AppRecord>> {
    Ok(Json(app_or_404(&state, &app).await?))
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteAppRequest {
    /// Destructive-action permits for deleting the resources of every
    /// environment.
    #[serde(default)]
    permits: Vec<String>,
    #[serde(default)]
    comment: String,
}

/// An accepted application deletion.
#[derive(Debug, Serialize)]
pub struct AppDeletion {
    /// Each environment's deletion.
    deletions: Vec<EnvDeletion>,
    /// Whether the application is already gone.
    removed: bool,
}

/// `DELETE /orgs/{org}/apps/{app}`, optionally with permits. Requires `owner`
/// and `deployer` on every environment type the application uses. Every
/// environment is deleted as by its own deletion, `development` included, and
/// the application is removed once the last of them is gone. Until then it
/// takes no new environments or deployments; a failed environment deletion
/// keeps the environment, and repeating the request retries it.
pub async fn delete(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, app)): Path<(String, String)>,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<AppDeletion>)> {
    let org = state.org();
    let mut req: DeleteAppRequest = if body.iter().all(u8::is_ascii_whitespace) {
        DeleteAppRequest::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| ApiError::invalid(e.to_string()))?
    };
    req.permits.sort();
    req.permits.dedup();
    app_or_404(&state, &app).await?;
    if !ctx.is_admin() {
        let types = state.store.app_env_types(org, &app).await?;
        if let Some(t) = types.iter().find(|t| !ctx.roles.deployer.contains(*t)) {
            return Err(ApiError::forbidden(format!(
                "requires the `deployer` role on environment type `{t}`"
            )));
        }
    }
    let snapshot = envs::deletion_snapshot(&req.permits);
    let accepted = state
        .store
        .accept_app_deletion(AppDeletionRequest {
            org: org.to_owned(),
            app: app.clone(),
            actor: ctx.principal.id.clone(),
            comment: req.comment,
            snapshot: snapshot.clone(),
            audit: ctx.audit(org, "app.delete", &app),
        })
        .await?;
    // Every deletion is started before any of them is described, so an
    // error describing one never leaves another unstarted.
    let mut deletions = Vec::with_capacity(accepted.deletions.len());
    let mut failure = None;
    for acceptance in &accepted.deletions {
        match envs::start_deletion(&state, acceptance, snapshot.clone()).await {
            Ok(d) => deletions.push(d),
            Err(e) => failure = failure.or(Some(e)),
        }
    }
    if let Some(e) = failure {
        return Err(e);
    }
    let removed = accepted.removed || state.store.app(org, &app).await?.is_none();
    Ok((
        StatusCode::ACCEPTED,
        Json(AppDeletion { deletions, removed }),
    ))
}

#[derive(Serialize)]
pub struct RoleAssignment {
    user: String,
    role: &'static str,
}

pub async fn users(
    State(state): State<AppState>,
    Path((_, app)): Path<(String, String)>,
) -> ApiResult<Json<Vec<RoleAssignment>>> {
    app_or_404(&state, &app).await?;
    Ok(Json(
        state
            .store
            .app_roles(state.org(), &app)
            .await?
            .into_iter()
            .map(|(user, role)| RoleAssignment {
                user,
                role: role.as_str(),
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddAppUser {
    id: String,
    role: AppRole,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateAppUser {
    role: AppRole,
}

async fn set_app_role(
    state: &AppState,
    ctx: &Ctx,
    app: &str,
    user: &str,
    role: AppRole,
    action: &str,
) -> ApiResult<RoleAssignment> {
    let org = state.org();
    app_or_404(state, app).await?;
    if state.store.principal(org, user).await?.is_none() {
        return Err(ApiError::not_found(format!("user `{user}` not found")));
    }
    state
        .store
        .set_app_role(
            org,
            app,
            user,
            role,
            ctx.audit(org, action, format!("{app}/{user}")),
        )
        .await?;
    Ok(RoleAssignment {
        user: user.to_owned(),
        role: role.as_str(),
    })
}

pub async fn add_user(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, app)): Path<(String, String)>,
    JsonBody(body): JsonBody<AddAppUser>,
) -> ApiResult<(StatusCode, Json<RoleAssignment>)> {
    if state
        .store
        .app_roles(state.org(), &app)
        .await?
        .iter()
        .any(|(u, _)| *u == body.id)
    {
        return Err(ApiError::conflict(format!(
            "`{}` already has a role on `{app}`",
            body.id
        )));
    }
    let r = set_app_role(&state, &ctx, &app, &body.id, body.role, "app.user.create").await?;
    Ok((StatusCode::CREATED, Json(r)))
}

pub async fn update_user(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, app, user)): Path<(String, String, String)>,
    JsonBody(body): JsonBody<UpdateAppUser>,
) -> ApiResult<Json<RoleAssignment>> {
    if !state
        .store
        .app_roles(state.org(), &app)
        .await?
        .iter()
        .any(|(u, _)| *u == user)
    {
        return Err(ApiError::not_found(format!(
            "`{user}` has no role on `{app}`"
        )));
    }
    Ok(Json(
        set_app_role(&state, &ctx, &app, &user, body.role, "app.user.update").await?,
    ))
}

pub async fn remove_user(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, app, user)): Path<(String, String, String)>,
) -> ApiResult<StatusCode> {
    let org = state.org();
    state
        .store
        .remove_app_role(
            org,
            &app,
            &user,
            ctx.audit(org, "app.user.delete", format!("{app}/{user}")),
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn env_types(State(state): State<AppState>) -> ApiResult<Json<Vec<EnvTypeRecord>>> {
    Ok(Json(state.store.env_types(state.org()).await?))
}

pub async fn env_type(
    State(state): State<AppState>,
    Path((_, id)): Path<(String, String)>,
) -> ApiResult<Json<EnvTypeRecord>> {
    Ok(Json(env_type_or_404(&state, &id).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateEnvType {
    id: String,
    #[serde(default)]
    description: String,
}

pub async fn create_env_type(
    State(state): State<AppState>,
    ctx: Ctx,
    JsonBody(body): JsonBody<CreateEnvType>,
) -> ApiResult<(StatusCode, Json<EnvTypeRecord>)> {
    check_id("environment type", &body.id)?;
    let org = state.org();
    state
        .store
        .create_env_type(
            org,
            &body.id,
            &body.description,
            ctx.audit(org, "env_type.create", &body.id),
        )
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(env_type_or_404(&state, &body.id).await?),
    ))
}

/// Deletes an environment type no environment uses.
pub async fn delete_env_type(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, id)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let org = state.org();
    state
        .store
        .delete_env_type(org, &id, ctx.audit(org, "env_type.delete", &id))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn env_type_users(
    State(state): State<AppState>,
    Path((_, id)): Path<(String, String)>,
) -> ApiResult<Json<Vec<RoleAssignment>>> {
    env_type_or_404(&state, &id).await?;
    Ok(Json(
        state
            .store
            .env_type_roles(state.org(), &id)
            .await?
            .into_iter()
            .map(|user| RoleAssignment {
                user,
                role: "deployer",
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddEnvTypeUser {
    id: String,
    role: String,
}

pub async fn add_env_type_user(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, id)): Path<(String, String)>,
    JsonBody(body): JsonBody<AddEnvTypeUser>,
) -> ApiResult<(StatusCode, Json<RoleAssignment>)> {
    if body.role != "deployer" {
        return Err(ApiError::invalid(
            "the only environment type role is `deployer`",
        ));
    }
    let org = state.org();
    env_type_or_404(&state, &id).await?;
    if state.store.principal(org, &body.id).await?.is_none() {
        return Err(ApiError::not_found(format!("user `{}` not found", body.id)));
    }
    state
        .store
        .set_env_type_role(
            org,
            &id,
            &body.id,
            ctx.audit(org, "env_type.user.create", format!("{id}/{}", body.id)),
        )
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(RoleAssignment {
            user: body.id,
            role: "deployer",
        }),
    ))
}

pub async fn remove_env_type_user(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, id, user)): Path<(String, String, String)>,
) -> ApiResult<StatusCode> {
    let org = state.org();
    state
        .store
        .remove_env_type_role(
            org,
            &id,
            &user,
            ctx.audit(org, "env_type.user.delete", format!("{id}/{user}")),
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
