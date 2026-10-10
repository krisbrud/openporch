//! Audit log queries. Entries cannot be changed or deleted through the API.

use axum::Json;
use axum::extract::State;
use openporch_store::{AuditEntry, AuditFilter, IdentityStore};
use serde::{Deserialize, Serialize};

use crate::api::QueryParams;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

const DEFAULT_LIMIT: u32 = 100;
const MAX_LIMIT: u32 = 1000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    from: Option<i64>,
    to: Option<i64>,
    user_id: Option<String>,
    path_prefix: Option<String>,
    method: Option<String>,
    before: Option<i64>,
    limit: Option<u32>,
}

/// A page of entries, newest first. Pass `next` as `before` for the next page.
#[derive(Serialize)]
pub struct Page {
    entries: Vec<AuditEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next: Option<i64>,
}

pub async fn query(
    State(state): State<AppState>,
    QueryParams(q): QueryParams<Query>,
) -> ApiResult<Json<Page>> {
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(ApiError::invalid(format!(
            "`limit` must be between 1 and {MAX_LIMIT}"
        )));
    }
    let filter = AuditFilter {
        from: q.from,
        to: q.to,
        user_id: q.user_id,
        path_prefix: q.path_prefix,
        method: q.method.map(|m| m.to_ascii_uppercase()),
        before: q.before,
        limit,
    };
    let entries = state.store.audit_query(state.org(), &filter).await?;
    let next = (entries.len() == limit as usize)
        .then(|| entries.last().map(|e| e.seq))
        .flatten();
    Ok(Json(Page { entries, next }))
}
