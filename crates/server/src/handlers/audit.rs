//! Audit log queries and expiry. Entries cannot be changed or deleted
//! through the API, except that an administrator can expire those older than
//! the retention period.

use std::sync::atomic::{AtomicU64, Ordering};

use axum::Json;
use axum::extract::State;
use openporch_store::{
    AuditEntry, AuditFilter, AuditOutcome, EXPIRY_ACTION, IdentityStore, Retained, Store,
};
use serde::{Deserialize, Serialize};

use crate::api::{Ctx, JsonBody, QueryParams};
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

/// Expiries that failed since the server started.
static EXPIRY_FAILURES: AtomicU64 = AtomicU64::new(0);

/// Expiries that failed since the server started.
pub fn expiry_failures() -> u64 {
    EXPIRY_FAILURES.load(Ordering::Relaxed)
}

/// The start of the retention period: entries written before it may expire.
pub fn retention_start(state: &AppState) -> i64 {
    const DAY_MS: i64 = 24 * 60 * 60 * 1000;
    openporch_store::now_ms() - i64::from(state.config.audit.retention_days) * DAY_MS
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpireRequest {
    /// Report what would expire without removing anything.
    #[serde(default)]
    dry_run: bool,
    /// Expire only entries written before this, in milliseconds since the
    /// epoch, such as the `before` of an earlier dry run whose entries were
    /// exported. It cannot reach into the retention period.
    before: Option<i64>,
}

#[derive(Serialize)]
pub struct Expired {
    dry_run: bool,
    retention_days: u32,
    /// Entries written before this, in milliseconds since the epoch, expire.
    before: i64,
    /// Entries removed, or that would be.
    expired: i64,
    /// Entries kept.
    remaining: i64,
    /// Everything else, which expiry never removes.
    retained: Retained,
    /// What keeping it means.
    note: &'static str,
}

const RETAINED_NOTE: &str = "Expiry removes only audit entries. Resources, their state and \
     recovery records, and the artifacts and secrets they reference are kept, and the cloud \
     resources behind them keep running and may keep costing money until they are deleted.";

/// Removes the entries older than the retention period, recording the
/// expiry in the audit log in the same transaction.
pub async fn expire(
    State(state): State<AppState>,
    ctx: Ctx,
    JsonBody(req): JsonBody<ExpireRequest>,
) -> ApiResult<Json<Expired>> {
    let org = state.org();
    let start = retention_start(&state);
    let before = match req.before {
        Some(before) if before > start => {
            return Err(ApiError::invalid(format!(
                "`before` must be at most {start}: entries newer than that are inside the \
                 retention period"
            )));
        }
        Some(before) => before,
        None => start,
    };
    let retention_days = state.config.audit.retention_days;
    // Reported before anything is removed.
    let retained = state.store.retained().await?;
    if req.dry_run {
        let status = state.store.audit_retention(before).await?;
        return Ok(Json(Expired {
            dry_run: true,
            retention_days,
            before,
            expired: status.expirable,
            remaining: status.entries - status.expirable,
            retained,
            note: RETAINED_NOTE,
        }));
    }
    let mut audit = ctx.audit(org, EXPIRY_ACTION, String::new());
    audit.outcome = AuditOutcome::Succeeded;
    match state.store.expire_audit(before, audit).await {
        // Once the expiry has committed, nothing after it may turn it into
        // an error.
        Ok((removed, kept)) => {
            tracing::info!(removed, before, "audit entries expired");
            Ok(Json(Expired {
                dry_run: false,
                retention_days,
                before,
                expired: i64::try_from(removed).unwrap_or(i64::MAX),
                remaining: kept,
                retained,
                note: RETAINED_NOTE,
            }))
        }
        Err(e) => {
            let error = ApiError::from(e);
            // A refusal, such as a principal without administrator
            // authority, is not a failed expiry.
            if error.status.is_server_error() {
                EXPIRY_FAILURES.fetch_add(1, Ordering::Relaxed);
                tracing::error!(before, "audit expiry failed");
                let mut failed = ctx.audit(org, EXPIRY_ACTION, format!("entries before {before}"));
                failed.outcome = AuditOutcome::Failed;
                failed.reason = Some("storage_error".into());
                if let Err(e) = state.store.record_audit(failed).await {
                    tracing::error!(error = %e, "could not record the failed audit expiry");
                }
            }
            Err(error)
        }
    }
}
