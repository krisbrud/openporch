//! Shared values of applications and environments, with value set versions.
//!
//! A value is plain text (`value`) or a secret (`secret_ref`: a reference
//! `{store, ref, version?}` or `{value}` to write to the primary store). Secrets are
//! returned as references only. Changing an environment's values requires
//! `deployer` on its type; changing application values requires `deployer` on the
//! type of every environment of the application.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use openporch_store::{
    OrgRole, PutValue, Store, ValueContent, ValueRecord, ValueSetVersion, ValueStore,
    value_secret_prefix,
};
use serde::Deserialize;
use serde_json::Value;

use super::envs::env_or_404;
use crate::api::{Ctx, JsonBody};
use crate::error::{ApiError, ApiResult};
use crate::secret_inputs::{self, PlannedSecret};
use crate::state::AppState;

/// The path segment listing versions, which no key may take.
const VERSIONS: &str = "versions";

/// Longest key.
const MAX_KEY_LEN: usize = 100;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValueBody {
    key: Option<String>,
    #[serde(default)]
    description: String,
    value: Option<String>,
    secret_ref: Option<Value>,
}

fn check_key(key: &str) -> ApiResult<()> {
    let mut chars = key.chars();
    let valid = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        && key.len() <= MAX_KEY_LEN
        && key != VERSIONS;
    if valid {
        Ok(())
    } else {
        Err(ApiError::invalid_id(format!(
            "value key `{key}` is invalid: use up to {MAX_KEY_LEN} letters, digits, `_` and \
             `-`, starting with a letter or `_`, other than `{VERSIONS}`"
        )))
    }
}

/// Requires `deployer` on the type of `env`, or on every environment type of the
/// application for application values. Administrators pass.
async fn check_deployer(
    state: &AppState,
    ctx: &Ctx,
    app: &str,
    env: Option<&str>,
) -> ApiResult<()> {
    if ctx.roles.org == Some(OrgRole::Administrator) {
        return Ok(());
    }
    let types: Vec<String> = match env {
        Some(env) => vec![env_or_404(state, app, env).await?.env_type],
        None => state
            .store
            .envs(state.org(), app)
            .await?
            .into_iter()
            .map(|e| e.env_type)
            .collect(),
    };
    match types.iter().find(|t| !ctx.roles.deployer.contains(*t)) {
        None => Ok(()),
        Some(t) => Err(ApiError::forbidden(format!(
            "requires the `deployer` role on environment type `{t}`{}",
            if env.is_some() {
                ""
            } else {
                ", because application values apply to its environments"
            }
        ))),
    }
}

async fn check_env(state: &AppState, app: &str, env: Option<&str>) -> ApiResult<()> {
    if let Some(env) = env {
        env_or_404(state, app, env).await?;
    }
    Ok(())
}

async fn value_or_404(
    state: &AppState,
    app: &str,
    env: Option<&str>,
    key: &str,
) -> ApiResult<ValueRecord> {
    state
        .store
        .value(state.org(), app, env, key)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("value `{key}` not found")))
}

/// Writes a value; a secret given as `{value}` is written to the primary store
/// first and discarded if the change is rejected.
async fn write(
    state: &AppState,
    ctx: &Ctx,
    app: &str,
    env: Option<&str>,
    key: &str,
    body: ValueBody,
    create: bool,
) -> ApiResult<()> {
    check_key(key)?;
    let content = match (body.value, &body.secret_ref) {
        (Some(v), None) => Ok(ValueContent::Value(v)),
        (None, Some(secret)) => Err(PlannedSecret::new("`secret_ref`", secret)?),
        _ => {
            return Err(ApiError::invalid(
                "a value needs exactly one of `value` and `secret_ref`",
            ));
        }
    };
    check_deployer(state, ctx, app, env).await?;
    let org = state.org();
    let target = match env {
        Some(env) => format!("{app}/{env}/{key}"),
        None => format!("{app}/{key}"),
    };
    let action = if create {
        "value.create"
    } else {
        "value.update"
    };
    let audit = ctx.audit(org, action, target);
    let put = |content| PutValue {
        key: key.to_owned(),
        description: body.description.clone(),
        content,
        create,
    };
    match content {
        Ok(plain) => {
            state
                .store
                .put_value(org, app, env, put(plain), audit)
                .await?
        }
        Err(secret) => {
            let primary = secret_inputs::primary(state)?;
            let mut written = Vec::new();
            let result: ApiResult<()> = async {
                let name = format!("{}{key}", value_secret_prefix(app, env));
                let r = secret.materialize(&primary, &name, &mut written).await?;
                state
                    .store
                    .put_value(org, app, env, put(ValueContent::SecretRef(r)), audit)
                    .await?;
                Ok(())
            }
            .await;
            if result.is_err() {
                secret_inputs::discard(&primary, written).await;
            }
            result?
        }
    }
    Ok(())
}

async fn list(state: &AppState, app: &str, env: Option<&str>) -> ApiResult<Json<Vec<ValueRecord>>> {
    check_env(state, app, env).await?;
    Ok(Json(state.store.values(state.org(), app, env).await?))
}

async fn create(
    state: &AppState,
    ctx: &Ctx,
    app: &str,
    env: Option<&str>,
    body: Value,
) -> ApiResult<(StatusCode, Json<ValueRecord>)> {
    let mut body: ValueBody = secret_inputs::parse_body(body)?;
    let key = body
        .key
        .take()
        .ok_or_else(|| ApiError::invalid("`key` is required"))?;
    write(state, ctx, app, env, &key, body, true).await?;
    Ok((
        StatusCode::CREATED,
        Json(value_or_404(state, app, env, &key).await?),
    ))
}

async fn replace(
    state: &AppState,
    ctx: &Ctx,
    app: &str,
    env: Option<&str>,
    key: &str,
    body: Value,
) -> ApiResult<Json<ValueRecord>> {
    let mut body: ValueBody = secret_inputs::parse_body(body)?;
    if body.key.take().is_some_and(|k| k != key) {
        return Err(ApiError::invalid("`key` cannot be changed"));
    }
    check_env(state, app, env).await?;
    value_or_404(state, app, env, key).await?;
    write(state, ctx, app, env, key, body, false).await?;
    Ok(Json(value_or_404(state, app, env, key).await?))
}

async fn delete(
    state: &AppState,
    ctx: &Ctx,
    app: &str,
    env: Option<&str>,
    key: &str,
) -> ApiResult<StatusCode> {
    check_env(state, app, env).await?;
    value_or_404(state, app, env, key).await?;
    check_deployer(state, ctx, app, env).await?;
    let org = state.org();
    let target = match env {
        Some(env) => format!("{app}/{env}/{key}"),
        None => format!("{app}/{key}"),
    };
    state
        .store
        .delete_value(org, app, env, key, ctx.audit(org, "value.delete", target))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn versions(
    state: &AppState,
    app: &str,
    env: Option<&str>,
) -> ApiResult<Json<Vec<ValueSetVersion>>> {
    check_env(state, app, env).await?;
    Ok(Json(
        state.store.value_versions(state.org(), app, env).await?,
    ))
}

// Application values.

pub async fn app_list(
    State(state): State<AppState>,
    Path((_, app)): Path<(String, String)>,
) -> ApiResult<Json<Vec<ValueRecord>>> {
    list(&state, &app, None).await
}

pub async fn app_get(
    State(state): State<AppState>,
    Path((_, app, key)): Path<(String, String, String)>,
) -> ApiResult<Json<ValueRecord>> {
    Ok(Json(value_or_404(&state, &app, None, &key).await?))
}

pub async fn app_create(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, app)): Path<(String, String)>,
    JsonBody(body): JsonBody<Value>,
) -> ApiResult<(StatusCode, Json<ValueRecord>)> {
    create(&state, &ctx, &app, None, body).await
}

pub async fn app_replace(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, app, key)): Path<(String, String, String)>,
    JsonBody(body): JsonBody<Value>,
) -> ApiResult<Json<ValueRecord>> {
    replace(&state, &ctx, &app, None, &key, body).await
}

pub async fn app_delete(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, app, key)): Path<(String, String, String)>,
) -> ApiResult<StatusCode> {
    delete(&state, &ctx, &app, None, &key).await
}

pub async fn app_versions(
    State(state): State<AppState>,
    Path((_, app)): Path<(String, String)>,
) -> ApiResult<Json<Vec<ValueSetVersion>>> {
    versions(&state, &app, None).await
}

// Environment values.

pub async fn env_list(
    State(state): State<AppState>,
    Path((_, app, env)): Path<(String, String, String)>,
) -> ApiResult<Json<Vec<ValueRecord>>> {
    list(&state, &app, Some(&env)).await
}

pub async fn env_get(
    State(state): State<AppState>,
    Path((_, app, env, key)): Path<(String, String, String, String)>,
) -> ApiResult<Json<ValueRecord>> {
    check_env(&state, &app, Some(&env)).await?;
    Ok(Json(value_or_404(&state, &app, Some(&env), &key).await?))
}

pub async fn env_create(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, app, env)): Path<(String, String, String)>,
    JsonBody(body): JsonBody<Value>,
) -> ApiResult<(StatusCode, Json<ValueRecord>)> {
    check_env(&state, &app, Some(&env)).await?;
    create(&state, &ctx, &app, Some(&env), body).await
}

pub async fn env_replace(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, app, env, key)): Path<(String, String, String, String)>,
    JsonBody(body): JsonBody<Value>,
) -> ApiResult<Json<ValueRecord>> {
    replace(&state, &ctx, &app, Some(&env), &key, body).await
}

pub async fn env_delete(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, app, env, key)): Path<(String, String, String, String)>,
) -> ApiResult<StatusCode> {
    delete(&state, &ctx, &app, Some(&env), &key).await
}

pub async fn env_versions(
    State(state): State<AppState>,
    Path((_, app, env)): Path<(String, String, String)>,
) -> ApiResult<Json<Vec<ValueSetVersion>>> {
    versions(&state, &app, Some(&env)).await
}
