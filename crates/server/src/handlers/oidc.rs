//! The OIDC issuer's discovery document and JWKS, served without
//! credentials, and signing key rotation for administrators.

use axum::Json;
use axum::extract::State;
use openporch_engine::oidc::KeyInfo;
use openporch_store::{AuditOutcome, IdentityStore, now_ms};
use serde::Serialize;
use serde_json::Value;

use crate::api::Ctx;
use crate::error::{ApiError, ApiResult};
use crate::state::{AppState, OIDC_KEY_PURPOSE, new_oidc_key, open_oidc_keys};

/// The audit action of a key rotation.
pub const ROTATE_ACTION: &str = "oidc.key.rotate";

fn issuer(state: &AppState) -> ApiResult<&openporch_engine::oidc::Issuer> {
    match (&state.issuer, &state.config.public_url) {
        (Some(issuer), _) => Ok(issuer),
        (None, None) => Err(ApiError::not_found(
            "the OIDC issuer is off: set `public_url` to the HTTPS URL cloud providers reach \
             this server at",
        )),
        (None, Some(_)) => Err(ApiError::not_ready(
            "the OIDC issuer is unavailable until the master key is loaded",
        )),
    }
}

/// `GET /oidc/.well-known/openid-configuration`.
pub async fn discovery(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(issuer(&state)?.discovery()))
}

/// `GET /oidc/jwks`: the public signing keys, never a private one.
pub async fn jwks(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(issuer(&state)?.jwks(now_ms())))
}

/// The result of a rotation.
#[derive(Serialize)]
pub struct Rotated {
    /// The key new tokens are signed with.
    signing_kid: String,
    /// Every published key, newest first, with when each rotated-out one is
    /// unpublished.
    keys: Vec<KeyInfo>,
}

/// `POST /orgs/{org}/oidc/keys/rotate`: a new key signs from now on, and the
/// old one stays in the JWKS for `oidc.key_grace_minutes`.
pub async fn rotate(State(state): State<AppState>, ctx: Ctx) -> ApiResult<Json<Rotated>> {
    let issuer = issuer(&state)?;
    let keyring = state
        .keyring
        .as_ref()
        .ok_or_else(|| ApiError::not_ready("the master key is not loaded"))?;
    let generated = new_oidc_key(keyring).await.map_err(ApiError::internal)?;
    let grace_ms = i64::try_from(state.config.oidc.key_grace_minutes)
        .unwrap_or(i64::MAX)
        .saturating_mul(60_000);
    let mut audit = ctx.audit(state.org(), ROTATE_ACTION, "oidc");
    audit.outcome = AuditOutcome::Succeeded;
    let stored = state
        .store
        .rotate_signing_key(OIDC_KEY_PURPOSE, "RS256", generated, grace_ms, audit)
        .await?;
    let opened = open_oidc_keys(keyring, &stored).map_err(ApiError::internal)?;
    issuer.replace(&opened).map_err(ApiError::internal)?;
    let signing_kid = issuer
        .signing_kid()
        .ok_or_else(|| ApiError::internal("no active key after rotation"))?;
    Ok(Json(Rotated {
        signing_kid,
        keys: issuer.published(now_ms()),
    }))
}
