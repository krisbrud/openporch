//! Audit log retention. Entries never expire on their own: an administrator
//! expires those older than the retention period, and the expiry is itself
//! audited in the same transaction, so entries are never lost silently.

use serde::Serialize;

use crate::Result;
use crate::audit::{self, AuditIntent};
use crate::sqlite::SqliteStore;

/// How the audit log stands against its retention period.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuditRetention {
    /// Entries kept.
    pub entries: i64,
    /// When the oldest entry was written, in milliseconds since the epoch.
    pub oldest_at: Option<i64>,
    /// Entries older than the retention period, which expiry would remove.
    pub expirable: i64,
    /// The last expiry, successful or not.
    pub last_expiry: Option<Expiry>,
}

/// A recorded expiry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct Expiry {
    /// Its audit entry.
    pub id: String,
    /// When it ran.
    pub at: i64,
    /// Who ran it.
    pub user_id: Option<String>,
    /// `succeeded` or `failed`.
    pub outcome: String,
    /// What it removed, or why it failed.
    pub target: Option<String>,
    /// Why it failed.
    pub reason: Option<String>,
}

/// What audit expiry keeps: everything but audit entries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Retained {
    /// Active resources, whose cloud counterparts may keep costing money.
    pub resources: i64,
    /// Active resources by driver, such as `openporch/opentofu`.
    pub by_driver: std::collections::BTreeMap<String, i64>,
    /// Resources whose outcome is unknown, with their recovery records.
    pub needing_recovery: i64,
    /// Resources whose deletion still has to be cleaned up.
    pub cleanup_pending: i64,
}

/// The audit action an expiry records.
pub const EXPIRY_ACTION: &str = "audit_log.expire";

impl SqliteStore {
    /// How the audit log stands against a retention period that keeps
    /// entries written at or after `before`.
    pub async fn audit_retention(&self, before: i64) -> Result<AuditRetention> {
        let (entries, oldest_at, expirable): (i64, Option<i64>, i64) = sqlx::query_as(
            "SELECT COUNT(*), MIN(at), COALESCE(SUM(at < ?), 0) FROM audit_log WHERE org_id = ?",
        )
        .bind(before)
        .bind(self.org())
        .fetch_one(self.pool())
        .await?;
        let last_expiry = sqlx::query_as(
            "SELECT id, at, actor_id AS user_id, outcome, target, reason FROM audit_log \
             WHERE org_id = ? AND action = ? ORDER BY seq DESC LIMIT 1",
        )
        .bind(self.org())
        .bind(EXPIRY_ACTION)
        .fetch_optional(self.pool())
        .await?;
        Ok(AuditRetention {
            entries,
            oldest_at,
            expirable,
            last_expiry,
        })
    }

    /// Removes the audit entries written before `before` and records the
    /// expiry with `audit`, in one transaction; returns how many were removed
    /// and how many are kept. `audit` must be an administrator's.
    pub async fn expire_audit(&self, before: i64, mut audit: AuditIntent) -> Result<(u64, i64)> {
        self.check_org(&audit.org)?;
        let mut tx = self.begin().await?;
        crate::identity::check_admin_authority(&mut tx, self.org(), audit.actor.as_deref()).await?;
        let removed = sqlx::query("DELETE FROM audit_log WHERE org_id = ? AND at < ?")
            .bind(self.org())
            .bind(before)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        audit.target = Some(format!("{removed} entries before {before}"));
        audit::insert(&mut tx, &audit, None, None).await?;
        let kept = sqlx::query_scalar("SELECT COUNT(*) FROM audit_log WHERE org_id = ?")
            .bind(self.org())
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok((removed, kept))
    }

    /// What expiry keeps.
    pub async fn retained(&self) -> Result<Retained> {
        let by_driver: Vec<(String, i64)> = sqlx::query_as(
            "SELECT COALESCE(driver_type, 'none'), COUNT(*) FROM active_resources \
             WHERE org_id = ? GROUP BY 1",
        )
        .bind(self.org())
        .fetch_all(self.pool())
        .await?;
        let (needing_recovery, cleanup_pending): (i64, i64) = sqlx::query_as(
            "SELECT COALESCE(SUM(status IN ('unknown', 'recovery_required')), 0), \
             COALESCE(SUM(cleanup_pending != 0), 0) FROM active_resources WHERE org_id = ?",
        )
        .bind(self.org())
        .fetch_one(self.pool())
        .await?;
        Ok(Retained {
            resources: by_driver.iter().map(|(_, n)| n).sum(),
            by_driver: by_driver.into_iter().collect(),
            needing_recovery,
            cleanup_pending,
        })
    }
}
