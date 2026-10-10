use serde::Serialize;
use sqlx::SqliteConnection;

use crate::Result;

/// The outcome an audit entry records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditOutcome {
    /// A mutation was accepted; asynchronous work may follow.
    Accepted,
    /// Work completed.
    Succeeded,
    /// Work failed.
    Failed,
    /// A privileged request was denied.
    Denied,
}

impl AuditOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Denied => "denied",
        }
    }
}

/// What an audit entry records about one request. Bodies, tokens and secret values
/// are never part of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditIntent {
    /// The organization.
    pub org: String,
    /// The principal, when known.
    pub actor: Option<String>,
    /// The request ID.
    pub request_id: Option<String>,
    /// The HTTP method.
    pub method: String,
    /// The request path.
    pub path: String,
    /// A stable action name such as `deployment.create`.
    pub action: String,
    /// The target object, such as `shop/dev`.
    pub target: Option<String>,
    /// The outcome.
    pub outcome: AuditOutcome,
    /// The HTTP status code returned.
    pub status_code: Option<u16>,
    /// A reason code, such as `missing_deployer_role`.
    pub reason: Option<String>,
}

impl AuditIntent {
    /// An accepted mutation by `actor`.
    pub fn accepted(org: &str, actor: &str, method: &str, path: &str, action: &str) -> Self {
        Self {
            org: org.to_owned(),
            actor: Some(actor.to_owned()),
            request_id: None,
            method: method.to_owned(),
            path: path.to_owned(),
            action: action.to_owned(),
            target: None,
            outcome: AuditOutcome::Accepted,
            status_code: None,
            reason: None,
        }
    }
}

/// A stored audit entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct AuditEntry {
    /// Position in the log; pass it as `before` to read older entries.
    pub seq: i64,
    /// Stable entry ID.
    pub id: String,
    /// Timestamp in Unix milliseconds.
    pub at: i64,
    /// The principal, when known.
    #[serde(rename = "user_id")]
    pub actor_id: Option<String>,
    /// The request ID.
    pub request_id: Option<String>,
    /// The operation the entry belongs to, such as a deployment ID.
    pub operation_id: Option<String>,
    /// The HTTP method.
    pub method: String,
    /// The request path.
    pub path: String,
    /// The action name.
    pub action: String,
    /// The target object.
    pub target: Option<String>,
    /// `accepted`, `succeeded`, `failed` or `denied`.
    pub outcome: String,
    /// The HTTP status code.
    pub status_code: Option<i64>,
    /// The reason code.
    pub reason: Option<String>,
    /// The ID of the entry this one reports the outcome of.
    pub correlates: Option<String>,
}

/// Inserts an audit entry inside the caller's transaction and returns its ID.
pub(crate) async fn insert(
    conn: &mut SqliteConnection,
    intent: &AuditIntent,
    operation_id: Option<&str>,
    correlates: Option<&str>,
) -> Result<String> {
    let id = crate::random_hex(16);
    sqlx::query(
        "INSERT INTO audit_log (id, at, org_id, actor_id, request_id, operation_id, method, path, \
         action, target, outcome, status_code, reason, correlates) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(crate::now_ms())
    .bind(&intent.org)
    .bind(&intent.actor)
    .bind(&intent.request_id)
    .bind(operation_id)
    .bind(&intent.method)
    .bind(&intent.path)
    .bind(&intent.action)
    .bind(&intent.target)
    .bind(intent.outcome.as_str())
    .bind(intent.status_code.map(i64::from))
    .bind(&intent.reason)
    .bind(correlates)
    .execute(conn)
    .await?;
    Ok(id)
}
