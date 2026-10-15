//! Liveness, readiness and version.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::error::code;
use crate::state::AppState;

/// The public contract version of the API.
pub const CONTRACT_VERSION: &str = "1";

pub async fn livez() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok" }))
}

/// Ready when the database answers, the required keys are loaded, the in-process
/// executor runs and shutdown has not begun. Ownership is held for the life of the
/// process. Reasons name key IDs only, never key material.
pub async fn readyz(State(state): State<AppState>) -> Response {
    let mut reasons = state.not_ready_reasons();
    if sqlx_ping(&state).await.is_err() {
        reasons.push("database_unavailable".to_owned());
    }
    if reasons.is_empty() {
        return Json(json!({ "status": "ready" })).into_response();
    }
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({
            "error": code::NOT_READY,
            "message": format!("not ready: {}", reasons.join(", ")),
            "reasons": reasons,
        })),
    )
        .into_response()
}

async fn sqlx_ping(state: &AppState) -> Result<(), openporch_store::StoreError> {
    state.store.ping().await
}

pub async fn version(State(state): State<AppState>) -> Json<serde_json::Value> {
    // Each configured provider with the login flows it offers.
    let providers: Vec<serde_json::Value> = state
        .config
        .github
        .iter()
        .map(|g| {
            let mut flows = vec!["device"];
            if g.browser.is_some() {
                flows.push("browser");
            }
            json!({"id": "github", "flows": flows})
        })
        .collect();
    Json(json!({
        "version": crate::VERSION,
        "contract": CONTRACT_VERSION,
        "capabilities": {
            "authentication": ["token", "session"],
            "identity_providers": providers,
            "drivers": [],
        },
    }))
}
