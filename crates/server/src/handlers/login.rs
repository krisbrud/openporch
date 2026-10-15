//! GitHub login. With the device flow, the client starts a device login, shows the
//! person the code and URL, and polls until GitHub reports the person's decision.
//! With the browser flow, for a web portal, the browser goes to GitHub and back to
//! the server's callback, which returns it to the portal with a one-time login code
//! that the portal exchanges for a session. Either way the server talks to GitHub,
//! so the GitHub access token never reaches the client.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use reqwest::Url;
use serde::{Deserialize, Serialize};

use crate::config::GithubBrowserConfig;

use crate::api::{JsonBody, QueryParams};
use crate::error::{ApiError, ApiResult, code};
use crate::identity::{DeviceLogin, DevicePoll, IdentityProvider, ProviderError};
use crate::login::{self, Session};
use crate::rate_limit::ClientAddr;
use crate::state::AppState;

const TOKEN_PATH: &str = "/auth/github/token";

fn github(state: &AppState) -> ApiResult<Arc<dyn IdentityProvider>> {
    state
        .github
        .clone()
        .ok_or_else(|| ApiError::not_found("GitHub login is not configured on this server"))
}

impl From<ProviderError> for ApiError {
    fn from(e: ProviderError) -> Self {
        match e {
            ProviderError::Invalid(m) => ApiError::invalid(m),
            ProviderError::Unavailable(_) => {
                tracing::warn!(error = %e, "identity provider call failed");
                ApiError::new(
                    StatusCode::BAD_GATEWAY,
                    code::PROVIDER_UNAVAILABLE,
                    "the identity provider is unavailable; try again",
                )
            }
        }
    }
}

/// Starts a GitHub device login.
pub async fn start(
    State(state): State<AppState>,
    ClientAddr(addr): ClientAddr,
) -> ApiResult<Json<DeviceLogin>> {
    state.login_limit.check(&addr)?;
    Ok(Json(github(&state)?.start_device_login().await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Poll {
    device_code: String,
    /// The one-time invitation code, on a first login.
    invitation_code: Option<String>,
}

/// A device login that is still waiting for the person.
#[derive(Serialize)]
struct Pending {
    status: &'static str,
    interval: u64,
}

fn login_failed(message: &str) -> ApiError {
    ApiError::new(StatusCode::UNAUTHORIZED, code::UNAUTHENTICATED, message)
}

/// Polls a GitHub device login. Returns `202` while the person has not decided, the
/// session once they approve, and `401` when they deny, the codes expire or no member
/// or invitation matches the GitHub account.
pub async fn poll(
    State(state): State<AppState>,
    ClientAddr(addr): ClientAddr,
    JsonBody(body): JsonBody<Poll>,
) -> ApiResult<Response> {
    state.login_limit.check(&addr)?;
    let provider = github(&state)?;
    match provider.poll_device_login(&body.device_code).await? {
        DevicePoll::Pending { interval } => Ok((
            StatusCode::ACCEPTED,
            Json(Pending {
                status: "pending",
                interval,
            }),
        )
            .into_response()),
        DevicePoll::Denied => Err(login_failed("the login was denied on GitHub")),
        DevicePoll::Expired => Err(login_failed(
            "the device code expired or is unknown; start the login again",
        )),
        DevicePoll::Verified(identity) => {
            let session: Session = login::login_session(
                &state,
                &identity,
                body.invitation_code.as_deref(),
                TOKEN_PATH,
            )
            .await?;
            Ok(Json(session).into_response())
        }
    }
}

const SESSION_PATH: &str = "/auth/github/session";
/// The cookie that binds a browser login to the browser that started it.
const LOGIN_COOKIE: &str = "openporch_github_login";

fn browser_config(state: &AppState) -> ApiResult<&GithubBrowserConfig> {
    state
        .config
        .github
        .as_ref()
        .and_then(|g| g.browser.as_ref())
        .ok_or_else(|| ApiError::not_found("GitHub browser login is not configured on this server"))
}

/// The login cookie: sent back only to the callback, never readable by scripts, and
/// only over TLS when the callback uses it. `Lax` lets it ride GitHub's redirect.
fn login_cookie(config: &GithubBrowserConfig, value: &str, max_age: i64) -> HeaderValue {
    let https = Url::parse(&config.callback_url).is_ok_and(|u| u.scheme() == "https");
    let secure = if https { "; Secure" } else { "" };
    let cookie = format!(
        "{LOGIN_COOKIE}={value}; Max-Age={max_age}; Path={}; HttpOnly; SameSite=Lax{secure}",
        crate::config::GITHUB_CALLBACK_PATH
    );
    HeaderValue::from_str(&cookie).unwrap_or_else(|_| HeaderValue::from_static(""))
}

fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(n, _)| *n == name)
        .map(|(_, v)| v)
}

/// A `302` to `location` that is never cached, with `cookie` set.
fn redirect(location: &str, cookie: HeaderValue) -> ApiResult<Response> {
    let location = HeaderValue::from_str(location)
        .map_err(|_| ApiError::internal("a redirect address is not a valid header"))?;
    Ok((
        StatusCode::FOUND,
        [
            (header::LOCATION, location),
            (header::SET_COOKIE, cookie),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
    )
        .into_response())
}

#[derive(Deserialize)]
pub struct Authorize {
    /// The page to return to: one of the configured return URLs. It may be left out
    /// when only one is configured.
    return_to: Option<String>,
}

/// Starts a browser login: binds it to this browser with a cookie and sends the
/// browser to GitHub.
pub async fn authorize(
    State(state): State<AppState>,
    ClientAddr(addr): ClientAddr,
    QueryParams(query): QueryParams<Authorize>,
) -> ApiResult<Response> {
    state.login_limit.check(&addr)?;
    let config = browser_config(&state)?;
    let provider = github(&state)?;
    let return_to = match (&query.return_to, config.return_urls.as_slice()) {
        (None, [only]) => only.clone(),
        (None, _) => {
            return Err(ApiError::invalid(
                "`return_to` is required: name one of the server's return URLs",
            ));
        }
        (Some(url), urls) if urls.iter().any(|u| u == url) => url.clone(),
        (Some(_), _) => {
            return Err(ApiError::invalid(
                "`return_to` is not one of the server's return URLs",
            ));
        }
    };
    let started = state
        .browser_logins
        .start(&return_to, openporch_store::now_ms())
        .ok_or_else(|| ApiError::rate_limited(60))?;
    let url = provider
        .authorize_url(&started.state, &config.callback_url)
        .ok_or_else(|| {
            ApiError::not_found("GitHub browser login is not configured on this server")
        })?;
    let max_age = crate::browser_login::PENDING_TTL_MS / 1000;
    redirect(
        url.as_str(),
        login_cookie(config, &started.binding, max_age),
    )
}

#[derive(Deserialize)]
pub struct Callback {
    state: Option<String>,
    code: Option<String>,
    error: Option<String>,
}

/// Where a login returns to, with one query parameter added.
fn returning(return_to: &str, key: &str, value: &str) -> ApiResult<String> {
    let mut url = Url::parse(return_to).map_err(ApiError::internal)?;
    url.query_pairs_mut().append_pair(key, value);
    Ok(url.into())
}

/// GitHub's redirect back. It must carry the state of a login this browser started.
/// The browser then returns to the login's page with `login_code`, or with `error`:
/// `access_denied` when the person declined, `login_failed` when GitHub did not
/// confirm the sign-in, and `unavailable` when GitHub could not be reached.
pub async fn callback(
    State(state): State<AppState>,
    ClientAddr(addr): ClientAddr,
    headers: HeaderMap,
    QueryParams(query): QueryParams<Callback>,
) -> ApiResult<Response> {
    state.login_limit.check(&addr)?;
    let config = browser_config(&state)?;
    let provider = github(&state)?;
    let now = openporch_store::now_ms();
    let return_to = state
        .browser_logins
        .callback(
            query.state.as_deref().unwrap_or_default(),
            cookie_value(&headers, LOGIN_COOKIE),
            now,
        )
        .map_err(|e| {
            login_failed(match e {
                crate::browser_login::CallbackError::UnknownState => {
                    "this login is unknown, expired or already used; sign in again"
                }
                crate::browser_login::CallbackError::WrongBrowser => {
                    "this login was started in another browser; sign in again"
                }
            })
        })?;
    let clear = login_cookie(config, "", 0);
    let fail = |reason: &str| redirect(&returning(&return_to, "error", reason)?, clear.clone());
    if let Some(error) = query.error.as_deref() {
        return fail(if error == "access_denied" {
            "access_denied"
        } else {
            "login_failed"
        });
    }
    let Some(code) = query.code.as_deref() else {
        return fail("login_failed");
    };
    let identity = match provider.exchange_code(code, &config.callback_url).await {
        Ok(Some(identity)) => identity,
        Ok(None) | Err(ProviderError::Invalid(_)) => return fail("login_failed"),
        Err(e @ ProviderError::Unavailable(_)) => {
            tracing::warn!(error = %e, "identity provider call failed");
            return fail("unavailable");
        }
    };
    let Some(login_code) = state.browser_logins.complete(identity, now) else {
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

/// Exchanges a browser login's one-time code for a session.
pub async fn exchange(
    State(state): State<AppState>,
    ClientAddr(addr): ClientAddr,
    JsonBody(body): JsonBody<Exchange>,
) -> ApiResult<Json<Session>> {
    state.login_limit.check(&addr)?;
    browser_config(&state)?;
    let identity = state
        .browser_logins
        .exchange(&body.login_code, openporch_store::now_ms())
        .ok_or_else(|| {
            login_failed("the login code is unknown, expired or already used; sign in again")
        })?;
    let session = login::login_session(
        &state,
        &identity,
        body.invitation_code.as_deref(),
        SESSION_PATH,
    )
    .await?;
    Ok(Json(session))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_login_cookie_is_secure_whenever_the_callback_uses_tls() {
        let config = |callback_url: &str| GithubBrowserConfig {
            client_secret_file: "secret".into(),
            callback_url: callback_url.into(),
            return_urls: vec![],
        };
        for (url, secure) in [
            ("https://porch.example.com/auth/github/callback", true),
            ("HTTPS://porch.example.com/auth/github/callback", true),
            ("http://127.0.0.1:8080/auth/github/callback", false),
        ] {
            let cookie = login_cookie(&config(url), "v", 600);
            assert_eq!(
                cookie.to_str().unwrap().ends_with("; Secure"),
                secure,
                "{url}"
            );
        }
    }
}
