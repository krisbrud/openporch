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

/// Ready when the database answers, the required keys are loaded and the in-process
/// executor runs. Ownership is held for the life of the process. Reasons name key
/// IDs only, never key material.
pub async fn readyz(State(state): State<AppState>) -> Response {
    let mut reasons = state.not_ready.clone();
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

pub async fn version() -> Json<serde_json::Value> {
    Json(json!({
        "version": crate::VERSION,
        "contract": CONTRACT_VERSION,
        "capabilities": {
            "authentication": ["token", "session"],
            "identity_providers": [],
            "drivers": [],
        },
    }))
}
