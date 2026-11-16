//! Browser login through a configured OpenID Connect provider. Like GitHub's
//! browser login, the browser goes to the provider and back to the server's
//! callback, which returns it to the portal with a one-time login code that the
//! portal exchanges for a session. The authorization code, the PKCE verifier,
//! the nonce and the provider's tokens never reach the browser or the portal.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue};
use axum::response::Response;
use reqwest::Url;
use serde::Deserialize;

use crate::api::{JsonBody, QueryParams};
use crate::browser_login::CallbackError;
use crate::error::{ApiError, ApiResult};
use crate::handlers::login::{cookie_value, login_failed, pick_return_to, redirect, returning};
use crate::identity::ProviderError;
use crate::login::{self, Session};
use crate::oidc_login::OidcLogin;
use crate::rate_limit::ClientAddr;
use crate::state::AppState;

async fn provider(state: &AppState, id: &str) -> ApiResult<Arc<OidcLogin>> {
    crate::login_providers::provider(state, id).await
}

/// The cookie that binds a login to the browser that started it. Its name
/// comes from the login's `state`, so logins a browser runs at the same time
/// each keep their own cookie.
fn cookie_name(id: &str, state: &str) -> String {
    format!(
        "openporch_oidc_{id}_{}",
        &crate::auth::hash_token(state)[..16]
    )
}

/// The login cookie: sent back only to this provider's callback, never readable
/// by scripts, and only over TLS when the callback uses it. `Lax` lets it ride
/// the provider's redirect.
fn login_cookie(provider: &OidcLogin, state: &str, value: &str, max_age: i64) -> HeaderValue {
    let config = provider.config();
    let https = Url::parse(&config.callback_url).is_ok_and(|u| u.scheme() == "https");
    let secure = if https { "; Secure" } else { "" };
    let cookie = format!(
        "{}={value}; Max-Age={max_age}; Path={}; HttpOnly; SameSite=Lax{secure}",
        cookie_name(&config.id, state),
        crate::config::oidc_callback_path(&config.id),
    );
    HeaderValue::from_str(&cookie).unwrap_or_else(|_| HeaderValue::from_static(""))
}

/// Whether `url` is a page on a loopback listener, as a command-line client
/// opens one: `http://127.0.0.1:<port>/<path>` or `http://[::1]:<port>/<path>`,
/// with an explicit port and no credentials, query or fragment. `localhost` is
/// left out, since a name can resolve elsewhere (RFC 8252, section 8.3).
pub(crate) fn is_loopback_return(url: &str) -> bool {
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    parsed.scheme() == "http"
        && matches!(parsed.host_str(), Some("127.0.0.1" | "[::1]"))
        && parsed.port().is_some()
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.query().is_none()
        && parsed.fragment().is_none()
}

#[derive(Deserialize)]
pub struct Authorize {
    /// The page to return to: one of the provider's return URLs. It may be left
    /// out when only one is configured.
    return_to: Option<String>,
}

/// Starts a login: binds it to this browser with a cookie and sends the browser
/// to the provider with the login's state, nonce and PKCE challenge.
pub async fn authorize(
    State(state): State<AppState>,
    ClientAddr(addr): ClientAddr,
    Path(id): Path<String>,
    QueryParams(query): QueryParams<Authorize>,
) -> ApiResult<Response> {
    state.login_limit.check(&addr)?;
    let provider = provider(&state, &id).await?;
    let config = provider.config();
    let return_to = match query.return_to.as_deref() {
        Some(url) if config.loopback_return && is_loopback_return(url) => url.to_owned(),
        requested => pick_return_to(requested, &config.return_urls)?,
    };
    let secrets = OidcLogin::new_secrets();
    let now = openporch_store::now_ms();
    let started = state
        .browser_logins
        .start(&id, &return_to, Some(secrets.clone()), now)
        .ok_or_else(|| ApiError::rate_limited(60))?;
    let url = match provider.authorize_url(&started.state, &secrets, now).await {
        Ok(url) => url,
        Err(e) => {
            // The browser never gets this login, so it must not hold a slot.
            state.browser_logins.abandon(&started.state);
            return Err(e.into());
        }
    };
    let max_age = crate::browser_login::PENDING_TTL_MS / 1000;
    redirect(
        url.as_str(),
        login_cookie(&provider, &started.state, &started.binding, max_age),
    )
}

#[derive(Deserialize)]
pub struct Callback {
    state: Option<String>,
    code: Option<String>,
    error: Option<String>,
    /// The issuer, from providers that name it in the response (RFC 9207).
    iss: Option<String>,
}

/// The provider's redirect back. It must carry the state of a login this
/// browser started with this provider. The browser then returns to the login's
/// page with `login_code`, or with `error`: `access_denied` when the person
/// declined, `login_failed` when the provider did not confirm the sign-in or its
/// ID token failed a check, and `unavailable` when the provider could not be
/// reached.
pub async fn callback(
    State(state): State<AppState>,
    ClientAddr(addr): ClientAddr,
    Path(id): Path<String>,
    headers: HeaderMap,
    QueryParams(query): QueryParams<Callback>,
) -> ApiResult<Response> {
    state.login_limit.check(&addr)?;
    let provider = provider(&state, &id).await?;
    let now = openporch_store::now_ms();
    let login_state = query.state.as_deref().unwrap_or_default();
    let resumed = state
        .browser_logins
        .callback(
            &id,
            login_state,
            cookie_value(&headers, &cookie_name(&id, login_state)),
            now,
        )
        .map_err(|e| {
            login_failed(match e {
                CallbackError::UnknownState => {
                    "this login is unknown, expired or already used; sign in again"
                }
                CallbackError::WrongBrowser => {
                    "this login was started in another browser; sign in again"
                }
            })
        })?;
    let return_to = resumed.return_to;
    let clear = login_cookie(&provider, login_state, "", 0);
    let fail = |reason: &str| redirect(&returning(&return_to, "error", reason)?, clear.clone());
    let Some(secrets) = resumed.secrets else {
        return fail("login_failed");
    };
    if let Some(error) = query.error.as_deref() {
        return fail(if error == "access_denied" {
            "access_denied"
        } else {
            "login_failed"
        });
    }
    // A response naming another issuer was meant for another provider.
    if query
        .iss
        .as_deref()
        .is_some_and(|iss| iss != provider.config().issuer)
    {
        tracing::warn!(provider = %id, "an OIDC callback named another issuer");
        return fail("login_failed");
    }
    let Some(code) = query.code.as_deref() else {
        return fail("login_failed");
    };
    let identity = match provider.exchange_code(code, &secrets, now).await {
        Ok(Some(identity)) => identity,
        Ok(None) => return fail("login_failed"),
        Err(ProviderError::Invalid(reason)) => {
            tracing::warn!(provider = %id, %reason, "an OIDC login was refused");
            return fail("login_failed");
        }
        Err(e @ ProviderError::Unavailable(_)) => {
            tracing::warn!(provider = %id, error = %e, "identity provider call failed");
            return fail("unavailable");
        }
    };
    // The exchange window starts once the provider has answered.
    let now = openporch_store::now_ms();
    let Some(login_code) = state.browser_logins.complete(&id, identity, now) else {
        return fail("unavailable");
    };
    redirect(&returning(&return_to, "login_code", &login_code)?, clear)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Exchange {
    /// The one-time code the login returned with.
    login_code: String,
    /// The one-time invitation code, on a first login.
    invitation_code: Option<String>,
}

/// Exchanges a login's one-time code for a session.
pub async fn exchange(
    State(state): State<AppState>,
    ClientAddr(addr): ClientAddr,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<Exchange>,
) -> ApiResult<Json<Session>> {
    state.login_limit.check(&addr)?;
    provider(&state, &id).await?;
    let identity = state
        .browser_logins
        .exchange(&id, &body.login_code, openporch_store::now_ms())
        .ok_or_else(|| {
            login_failed("the login code is unknown, expired or already used; sign in again")
        })?;
    let session = login::login_session(
        &state,
        &identity,
        body.invitation_code.as_deref(),
        &format!("/auth/oidc/{id}/session"),
    )
    .await?;
    Ok(Json(session))
}

#[cfg(test)]
mod tests {
    use super::is_loopback_return;

    #[test]
    fn loopback_returns_name_a_loopback_address_and_port() {
        for url in [
            "http://127.0.0.1:53682/callback/abc",
            "http://[::1]:53682/callback",
        ] {
            assert!(is_loopback_return(url), "{url}");
        }
        for url in [
            "http://localhost:53682/callback",
            "http://127.0.0.1/callback",
            "https://127.0.0.1:53682/callback",
            "http://127.0.0.2:53682/callback",
            "http://127.0.0.1:53682/callback?x=1",
            "http://127.0.0.1:53682/callback#x",
            "http://user@127.0.0.1:53682/callback",
            "http://127.0.0.1.evil.example:53682/",
            "not a url",
        ] {
            assert!(!is_loopback_return(url), "{url}");
        }
    }
}
