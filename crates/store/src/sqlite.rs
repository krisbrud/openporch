use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::Arc;

use openporch_core::ids::Guresid;
use openporch_core::model::SecretRef;
use openporch_core::sets::{DeploymentSet, SET_ID_ALGORITHM};
use rand::RngCore;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{SqliteConnection, SqlitePool};

use crate::audit::{self, AuditEntry, AuditIntent, AuditOutcome};
use crate::deploy::*;
use crate::secrets::{BUILTIN_STORE, Sealed};
use crate::{Ownership, Result, Store, StoreError, now_ms};

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// The SQLite store. Open it with [`SqliteStore::open`] while holding the
/// installation's [`Ownership`].
#[derive(Debug, Clone)]
pub struct SqliteStore {
    pool: SqlitePool,
    org: String,
    ownership: Arc<Ownership>,
}

impl SqliteStore {
    /// Opens (creating if needed) `openporch.db` in the owned data directory, applies
    /// migrations, records the owner and makes sure the organization, the default
    /// `development` environment type and the built-in secret store exist.
    ///
    /// Fails if the database belongs to another organization or was written by a newer
    /// schema than this binary supports.
    ///
    /// The store keeps `ownership` for as long as it or any clone of it lives, so the
    /// installation lock cannot be released while the database is still in use.
    pub async fn open(ownership: Ownership, org: &str) -> Result<Self> {
        let options = SqliteConnectOptions::new()
            .filename(ownership.dir().join("openporch.db"))
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Full)
            .foreign_keys(true)
            .busy_timeout(std::time::Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(options)
            .await?;
        check_schema_version(&pool).await?;
        MIGRATOR.run(&pool).await?;
        let store = Self {
            pool,
            org: org.to_owned(),
            ownership: Arc::new(ownership),
        };
        store.init_installation().await?;
        Ok(store)
    }

    /// The connection pool, for components such as the built-in secret store.
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// The organization this installation serves.
    pub fn org(&self) -> &str {
        &self.org
    }

    /// The installation ownership this store holds.
    pub fn ownership(&self) -> &Ownership {
        &self.ownership
    }

    async fn init_installation(&self) -> Result<()> {
        let ownership = &*self.ownership;
        let now = now_ms();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let existing: Option<String> =
            sqlx::query_scalar("SELECT org_id FROM installation WHERE id = 1")
                .fetch_optional(&mut *tx)
                .await?;
        match existing {
            Some(org) if org != self.org => {
                return Err(StoreError::Conflict(format!(
                    "this installation serves organization `{org}`, not `{}`",
                    self.org
                )));
            }
            Some(_) => {}
            None => {
                sqlx::query("INSERT INTO installation (id, org_id, created_at) VALUES (1, ?, ?)")
                    .bind(&self.org)
                    .bind(now)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query(
                    "INSERT INTO env_types (org_id, id, description, created_at) \
                     VALUES (?, 'development', 'Default environment type', ?)",
                )
                .bind(&self.org)
                .bind(now)
                .execute(&mut *tx)
                .await?;
                sqlx::query(
                    "INSERT INTO secret_stores (org_id, id, type, is_primary, created_at) \
                     VALUES (?, ?, 'openporch', 1, ?)",
                )
                .bind(&self.org)
                .bind(BUILTIN_STORE)
                .bind(now)
                .execute(&mut *tx)
                .await?;
            }
        }
        sqlx::query(
            "UPDATE installation SET owner_id = ?, owner_pid = ?, owner_acquired_at = ? WHERE id = 1",
        )
        .bind(ownership.id())
        .bind(i64::from(std::process::id()))
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Checks that the database answers.
    pub async fn ping(&self) -> Result<()> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    /// Whether the installation was created by this start, i.e. it has no principals yet.
    pub async fn is_new(&self) -> Result<bool> {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM principals WHERE org_id = ?")
            .bind(&self.org)
            .fetch_one(&self.pool)
            .await?;
        Ok(n == 0)
    }

    pub(crate) async fn begin(&self) -> Result<sqlx::Transaction<'static, sqlx::Sqlite>> {
        // IMMEDIATE takes the write lock up front, so read-then-write checks in the
        // transaction cannot be invalidated by a concurrent writer.
        Ok(self.pool.begin_with("BEGIN IMMEDIATE").await?)
    }

    pub(crate) fn check_org(&self, org: &str) -> Result<()> {
        if org == self.org {
            Ok(())
        } else {
            Err(StoreError::NotFound(format!("organization `{org}`")))
        }
    }
}

/// Refuses databases written by a newer binary before any migration runs.
async fn check_schema_version(pool: &SqlitePool) -> Result<()> {
    let has_table: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    )
    .fetch_one(pool)
    .await?;
    if has_table == 0 {
        return Ok(());
    }
    let found: Option<i64> = sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations")
        .fetch_one(pool)
        .await?;
    let supported = MIGRATOR.iter().map(|m| m.version).max().unwrap_or(0);
    match found {
        Some(found) if found > supported => Err(StoreError::SchemaTooNew { found, supported }),
        _ => Ok(()),
    }
}

#[derive(sqlx::FromRow)]
struct DeploymentRow {
    id: String,
    app_id: String,
    env_id: String,
    seq: i64,
    actor_id: String,
    set_id: Option<String>,
    delta_id: Option<String>,
    comment: String,
    generation: i64,
    status: String,
    error: Option<String>,
    created_at: i64,
    finished_at: Option<i64>,
}

impl DeploymentRow {
    fn into_record(self) -> Result<DeploymentRecord> {
        Ok(DeploymentRecord {
            id: self.id,
            app: self.app_id,
            env: self.env_id,
            seq: self.seq,
            actor: self.actor_id,
            set_id: self.set_id,
            delta_id: self.delta_id,
            comment: self.comment,
            generation: self.generation,
            status: DeploymentStatus::parse(&self.status)?,
            error: self.error,
            created_at: self.created_at,
            finished_at: self.finished_at,
        })
    }
}

const DEPLOYMENT_COLUMNS: &str = "id, app_id, env_id, seq, actor_id, set_id, delta_id, comment, \
     generation, status, error, created_at, finished_at";

async fn fetch_deployment(
    conn: &mut SqliteConnection,
    org: &str,
    app: &str,
    env: &str,
    id: &str,
) -> Result<Option<DeploymentRecord>> {
    let row: Option<DeploymentRow> = sqlx::query_as(&format!(
        "SELECT {DEPLOYMENT_COLUMNS} FROM deployments \
         WHERE org_id = ? AND app_id = ? AND env_id = ? AND id = ?"
    ))
    .bind(org)
    .bind(app)
    .bind(env)
    .bind(id)
    .fetch_optional(conn)
    .await?;
    row.map(DeploymentRow::into_record).transpose()
}

async fn ensure_env_type(conn: &mut SqliteConnection, org: &str, id: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO env_types (org_id, id, created_at) VALUES (?, ?, ?) \
         ON CONFLICT (org_id, id) DO NOTHING",
    )
    .bind(org)
    .bind(id)
    .bind(now_ms())
    .execute(conn)
    .await?;
    Ok(())
}

fn unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.is_unique_violation())
}

fn new_guresid() -> Guresid {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    Guresid::from_bytes(bytes)
}

impl Store for SqliteStore {
    async fn record_audit(&self, intent: AuditIntent) -> Result<String> {
        self.check_org(&intent.org)?;
        let mut conn = self.pool.acquire().await?;
        audit::insert(&mut conn, &intent, None, None).await
    }

    async fn audit_entries(&self, org: &str, limit: u32) -> Result<Vec<AuditEntry>> {
        self.check_org(org)?;
        Ok(sqlx::query_as(
            "SELECT seq, id, at, actor_id, request_id, operation_id, method, path, action, target, \
             outcome, status_code, reason, correlates FROM audit_log WHERE org_id = ? \
             ORDER BY seq DESC LIMIT ?",
        )
        .bind(org)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?)
    }

    async fn create_env_type(
        &self,
        org: &str,
        id: &str,
        description: &str,
        audit: AuditIntent,
    ) -> Result<()> {
        self.check_org(org)?;
        let mut tx = self.begin().await?;
        crate::identity::check_admin_authority(&mut tx, org, audit.actor.as_deref()).await?;
        let res = sqlx::query(
            "INSERT INTO env_types (org_id, id, description, created_at, created_by) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(org)
        .bind(id)
        .bind(description)
        .bind(now_ms())
        .bind(&audit.actor)
        .execute(&mut *tx)
        .await;
        if let Err(e) = &res
            && unique_violation(e)
        {
            return Err(StoreError::AlreadyExists(format!(
                "environment type `{id}`"
            )));
        }
        res?;
        audit::insert(&mut tx, &audit, None, None).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn create_app(
        &self,
        org: &str,
        id: &str,
        name: &str,
        owner: &str,
        audit: AuditIntent,
    ) -> Result<()> {
        self.check_org(org)?;
        let now = now_ms();
        let mut tx = self.begin().await?;
        crate::identity::check_create_app_authority(&mut tx, org, audit.actor.as_deref()).await?;
        let res = sqlx::query(
            "INSERT INTO apps (org_id, id, name, created_at, created_by) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(org)
        .bind(id)
        .bind(name)
        .bind(now)
        .bind(owner)
        .execute(&mut *tx)
        .await;
        if let Err(e) = &res
            && unique_violation(e)
        {
            return Err(StoreError::AlreadyExists(format!("application `{id}`")));
        }
        res?;
        ensure_env_type(&mut tx, org, "development").await?;
        sqlx::query(
            "INSERT INTO envs (org_id, app_id, id, name, type, created_at, created_by) \
             VALUES (?, ?, 'development', 'Development', 'development', ?, ?)",
        )
        .bind(org)
        .bind(id)
        .bind(now)
        .bind(owner)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO app_roles (org_id, app_id, principal_id, role) VALUES (?, ?, ?, 'owner')",
        )
        .bind(org)
        .bind(id)
        .bind(owner)
        .execute(&mut *tx)
        .await?;
        audit::insert(&mut tx, &audit, None, None).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn create_env(
        &self,
        org: &str,
        app: &str,
        id: &str,
        name: &str,
        env_type: &str,
        audit: AuditIntent,
    ) -> Result<()> {
        self.check_org(org)?;
        let mut tx = self.begin().await?;
        let type_exists: Option<i64> =
            sqlx::query_scalar("SELECT 1 FROM env_types WHERE org_id = ? AND id = ?")
                .bind(org)
                .bind(env_type)
                .fetch_optional(&mut *tx)
                .await?;
        if type_exists.is_none() {
            return Err(StoreError::NotFound(format!(
                "environment type `{env_type}`"
            )));
        }
        let app_exists: Option<i64> =
            sqlx::query_scalar("SELECT 1 FROM apps WHERE org_id = ? AND id = ?")
                .bind(org)
                .bind(app)
                .fetch_optional(&mut *tx)
                .await?;
        if app_exists.is_none() {
            return Err(StoreError::NotFound(format!("application `{app}`")));
        }
        let res = sqlx::query(
            "INSERT INTO envs (org_id, app_id, id, name, type, created_at, created_by) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(org)
        .bind(app)
        .bind(id)
        .bind(name)
        .bind(env_type)
        .bind(now_ms())
        .bind(&audit.actor)
        .execute(&mut *tx)
        .await;
        if let Err(e) = &res
            && unique_violation(e)
        {
            return Err(StoreError::AlreadyExists(format!(
                "environment `{app}/{id}`"
            )));
        }
        res?;
        audit::insert(&mut tx, &audit, None, None).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn env(&self, org: &str, app: &str, env: &str) -> Result<Option<EnvRecord>> {
        self.check_org(org)?;
        #[allow(clippy::type_complexity)]
        let row: Option<(
            String,
            String,
            String,
            String,
            i64,
            Option<String>,
            Option<String>,
            Option<String>,
            String,
        )> = sqlx::query_as(
            "SELECT app_id, id, name, type, generation, desired_set_id, last_successful_set_id, \
             last_successful_deploy_id, state FROM envs WHERE org_id = ? AND app_id = ? AND id = ?",
        )
        .bind(org)
        .bind(app)
        .bind(env)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| EnvRecord {
            app: r.0,
            id: r.1,
            name: r.2,
            env_type: r.3,
            generation: r.4,
            desired_set_id: r.5,
            last_successful_set_id: r.6,
            last_successful_deploy_id: r.7,
            state: r.8,
        }))
    }

    async fn put_set(&self, org: &str, app: &str, set: &DeploymentSet) -> Result<String> {
        self.check_org(org)?;
        let id = set.id();
        sqlx::query(
            "INSERT INTO sets (org_id, app_id, id, algorithm, content, created_at) \
             VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT (org_id, app_id, id) DO NOTHING",
        )
        .bind(org)
        .bind(app)
        .bind(&id)
        .bind(SET_ID_ALGORITHM)
        .bind(set.canonical_json())
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    async fn put_delta(
        &self,
        org: &str,
        app: &str,
        id: &str,
        content: &serde_json::Value,
        audit: AuditIntent,
    ) -> Result<()> {
        self.check_org(org)?;
        let now = now_ms();
        let mut tx = self.begin().await?;
        let frozen: Option<Option<i64>> = sqlx::query_scalar(
            "SELECT frozen_at FROM deltas WHERE org_id = ? AND app_id = ? AND id = ?",
        )
        .bind(org)
        .bind(app)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(Some(_)) = frozen {
            return Err(StoreError::Conflict(format!(
                "delta `{id}` has been deployed and can no longer change"
            )));
        }
        sqlx::query(
            "INSERT INTO deltas (org_id, app_id, id, content, created_at, created_by, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?) ON CONFLICT (org_id, app_id, id) \
             DO UPDATE SET content = excluded.content, updated_at = excluded.updated_at",
        )
        .bind(org)
        .bind(app)
        .bind(id)
        .bind(serde_json::to_string(content)?)
        .bind(now)
        .bind(&audit.actor)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        audit::insert(&mut tx, &audit, None, None).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn accept_deployment(&self, req: DeploymentRequest) -> Result<Acceptance> {
        self.check_org(&req.org)?;
        let (org, app, env) = (&req.org, &req.app, &req.env);
        let mut tx = self.begin().await?;

        if let Some(key) = &req.idempotency_key {
            let prior: Option<(String, Option<String>)> = sqlx::query_as(
                "SELECT id, request_fingerprint FROM deployments WHERE org_id = ? AND app_id = ? \
                 AND env_id = ? AND actor_id = ? AND idempotency_key = ?",
            )
            .bind(org)
            .bind(app)
            .bind(env)
            .bind(&req.actor)
            .bind(key)
            .fetch_optional(&mut *tx)
            .await?;
            if let Some((id, fingerprint)) = prior {
                if fingerprint.as_deref() != Some(req.fingerprint.as_str()) {
                    return Err(StoreError::IdempotencyMismatch(key.clone()));
                }
                let d = fetch_deployment(&mut tx, org, app, env, &id)
                    .await?
                    .ok_or_else(|| StoreError::Corrupt(format!("deployment `{id}`")))?;
                return Ok(Acceptance::Replayed(d));
            }
        }

        let row: Option<(i64, String)> = sqlx::query_as(
            "SELECT generation, state FROM envs WHERE org_id = ? AND app_id = ? AND id = ?",
        )
        .bind(org)
        .bind(app)
        .bind(env)
        .fetch_optional(&mut *tx)
        .await?;
        let (generation, state) =
            row.ok_or_else(|| StoreError::NotFound(format!("environment `{app}/{env}`")))?;
        if state != "active" {
            return Err(StoreError::Conflict(format!(
                "environment `{app}/{env}` is {state}"
            )));
        }
        if generation != req.expected_generation {
            return Err(StoreError::StaleGeneration {
                expected: req.expected_generation,
                actual: generation,
            });
        }
        let running: Option<String> = sqlx::query_scalar(
            "SELECT id FROM deployments WHERE org_id = ? AND app_id = ? AND env_id = ? \
             AND status = 'in_progress'",
        )
        .bind(org)
        .bind(app)
        .bind(env)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(id) = running {
            return Err(StoreError::Busy(id));
        }
        let uncertain: Vec<String> = sqlx::query_scalar(
            "SELECT descriptor FROM active_resources WHERE org_id = ? AND app_id = ? AND env_id = ? \
             AND status IN ('unknown', 'recovery_required') ORDER BY descriptor",
        )
        .bind(org)
        .bind(app)
        .bind(env)
        .fetch_all(&mut *tx)
        .await?;
        if !uncertain.is_empty() {
            return Err(StoreError::RecoveryRequired(uncertain.join(", ")));
        }
        let set_exists: Option<i64> =
            sqlx::query_scalar("SELECT 1 FROM sets WHERE org_id = ? AND app_id = ? AND id = ?")
                .bind(org)
                .bind(app)
                .bind(&req.set_id)
                .fetch_optional(&mut *tx)
                .await?;
        if set_exists.is_none() {
            return Err(StoreError::NotFound(format!("set `{}`", req.set_id)));
        }
        if let Some(delta) = &req.delta_id {
            let updated = sqlx::query(
                "UPDATE deltas SET frozen_at = COALESCE(frozen_at, ?) \
                 WHERE org_id = ? AND app_id = ? AND id = ?",
            )
            .bind(now_ms())
            .bind(org)
            .bind(app)
            .bind(delta)
            .execute(&mut *tx)
            .await?;
            if updated.rows_affected() == 0 {
                return Err(StoreError::NotFound(format!("delta `{delta}`")));
            }
        }

        let id = crate::random_hex(16);
        let new_generation = generation + 1;
        let seq: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM deployments \
             WHERE org_id = ? AND app_id = ? AND env_id = ?",
        )
        .bind(org)
        .bind(app)
        .bind(env)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO deployments (org_id, app_id, env_id, id, seq, actor_id, set_id, delta_id, \
             comment, generation, status, idempotency_key, request_fingerprint, snapshot, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'in_progress', ?, ?, ?, ?)",
        )
        .bind(org)
        .bind(app)
        .bind(env)
        .bind(&id)
        .bind(seq)
        .bind(&req.actor)
        .bind(&req.set_id)
        .bind(&req.delta_id)
        .bind(&req.comment)
        .bind(new_generation)
        .bind(&req.idempotency_key)
        .bind(&req.fingerprint)
        .bind(serde_json::to_string(&req.snapshot)?)
        .bind(now_ms())
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE envs SET generation = ?, desired_set_id = ? \
             WHERE org_id = ? AND app_id = ? AND id = ?",
        )
        .bind(new_generation)
        .bind(&req.set_id)
        .bind(org)
        .bind(app)
        .bind(env)
        .execute(&mut *tx)
        .await?;
        let mut intent = req.audit.clone();
        intent.outcome = AuditOutcome::Accepted;
        intent.target.get_or_insert_with(|| format!("{app}/{env}"));
        audit::insert(&mut tx, &intent, Some(&id), None).await?;
        let d = fetch_deployment(&mut tx, org, app, env, &id)
            .await?
            .ok_or_else(|| StoreError::Corrupt(format!("deployment `{id}`")))?;
        tx.commit().await?;
        Ok(Acceptance::Created(d))
    }

    async fn deployment(
        &self,
        org: &str,
        app: &str,
        env: &str,
        id: &str,
    ) -> Result<Option<DeploymentRecord>> {
        self.check_org(org)?;
        let mut conn = self.pool.acquire().await?;
        fetch_deployment(&mut conn, org, app, env, id).await
    }

    async fn begin_operation(&self, start: OperationStart) -> Result<OperationStarted> {
        self.check_org(&start.org)?;
        let (org, app, env) = (&start.org, &start.app, &start.env);
        let descriptor = start.descriptor.to_string();
        let now = now_ms();
        let mut tx = self.begin().await?;
        let status: Option<String> = sqlx::query_scalar(
            "SELECT status FROM deployments WHERE org_id = ? AND app_id = ? AND env_id = ? AND id = ?",
        )
        .bind(org)
        .bind(app)
        .bind(env)
        .bind(&start.deployment_id)
        .fetch_optional(&mut *tx)
        .await?;
        match status.as_deref() {
            Some("in_progress") => {}
            Some(other) => {
                return Err(StoreError::Conflict(format!(
                    "deployment `{}` is {other}",
                    start.deployment_id
                )));
            }
            None => {
                return Err(StoreError::NotFound(format!(
                    "deployment `{}`",
                    start.deployment_id
                )));
            }
        }
        let existing: Option<(String, String)> = sqlx::query_as(
            "SELECT guresid, status FROM active_resources \
             WHERE org_id = ? AND app_id = ? AND env_id = ? AND descriptor = ?",
        )
        .bind(org)
        .bind(app)
        .bind(env)
        .bind(&descriptor)
        .fetch_optional(&mut *tx)
        .await?;
        let resource_status = match start.kind {
            OperationKind::Put => "provisioning",
            OperationKind::Delete => "deleting",
        };
        let guresid = match existing {
            Some((_, status)) if status == "unknown" || status == "recovery_required" => {
                return Err(StoreError::RecoveryRequired(descriptor));
            }
            Some((guresid, _)) => {
                let open: Option<String> = sqlx::query_scalar(
                    "SELECT id FROM operations WHERE guresid = ? AND state = 'started'",
                )
                .bind(&guresid)
                .fetch_optional(&mut *tx)
                .await?;
                if let Some(op) = open {
                    return Err(StoreError::Conflict(format!(
                        "operation `{op}` on `{descriptor}` has not finished"
                    )));
                }
                sqlx::query(
                    "UPDATE active_resources SET status = ?, definition_id = COALESCE(?, definition_id), \
                     definition_version = COALESCE(?, definition_version), \
                     driver_type = COALESCE(?, driver_type), recovery = ?, updated_at = ? \
                     WHERE guresid = ?",
                )
                .bind(resource_status)
                .bind(&start.definition_id)
                .bind(start.definition_version)
                .bind(&start.driver_type)
                .bind(serde_json::to_string(&start.recovery)?)
                .bind(now)
                .bind(&guresid)
                .execute(&mut *tx)
                .await?;
                Guresid::from_str(&guresid).map_err(|e| StoreError::Corrupt(e.to_string()))?
            }
            None if start.kind == OperationKind::Delete => {
                return Err(StoreError::NotFound(format!(
                    "active resource `{descriptor}`"
                )));
            }
            None => {
                let guresid = new_guresid();
                sqlx::query(
                    "INSERT INTO active_resources (org_id, app_id, env_id, descriptor, guresid, \
                     definition_id, definition_version, driver_type, status, recovery, created_at, \
                     updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'provisioning', ?, ?, ?)",
                )
                .bind(org)
                .bind(app)
                .bind(env)
                .bind(&descriptor)
                .bind(guresid.as_str())
                .bind(&start.definition_id)
                .bind(start.definition_version)
                .bind(&start.driver_type)
                .bind(serde_json::to_string(&start.recovery)?)
                .bind(now)
                .bind(now)
                .execute(&mut *tx)
                .await?;
                guresid
            }
        };
        let attempt: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) + 1 FROM operations WHERE deployment_id = ? AND guresid = ?",
        )
        .bind(&start.deployment_id)
        .bind(guresid.as_str())
        .fetch_one(&mut *tx)
        .await?;
        let operation_id = crate::random_hex(16);
        sqlx::query(
            "INSERT INTO operations (id, org_id, app_id, env_id, deployment_id, guresid, kind, \
             input_fingerprint, state, attempt, recovery, started_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'started', ?, ?, ?)",
        )
        .bind(&operation_id)
        .bind(org)
        .bind(app)
        .bind(env)
        .bind(&start.deployment_id)
        .bind(guresid.as_str())
        .bind(start.kind.as_str())
        .bind(&start.input_fingerprint)
        .bind(attempt)
        .bind(serde_json::to_string(&start.recovery)?)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO deployment_resources (org_id, app_id, env_id, deployment_id, descriptor, \
             guresid, status, attempts) VALUES (?, ?, ?, ?, ?, ?, 'provisioning', ?) \
             ON CONFLICT (org_id, app_id, env_id, deployment_id, descriptor) \
             DO UPDATE SET status = 'provisioning', attempts = excluded.attempts, error = NULL",
        )
        .bind(org)
        .bind(app)
        .bind(env)
        .bind(&start.deployment_id)
        .bind(&descriptor)
        .bind(guresid.as_str())
        .bind(attempt)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(OperationStarted {
            operation_id,
            guresid,
            attempt,
        })
    }

    async fn checkpoint_operation(
        &self,
        operation_id: &str,
        result: CheckpointResult,
    ) -> Result<()> {
        let now = now_ms();
        let mut tx = self.begin().await?;
        #[allow(clippy::type_complexity)]
        let op: Option<(String, String, String, String, String, String, String)> = sqlx::query_as(
            "SELECT org_id, app_id, env_id, deployment_id, guresid, state, kind FROM operations \
             WHERE id = ?",
        )
        .bind(operation_id)
        .fetch_optional(&mut *tx)
        .await?;
        let (org, app, env, deployment_id, guresid, state, kind) =
            op.ok_or_else(|| StoreError::NotFound(format!("operation `{operation_id}`")))?;
        self.check_org(&org)?;
        if state != "started" && state != "unknown" {
            return Err(StoreError::Conflict(format!(
                "operation `{operation_id}` is already {state}"
            )));
        }
        match (&result, kind.as_str()) {
            (CheckpointResult::Provisioned(_), "put") | (CheckpointResult::Deleted, "delete") => {}
            (CheckpointResult::Provisioned(_) | CheckpointResult::Deleted, _) => {
                return Err(StoreError::Conflict(format!(
                    "operation `{operation_id}` is a {kind}; the result does not match it"
                )));
            }
            // An operation whose outcome is unknown is resolved only by observing the
            // resource: a guess or another failure leaves it blocking.
            (CheckpointResult::Failed { .. } | CheckpointResult::Unknown { .. }, _)
                if state == "unknown" =>
            {
                return Err(StoreError::Conflict(format!(
                    "operation `{operation_id}` has an unknown outcome; it is resolved only by \
                     an observation of the resource"
                )));
            }
            _ => {}
        }
        let descriptor: String =
            sqlx::query_scalar("SELECT descriptor FROM active_resources WHERE guresid = ?")
                .bind(&guresid)
                .fetch_one(&mut *tx)
                .await?;
        let (op_state, resource_status, run_status, error, outputs) = match &result {
            CheckpointResult::Provisioned(p) => {
                let Provisioned {
                    outputs,
                    secret_outputs,
                    cookie,
                    recovery,
                } = &**p;
                sqlx::query(
                    "UPDATE active_resources SET status = 'provisioned', outputs = ?, \
                     secret_outputs = ?, cookie_key_id = ?, cookie_nonce = ?, cookie = ?, \
                     recovery = ?, last_deploy_id = ?, observed_at = ?, updated_at = ? \
                     WHERE guresid = ?",
                )
                .bind(serde_json::to_string(outputs)?)
                .bind(serde_json::to_string(secret_outputs)?)
                .bind(cookie.as_ref().map(|c| c.key_id.clone()))
                .bind(cookie.as_ref().map(|c| c.nonce.clone()))
                .bind(cookie.as_ref().map(|c| c.ciphertext.clone()))
                .bind(serde_json::to_string(recovery)?)
                .bind(&deployment_id)
                .bind(now)
                .bind(now)
                .bind(&guresid)
                .execute(&mut *tx)
                .await?;
                (
                    "succeeded",
                    None,
                    "provisioned",
                    None,
                    Some(outputs.clone()),
                )
            }
            CheckpointResult::Deleted => {
                sqlx::query(
                    "INSERT INTO tombstones (org_id, app_id, env_id, descriptor, guresid, reason, \
                     actor_id, body, at) SELECT org_id, app_id, env_id, descriptor, guresid, \
                     'deleted', NULL, json_object('definition_id', definition_id, 'driver_type', \
                     driver_type, 'external_ids', json(external_ids), 'backend', backend), ? \
                     FROM active_resources WHERE guresid = ?",
                )
                .bind(now)
                .bind(&guresid)
                .execute(&mut *tx)
                .await?;
                sqlx::query("DELETE FROM active_resources WHERE guresid = ?")
                    .bind(&guresid)
                    .execute(&mut *tx)
                    .await?;
                ("succeeded", None, "deleted", None, None)
            }
            CheckpointResult::Failed { error } => (
                "failed",
                Some(if kind == "delete" {
                    "deletion_failed"
                } else {
                    "failed"
                }),
                "failed",
                Some(error.clone()),
                None,
            ),
            CheckpointResult::Unknown { error } => (
                "unknown",
                Some("unknown"),
                "unknown",
                Some(error.clone()),
                None,
            ),
        };
        if let Some(status) = resource_status {
            sqlx::query(
                "UPDATE active_resources SET status = ?, observed_at = ?, updated_at = ? \
                 WHERE guresid = ?",
            )
            .bind(status)
            .bind(now)
            .bind(now)
            .bind(&guresid)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query("UPDATE operations SET state = ?, error = ?, finished_at = ? WHERE id = ?")
            .bind(op_state)
            .bind(&error)
            .bind(now)
            .bind(operation_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE deployment_resources SET status = ?, error = ?, outputs = ?, observed_at = ? \
             WHERE org_id = ? AND app_id = ? AND env_id = ? AND deployment_id = ? AND descriptor = ?",
        )
        .bind(run_status)
        .bind(&error)
        .bind(outputs.map(|o| o.to_string()))
        .bind(now)
        .bind(&org)
        .bind(&app)
        .bind(&env)
        .bind(&deployment_id)
        .bind(&descriptor)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn active_resource(
        &self,
        org: &str,
        app: &str,
        env: &str,
        descriptor: &str,
    ) -> Result<Option<ActiveResource>> {
        self.check_org(org)?;
        #[allow(clippy::type_complexity)]
        let row: Option<(
            String,
            Option<String>,
            Option<String>,
            String,
            String,
            String,
            Option<String>,
            Option<Vec<u8>>,
            Option<Vec<u8>>,
            Option<String>,
            Option<i64>,
        )> = sqlx::query_as(
            "SELECT guresid, definition_id, driver_type, status, outputs, secret_outputs, \
             cookie_key_id, cookie_nonce, cookie, last_deploy_id, observed_at FROM active_resources \
             WHERE org_id = ? AND app_id = ? AND env_id = ? AND descriptor = ?",
        )
        .bind(org)
        .bind(app)
        .bind(env)
        .bind(descriptor)
        .fetch_optional(&self.pool)
        .await?;
        let Some(r) = row else { return Ok(None) };
        let secret_outputs: BTreeMap<String, SecretRef> = serde_json::from_str(&r.5)?;
        let cookie = match (r.6, r.7, r.8) {
            (Some(key_id), Some(nonce), Some(ciphertext)) => Some(Sealed {
                key_id,
                nonce,
                ciphertext,
            }),
            _ => None,
        };
        Ok(Some(ActiveResource {
            descriptor: descriptor.to_owned(),
            guresid: r.0,
            definition_id: r.1,
            driver_type: r.2,
            status: r.3,
            outputs: serde_json::from_str(&r.4)?,
            secret_outputs,
            cookie,
            last_deploy_id: r.9,
            observed_at: r.10,
        }))
    }

    async fn finish_deployment(
        &self,
        org: &str,
        app: &str,
        env: &str,
        id: &str,
        error: Option<String>,
    ) -> Result<()> {
        self.check_org(org)?;
        let mut tx = self.begin().await?;
        finish_in_tx(&mut tx, org, app, env, id, error.as_deref()).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn recover_interrupted(&self) -> Result<RecoveryReport> {
        let mut tx = self.begin().await?;
        let running: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT app_id, env_id, id FROM deployments WHERE org_id = ? AND status = 'in_progress' \
             ORDER BY created_at",
        )
        .bind(&self.org)
        .fetch_all(&mut *tx)
        .await?;
        let uncertain: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT id, deployment_id, guresid FROM operations WHERE org_id = ? AND state = 'started' \
             ORDER BY started_at",
        )
        .bind(&self.org)
        .fetch_all(&mut *tx)
        .await?;
        let now = now_ms();
        for (op, deployment, guresid) in &uncertain {
            sqlx::query(
                "UPDATE operations SET state = 'unknown', error = 'interrupted by a restart' \
                 WHERE id = ?",
            )
            .bind(op)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "UPDATE active_resources SET status = 'unknown', updated_at = ? WHERE guresid = ?",
            )
            .bind(now)
            .bind(guresid)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "UPDATE deployment_resources SET status = 'unknown', \
                 error = 'interrupted by a restart' WHERE deployment_id = ? AND guresid = ?",
            )
            .bind(deployment)
            .bind(guresid)
            .execute(&mut *tx)
            .await?;
        }
        for (app, env, id) in &running {
            finish_in_tx(
                &mut tx,
                &self.org,
                app,
                env,
                id,
                Some("interrupted by a server restart"),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(RecoveryReport {
            failed_deployments: running.into_iter().map(|r| r.2).collect(),
            uncertain: uncertain
                .into_iter()
                .map(
                    |(operation_id, deployment_id, guresid)| UncertainOperation {
                        operation_id,
                        deployment_id,
                        guresid,
                    },
                )
                .collect(),
        })
    }

    async fn required_key_ids(&self) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar(
            "SELECT key_id FROM secret_versions UNION SELECT key_id FROM signing_keys \
             UNION SELECT cookie_key_id FROM active_resources WHERE cookie_key_id IS NOT NULL \
             ORDER BY 1",
        )
        .fetch_all(&self.pool)
        .await?)
    }
}

/// Marks a running deployment finished and records the correlated audit outcome.
async fn finish_in_tx(
    conn: &mut SqliteConnection,
    org: &str,
    app: &str,
    env: &str,
    id: &str,
    error: Option<&str>,
) -> Result<()> {
    let row: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT status, set_id FROM deployments \
         WHERE org_id = ? AND app_id = ? AND env_id = ? AND id = ?",
    )
    .bind(org)
    .bind(app)
    .bind(env)
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?;
    let (status, set_id) = row.ok_or_else(|| StoreError::NotFound(format!("deployment `{id}`")))?;
    if status != "in_progress" {
        return Err(StoreError::Conflict(format!(
            "deployment `{id}` is already {status}"
        )));
    }
    let status = if error.is_some() {
        "failed"
    } else {
        "succeeded"
    };
    sqlx::query(
        "UPDATE deployments SET status = ?, error = ?, finished_at = ? \
         WHERE org_id = ? AND app_id = ? AND env_id = ? AND id = ?",
    )
    .bind(status)
    .bind(error)
    .bind(now_ms())
    .bind(org)
    .bind(app)
    .bind(env)
    .bind(id)
    .execute(&mut *conn)
    .await?;
    if error.is_none() {
        sqlx::query(
            "UPDATE envs SET last_successful_set_id = ?, last_successful_deploy_id = ? \
             WHERE org_id = ? AND app_id = ? AND id = ?",
        )
        .bind(&set_id)
        .bind(id)
        .bind(org)
        .bind(app)
        .bind(env)
        .execute(&mut *conn)
        .await?;
    }
    let accepted: Option<(String, Option<String>, String, String)> = sqlx::query_as(
        "SELECT id, actor_id, method, path FROM audit_log WHERE org_id = ? AND operation_id = ? \
         AND outcome = 'accepted' ORDER BY seq LIMIT 1",
    )
    .bind(org)
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?;
    let (correlates, actor, method, path) = match accepted {
        Some((a, actor, m, p)) => (Some(a), actor, m, p),
        None => (
            None,
            None,
            "POST".to_owned(),
            format!("/orgs/{org}/apps/{app}/envs/{env}/deploys"),
        ),
    };
    let intent = AuditIntent {
        org: org.to_owned(),
        actor,
        request_id: None,
        method,
        path,
        action: "deployment.finish".to_owned(),
        target: Some(format!("{app}/{env}")),
        outcome: if error.is_some() {
            AuditOutcome::Failed
        } else {
            AuditOutcome::Succeeded
        },
        status_code: None,
        reason: error.map(|_| "deployment_failed".to_owned()),
    };
    audit::insert(conn, &intent, Some(id), correlates.as_deref()).await?;
    Ok(())
}

#[cfg(test)]
mod tests;
