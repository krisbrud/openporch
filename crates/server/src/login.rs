//! Turning a verified external identity into a session. Identity providers (GitHub
//! in milestone 1) verify the identity; this module maps it to a member, accepting
//! an invitation on first login, and issues the signed session.

use openporch_store::{ExternalIdentity, IdentityStore, StoreError};

use crate::auth::{self, Claims, SESSION_TTL_MS};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// A user ID derived from the login name or email, falling back to a random one.
fn derive_user_id(identity: &ExternalIdentity) -> String {
    let candidate = identity
        .login
        .clone()
        .or_else(|| {
            identity
                .verified_email
                .as_ref()
                .and_then(|e| e.split('@').next().map(str::to_owned))
        })
        .unwrap_or_default()
        .to_ascii_lowercase();
    if openporch_core::ids::is_valid_object_id(&candidate) {
        candidate
    } else {
        format!("user-{}", auth::random_id(4))
    }
}

fn login_failed(message: impl Into<String>) -> ApiError {
    ApiError::new(
        axum::http::StatusCode::UNAUTHORIZED,
        crate::error::code::UNAUTHENTICATED,
        message,
    )
}

/// Logs in a verified identity and returns a session token. Unknown identities need
/// a valid invitation `code`.
pub async fn login(
    state: &AppState,
    identity: &ExternalIdentity,
    code: Option<&str>,
) -> ApiResult<String> {
    let org = state.org();
    let signer = state
        .signer
        .as_ref()
        .ok_or_else(|| ApiError::not_ready("sessions are unavailable until the keys load"))?;
    let now = openporch_store::now_ms();
    let principal = match state
        .store
        .principal_by_identity(org, &identity.provider, &identity.subject)
        .await?
    {
        Some(p) => p,
        None => {
            let Some(code) = code else {
                return Err(login_failed(
                    "no member is linked to this identity; an invitation is required",
                ));
            };
            let mut user_id = derive_user_id(identity);
            if state.store.principal(org, &user_id).await?.is_some() {
                user_id = format!("{user_id}-{}", auth::random_id(2));
            }
            match state
                .store
                .accept_invitation(org, &auth::hash_token(code), identity, &user_id, now)
                .await
            {
                Ok(p) => p,
                Err(StoreError::Conflict(reason)) => return Err(login_failed(reason)),
                Err(e) => return Err(e.into()),
            }
        }
    };
    let claims = Claims {
        sub: principal.id.clone(),
        org: org.to_owned(),
        jti: auth::random_id(16),
        iat: now,
        exp: now + SESSION_TTL_MS,
    };
    state
        .store
        .record_session(org, &claims.jti, &claims.sub, claims.exp)
        .await?;
    Ok(signer.sign(&claims))
}
