//! Turning a verified external identity into a session. Identity providers (GitHub
//! in milestone 1) verify the identity; this module maps it to a member, accepting
//! an invitation on first login, and issues the signed session.

use openporch_store::{
    AuditIntent, AuditOutcome, ExternalIdentity, IdentityStore, Store, StoreError,
};

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

/// An issued session.
#[derive(Debug, serde::Serialize)]
pub struct Session {
    /// The signed session token.
    pub token: String,
    /// The member the session belongs to.
    pub user: String,
    /// When the session expires, in milliseconds since the epoch.
    pub expires_at: i64,
}

/// Records a login that a verified identity could not complete. The entry names the
/// identity and the reason, never a code or token.
async fn record_failure(state: &AppState, identity: &ExternalIdentity, path: &str, reason: &str) {
    let mut audit = AuditIntent::accepted(state.org(), "", "POST", path, "login.failed");
    audit.actor = None;
    audit.target = Some(format!("{}:{}", identity.provider, identity.subject));
    audit.outcome = AuditOutcome::Denied;
    audit.status_code = Some(401);
    audit.reason = Some(reason.to_owned());
    if let Err(e) = state.store.record_audit(audit).await {
        tracing::error!(error = %e, "recording a failed login");
    }
}

/// Logs in a verified identity and returns a session. Unknown identities need a valid
/// invitation `code`. `path` is the login route, for the audit log.
pub async fn login_session(
    state: &AppState,
    identity: &ExternalIdentity,
    code: Option<&str>,
    path: &str,
) -> ApiResult<Session> {
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
                let reason = "no member is linked to this identity; an invitation is required";
                record_failure(state, identity, path, reason).await;
                return Err(login_failed(reason));
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
                Err(StoreError::Conflict(reason)) => {
                    record_failure(state, identity, path, &reason).await;
                    return Err(login_failed(reason));
                }
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
    let mut audit = AuditIntent::accepted(org, &principal.id, "POST", path, "session.create");
    audit.target = Some(principal.id.clone());
    state
        .store
        .record_session(org, &claims.jti, &claims.sub, claims.exp, audit)
        .await?;
    Ok(Session {
        token: signer.sign(&claims),
        user: principal.id,
        expires_at: claims.exp,
    })
}

/// Logs in a verified identity as the GitHub device login does and returns the token.
#[cfg(test)]
pub async fn login(
    state: &AppState,
    identity: &ExternalIdentity,
    code: Option<&str>,
) -> ApiResult<String> {
    Ok(login_session(state, identity, code, "/auth/github/token")
        .await?
        .token)
}
