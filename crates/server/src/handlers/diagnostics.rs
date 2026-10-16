//! A diagnostics report an operator can save and share: versions, sanitized
//! configuration, readiness, work in progress, storage, resources that need
//! recovery and, on request, one deployment's operations, audit entries and
//! log. It never contains tokens, key material, secret values, resource
//! inputs or outputs, or driver state.

use axum::Json;
use axum::extract::State;
use openporch_engine::metrics::METRICS;
use openporch_store::Store;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::api::QueryParams;
use crate::config::{Config, StateSettings};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// The most log lines a report includes, from the end of the log.
const LOG_LINES: i64 = 200;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    app: Option<String>,
    env: Option<String>,
    deployment: Option<String>,
}

/// The configuration, as an allow-list: anything added to the configuration
/// later stays out until it is listed here. Paths to files that hold secrets
/// are reported only as configured or not.
fn sanitized(config: &Config) -> Value {
    let state = match &config.opentofu.state {
        StateSettings::Local {} => json!({"backend": "local"}),
        StateSettings::S3 {
            bucket,
            region,
            key_prefix,
            lock_table,
            endpoint,
            role_arn,
        } => json!({
            "backend": "s3",
            "bucket": bucket,
            "region": region,
            "key_prefix": key_prefix,
            "lock_table": lock_table,
            "endpoint": endpoint,
            "role_arn": role_arn,
        }),
    };
    let limits = &config.limits;
    json!({
        "org": config.org,
        "listen": config.listen.to_string(),
        "tls_terminated_by_proxy": config.tls_terminated_by_proxy,
        "max_body_bytes": config.max_body_bytes,
        "shutdown_grace_seconds": config.shutdown_grace_seconds,
        "master_key_file_configured": config.master_key_file.is_some(),
        "github": config.github.as_ref().map(|g| json!({
            "api_url": g.api_url,
            "web_url": g.web_url,
            "browser_login": g.browser.is_some(),
        })),
        "rate_limits": {
            "login_per_minute": config.rate_limits.login_per_minute,
            "tokens_per_minute": config.rate_limits.tokens_per_minute,
        },
        "limits": {
            "driver_concurrency": limits.driver_concurrency,
            "running_deployments": limits.running_deployments,
            "deployment_minutes": limits.deployment_minutes,
            "deployment_log_bytes": limits.deployment_log_bytes,
            "graph_nodes": limits.graph_nodes,
            "graph_edges": limits.graph_edges,
        },
        "audit": {
            "retention_days": config.audit.retention_days,
        },
        "opentofu": {
            "root_configured": config.opentofu.root.is_some(),
            "state": state,
        },
    })
}

pub async fn get(
    State(state): State<AppState>,
    QueryParams(q): QueryParams<Query>,
) -> ApiResult<Json<Value>> {
    let deployment = match (q.app, q.env, q.deployment) {
        (None, None, None) => Value::Null,
        (Some(app), Some(env), Some(id)) => deployment(&state, &app, &env, &id).await?,
        _ => {
            return Err(ApiError::invalid(
                "name a deployment with all of `app`, `env` and `deployment`, or none of them",
            ));
        }
    };
    let stats = state.store.stats().await?;
    let recovery = state.store.operations_needing_recovery().await?;
    let audit_retention = state
        .store
        .audit_retention(super::audit::retention_start(&state))
        .await?;
    let not_ready = state.not_ready_reasons();
    let queued = state.runners.queued();
    Ok(Json(json!({
        "generated_at": now_ms(),
        "version": crate::VERSION,
        "contract": super::health::CONTRACT_VERSION,
        "ready": not_ready.is_empty(),
        "not_ready": not_ready,
        "config": sanitized(&state.config),
        "work": {
            "deployments_queued": queued,
            "deployments_running": state.runners.running(),
            "driver_calls_running": METRICS.driver_active(),
        },
        "storage": {
            "database_bytes": stats.database_bytes,
            "wal_bytes": stats.wal_bytes,
            "free_bytes": stats.free_bytes,
            "audit_entries": stats.audit_entries,
            "log_lines": stats.log_lines,
        },
        "audit": {
            "retention_days": state.config.audit.retention_days,
            "entries": audit_retention.entries,
            "oldest_at": audit_retention.oldest_at,
            "expirable": audit_retention.expirable,
            "last_expiry": audit_retention.last_expiry,
            "expiry_failures": super::audit::expiry_failures(),
        },
        "resources": stats.resources,
        "needing_recovery": recovery,
        "deployment": deployment,
    })))
}

/// What the report says about one deployment: its record, operations,
/// audit entries and the end of its log.
async fn deployment(state: &AppState, app: &str, env: &str, id: &str) -> ApiResult<Value> {
    let org = state.org();
    let Some(d) = state.store.deployment(org, app, env, id).await? else {
        return Err(ApiError::not_found(format!(
            "deployment `{id}` of `{app}/{env}` does not exist"
        )));
    };
    let operations = state.store.deployment_operations(app, env, id).await?;
    let audit = state.store.deployment_audit(id).await?;
    let (log, total) = state
        .store
        .deployment_log_tail(app, env, id, LOG_LINES)
        .await?;
    Ok(json!({
        "id": d.id,
        "app": d.app,
        "env": d.env,
        "kind": d.kind,
        "status": d.status,
        "error": d.error,
        "created_at": d.created_at,
        "finished_at": d.finished_at,
        "request_id": audit.iter().find_map(|a| a.request_id.clone()),
        "operations": operations,
        "audit": audit,
        "log": log,
        "log_truncated": total > LOG_LINES,
    }))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}
