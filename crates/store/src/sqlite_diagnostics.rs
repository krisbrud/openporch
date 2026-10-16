//! What diagnostics report about operations and deployments. Only states,
//! identities, times and the redacted errors already recorded are read:
//! never inputs, outputs, cookies, recovery data or driver state.

use serde::Serialize;

use crate::sqlite::SqliteStore;
use crate::{LogLine, Result};

/// A driver operation, as diagnostics show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct OperationSummary {
    /// Operation ID.
    pub id: String,
    /// Application.
    pub app: String,
    /// Environment.
    pub env: String,
    /// The deployment that ran it.
    pub deployment_id: String,
    /// The resource's descriptor, while it is still managed.
    pub resource: Option<String>,
    /// The resource's GUResID.
    pub guresid: String,
    /// `put` or `delete`.
    pub kind: String,
    /// `started`, `succeeded`, `failed` or `unknown`.
    pub state: String,
    /// Which attempt it was.
    pub attempt: i64,
    /// Its recorded error; driver errors are redacted before they are stored.
    pub error: Option<String>,
    /// When it started, in milliseconds since the epoch.
    pub started_at: i64,
    /// When it finished.
    pub finished_at: Option<i64>,
    /// The resource's status now, while it is still managed.
    pub resource_status: Option<String>,
    /// When the resource's status was last observed.
    pub observed_at: Option<i64>,
}

/// An audit entry about a deployment, as diagnostics show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct AuditSummary {
    /// Entry ID.
    pub id: String,
    /// When, in milliseconds since the epoch.
    pub at: i64,
    /// The principal, if any.
    pub user_id: Option<String>,
    /// The request.
    pub request_id: Option<String>,
    /// What was done.
    pub action: String,
    /// `accepted`, `succeeded`, `failed` or `denied`.
    pub outcome: String,
    /// Why it failed or was denied.
    pub reason: Option<String>,
}

const OPERATION_COLUMNS: &str = "o.id, o.app_id AS app, o.env_id AS env, o.deployment_id, \
     r.descriptor AS resource, o.guresid, o.kind, o.state, o.attempt, o.error, o.started_at, \
     o.finished_at, r.status AS resource_status, r.observed_at";

impl SqliteStore {
    /// The latest operation of every resource whose outcome is unknown and
    /// needs recovery, oldest first.
    pub async fn operations_needing_recovery(&self) -> Result<Vec<OperationSummary>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {OPERATION_COLUMNS} FROM active_resources r \
             JOIN operations o ON o.guresid = r.guresid AND o.org_id = r.org_id \
             WHERE r.org_id = ? AND r.status IN ('unknown', 'recovery_required') \
             AND o.started_at = (SELECT MAX(started_at) FROM operations \
                                 WHERE guresid = r.guresid AND org_id = r.org_id) \
             ORDER BY o.started_at"
        ))
        .bind(self.org())
        .fetch_all(self.pool())
        .await?)
    }

    /// The operations deployment `id` ran, in the order they started.
    pub async fn deployment_operations(
        &self,
        app: &str,
        env: &str,
        id: &str,
    ) -> Result<Vec<OperationSummary>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {OPERATION_COLUMNS} FROM operations o \
             LEFT JOIN active_resources r ON r.guresid = o.guresid AND r.org_id = o.org_id \
             WHERE o.org_id = ? AND o.app_id = ? AND o.env_id = ? AND o.deployment_id = ? \
             ORDER BY o.started_at, o.id"
        ))
        .bind(self.org())
        .bind(app)
        .bind(env)
        .bind(id)
        .fetch_all(self.pool())
        .await?)
    }

    /// The audit entries correlated with deployment `id`: its acceptance and
    /// its outcome.
    pub async fn deployment_audit(&self, id: &str) -> Result<Vec<AuditSummary>> {
        Ok(sqlx::query_as(
            "SELECT id, at, actor_id AS user_id, request_id, action, outcome, reason \
             FROM audit_log WHERE org_id = ? AND operation_id = ? ORDER BY seq",
        )
        .bind(self.org())
        .bind(id)
        .fetch_all(self.pool())
        .await?)
    }

    /// The last `limit` lines of deployment `id`'s log, oldest first, and how
    /// many lines it has in all.
    pub async fn deployment_log_tail(
        &self,
        app: &str,
        env: &str,
        id: &str,
        limit: i64,
    ) -> Result<(Vec<LogLine>, i64)> {
        let mut lines: Vec<LogLine> = sqlx::query_as(
            "SELECT seq, operation_id, descriptor, guresid, at, level, message FROM logs \
             WHERE org_id = ? AND app_id = ? AND env_id = ? AND deployment_id = ? \
             ORDER BY seq DESC LIMIT ?",
        )
        .bind(self.org())
        .bind(app)
        .bind(env)
        .bind(id)
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        lines.reverse();
        let total = sqlx::query_scalar(
            "SELECT COUNT(*) FROM logs \
             WHERE org_id = ? AND app_id = ? AND env_id = ? AND deployment_id = ?",
        )
        .bind(self.org())
        .bind(app)
        .bind(env)
        .bind(id)
        .fetch_one(self.pool())
        .await?;
        Ok((lines, total))
    }
}
