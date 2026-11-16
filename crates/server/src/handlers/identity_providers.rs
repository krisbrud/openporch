//! `/orgs/{org}/identity-providers`: OpenID Connect providers people sign in
//! with, managed through the API. The configuration file's providers are listed
//! too, as `source: config`, and cannot be changed here.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use openporch_store::LoginProviderStore;
use serde_json::{Map, Value};

use crate::api::{Ctx, JsonBody};
use crate::error::{ApiError, ApiResult};
use crate::login_providers::{self, Settings, View};
use crate::secret_inputs::{self, PlannedSecret};
use crate::state::AppState;

pub async fn list(State(state): State<AppState>) -> ApiResult<Json<Vec<View>>> {
    Ok(Json(login_providers::all(&state).await?))
}

pub async fn get(
    State(state): State<AppState>,
    Path((_, id)): Path<(String, String)>,
) -> ApiResult<Json<View>> {
    if let Some(c) = state.config.oidc_login.iter().find(|c| c.id == id) {
        return Ok(Json(View::config(c)));
    }
    let record = state
        .store
        .login_provider(state.org(), &id)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("identity provider `{id}`")))?;
    Ok(Json(View::api(&record)?))
}

/// A request body split into its ID, settings and client secret input. Errors
/// never quote the secret.
fn parse(mut body: Map<String, Value>) -> ApiResult<(Option<String>, Settings, Option<Value>)> {
    let id = match body.remove("id") {
        None => None,
        Some(Value::String(s)) => Some(s),
        Some(_) => return Err(ApiError::invalid("`id` must be a string")),
    };
    let secret = body.remove("client_secret");
    let settings: Settings = secret_inputs::parse_body(Value::Object(body))?;
    Ok((id, settings, secret))
}

/// Writes the client secret if a new one is given, then stores the provider;
/// a version written for a rejected change is discarded.
async fn write(
    state: &AppState,
    ctx: &Ctx,
    id: &str,
    settings: Settings,
    secret: Option<Value>,
    current: Option<&openporch_core::model::SecretRef>,
    create: bool,
) -> ApiResult<View> {
    login_providers::check(state, id, &settings)?;
    let planned = match secret {
        None | Some(Value::Null) => None,
        Some(v) => Some(PlannedSecret::new("`client_secret`", &v)?),
    };
    if let Some(r) = planned.as_ref().and_then(PlannedSecret::reference)
        && !login_providers::is_current_secret(current, id, r)
    {
        return Err(ApiError::invalid(
            "`client_secret` may name only the secret the provider holds now, to keep it; give \
             `{value}` to set a new one",
        ));
    }
    let org = state.org();
    let primary = secret_inputs::Primary::Builtin(secret_inputs::builtin(state)?);
    let mut written = Vec::new();
    let result: ApiResult<View> = async {
        let client_secret = match planned {
            Some(p) => Some(
                p.materialize(
                    primary.store(),
                    &login_providers::secret_name(id),
                    &mut written,
                )
                .await?,
            ),
            None => None,
        };
        let record = login_providers::record(id, &settings, client_secret);
        if create {
            let audit = ctx.audit(org, "identity_provider.create", id);
            state
                .store
                .create_login_provider(org, &record, audit)
                .await?;
        } else {
            let audit = ctx.audit(org, "identity_provider.update", id);
            state
                .store
                .update_login_provider(org, &record, audit)
                .await?;
        }
        state.api_logins.forget(id);
        let stored = state
            .store
            .login_provider(org, id)
            .await?
            .ok_or_else(|| ApiError::not_found(format!("identity provider `{id}`")))?;
        View::api(&stored)
    }
    .await;
    if result.is_err() {
        secret_inputs::discard(&primary, written).await;
    }
    result
}

pub async fn create(
    State(state): State<AppState>,
    ctx: Ctx,
    JsonBody(body): JsonBody<Map<String, Value>>,
) -> ApiResult<(StatusCode, Json<View>)> {
    let (id, settings, secret) = parse(body)?;
    let id = id.ok_or_else(|| ApiError::invalid("`id` is required"))?;
    let view = write(&state, &ctx, &id, settings, secret, None, true).await?;
    Ok((StatusCode::CREATED, Json(view)))
}

/// Replaces a provider's settings. A client secret left out makes it a public
/// client; naming the reference it holds keeps its secret.
pub async fn replace(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, id)): Path<(String, String)>,
    JsonBody(body): JsonBody<Map<String, Value>>,
) -> ApiResult<Json<View>> {
    let (body_id, settings, secret) = parse(body)?;
    if body_id.is_some_and(|b| b != id) {
        return Err(ApiError::invalid("`id` cannot be changed"));
    }
    let current = state
        .store
        .login_provider(state.org(), &id)
        .await?
        .ok_or_else(|| {
            if state.oidc_logins.contains_key(&id) {
                ApiError::conflict(format!(
                    "identity provider `{id}` is configured in the server's configuration file"
                ))
            } else {
                ApiError::not_found(format!("identity provider `{id}`"))
            }
        })?;
    let view = write(
        &state,
        &ctx,
        &id,
        settings,
        secret,
        current.client_secret.as_ref(),
        false,
    )
    .await?;
    Ok(Json(view))
}

pub async fn delete(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, id)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let org = state.org();
    if state.oidc_logins.contains_key(&id) {
        return Err(ApiError::conflict(format!(
            "identity provider `{id}` is configured in the server's configuration file"
        )));
    }
    state
        .store
        .delete_login_provider(org, &id, ctx.audit(org, "identity_provider.delete", &id))
        .await?;
    state.api_logins.forget(&id);
    Ok(StatusCode::NO_CONTENT)
}
