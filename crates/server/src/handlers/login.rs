//! GitHub device login. The client starts a device login, shows the person the code
//! and URL, and polls until GitHub reports the person's decision. The server talks to
//! GitHub, so the GitHub access token never reaches the client.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::api::JsonBody;
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
