//! Backups and restores. A restored database may be older than the cloud:
//! resources may have changed, or been deleted, after the backup was taken.
//! So every resource the backup records as provisioned gets an operation of
//! unknown outcome, which blocks it until its driver has observed it, the way
//! a restart's interrupted operations do.

use std::path::Path;

use sqlx::sqlite::SqlitePoolOptions;

use crate::deploy::UncertainOperation;
use crate::sqlite::{SqliteStore, connect_options};
use crate::{Ownership, Result, StoreError};

/// The error the restore's operations carry until they are observed.
pub const RESTORED: &str = "restored from a backup; observing the resource before it is changed";

/// What a database copy holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseCopy {
    /// The newest migration applied to it.
    pub schema_version: i64,
    /// The organization it serves, if it was ever started.
    pub org: Option<String>,
    /// The master key IDs its secrets, signing keys and cookies were
    /// encrypted with.
    pub key_ids: Vec<String>,
    /// How many resources it records as active.
    pub active_resources: i64,
}

/// Copies the database of the installation `ownership` holds to `dest`, a
/// file that must not exist yet, consistently and without changing it: it is
/// not migrated. The copy is in WAL mode, as [`SqliteStore::open`] expects.
pub async fn copy_database(ownership: &Ownership, dest: &Path) -> Result<DatabaseCopy> {
    let source = ownership.dir().join("openporch.db");
    if !source.is_file() {
        return Err(StoreError::NotFound(format!(
            "a database at {}",
            source.display()
        )));
    }
    let io = |context: String| move |source| StoreError::Io { context, source };
    // `VACUUM INTO` fills an empty file, so the copy gets owner-only
    // permissions before anything is written to it.
    let mut file = std::fs::OpenOptions::new();
    file.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut file, 0o600);
    file.open(dest)
        .map_err(io(format!("creating {}", dest.display())))?;
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(connect_options(&source).create_if_missing(false))
        .await?;
    let dest_name = dest
        .to_str()
        .ok_or_else(|| StoreError::Invalid(format!("{} is not UTF-8", dest.display())))?;
    let vacuumed = sqlx::query("VACUUM INTO ?")
        .bind(dest_name)
        .execute(&pool)
        .await;
    pool.close().await;
    vacuumed?;
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(connect_options(dest).create_if_missing(false))
        .await?;
    let copy = describe(&pool).await;
    pool.close().await;
    copy
}

async fn describe(pool: &sqlx::SqlitePool) -> Result<DatabaseCopy> {
    let schema_version: Option<i64> =
        sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations")
            .fetch_one(pool)
            .await?;
    let org: Option<String> = sqlx::query_scalar("SELECT org_id FROM installation WHERE id = 1")
        .fetch_optional(pool)
        .await?;
    let key_ids: Vec<String> = sqlx::query_scalar(
        "SELECT key_id FROM secret_versions UNION SELECT key_id FROM signing_keys \
         UNION SELECT cookie_key_id FROM active_resources WHERE cookie_key_id IS NOT NULL \
         ORDER BY 1",
    )
    .fetch_all(pool)
    .await?;
    let active_resources: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM active_resources")
        .fetch_one(pool)
        .await?;
    Ok(DatabaseCopy {
        schema_version: schema_version.unwrap_or(0),
        org,
        key_ids,
        active_resources,
    })
}

impl SqliteStore {
    /// Records an operation of unknown outcome, marked `restored` in its
    /// recovery data, for every active resource whose last successful
    /// provisioning recorded its inputs, and returns them to be observed.
    /// Operations an earlier start marked and nobody resolved are returned
    /// again. A resource with an operation already open is left to that
    /// operation's recovery; one never provisioned has nothing to observe;
    /// one being deleted is deleted by the next deployment, which a resource
    /// already gone does not fail.
    pub async fn mark_restored(&self) -> Result<Vec<UncertainOperation>> {
        let mut tx = self.begin().await?;
        let now = crate::now_ms();
        let resources: Vec<(String, String, String, String, String)> = sqlx::query_as(
            "SELECT guresid, app_id, env_id, descriptor, status FROM active_resources \
             WHERE org_id = ? AND cleanup_pending = 0 AND status != 'deletion_failed' \
             ORDER BY app_id, env_id, descriptor",
        )
        .bind(self.org())
        .fetch_all(&mut *tx)
        .await?;
        let mut marked = Vec::new();
        for (guresid, app, env, descriptor, status) in resources {
            let open: Option<(String, String, Option<i64>)> = sqlx::query_as(
                "SELECT id, deployment_id, json_extract(recovery, '$.restored') FROM operations \
                 WHERE guresid = ? AND state IN ('started', 'unknown') \
                 ORDER BY started_at DESC LIMIT 1",
            )
            .bind(&guresid)
            .fetch_optional(&mut *tx)
            .await?;
            if let Some((operation_id, deployment_id, restored)) = open {
                if restored == Some(1) {
                    marked.push(UncertainOperation {
                        operation_id,
                        deployment_id,
                        guresid,
                        app,
                        env,
                        descriptor,
                    });
                }
                continue;
            }
            let last: Option<(String, String, i64, String)> = sqlx::query_as(
                "SELECT deployment_id, input_fingerprint, attempt, recovery FROM operations \
                 WHERE guresid = ? AND kind = 'put' AND state = 'succeeded' \
                 AND json_type(recovery, '$.inputs') IS NOT NULL \
                 ORDER BY started_at DESC LIMIT 1",
            )
            .bind(&guresid)
            .fetch_optional(&mut *tx)
            .await?;
            let Some((deployment_id, fingerprint, attempt, recovery)) = last else {
                continue;
            };
            let mut recovery: serde_json::Value = serde_json::from_str(&recovery)?;
            recovery["restored"] = serde_json::Value::Bool(true);
            recovery["restored_status"] = serde_json::Value::String(status);
            let operation_id = crate::random_hex(16);
            sqlx::query(
                "INSERT INTO operations (id, org_id, app_id, env_id, deployment_id, guresid, \
                 kind, input_fingerprint, state, attempt, recovery, error, started_at) \
                 VALUES (?, ?, ?, ?, ?, ?, 'put', ?, 'unknown', ?, ?, ?, ?)",
            )
            .bind(&operation_id)
            .bind(self.org())
            .bind(&app)
            .bind(&env)
            .bind(&deployment_id)
            .bind(&guresid)
            .bind(&fingerprint)
            .bind(attempt)
            .bind(recovery.to_string())
            .bind(RESTORED)
            .bind(now)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "UPDATE active_resources SET status = 'unknown', updated_at = ? WHERE guresid = ?",
            )
            .bind(now)
            .bind(&guresid)
            .execute(&mut *tx)
            .await?;
            marked.push(UncertainOperation {
                operation_id,
                deployment_id,
                guresid,
                app,
                env,
                descriptor,
            });
        }
        tx.commit().await?;
        Ok(marked)
    }
}
