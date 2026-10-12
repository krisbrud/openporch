use std::str::FromStr;
use std::sync::Arc;

use openporch_core::ids::Guresid;
use openporch_core::sets::{DeploymentSet, SET_ID_ALGORITHM};
use rand::RngCore;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{SqliteConnection, SqlitePool};

use crate::audit::{self, AuditEntry, AuditIntent, AuditOutcome};
use crate::deploy::*;
use crate::identity::{check_admin_authority, check_configure_authority, check_env_authority};
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

#[derive(sqlx::FromRow)]
struct EnvRow {
    app_id: String,
    id: String,
    name: String,
    #[sqlx(rename = "type")]
    env_type: String,
    generation: i64,
    desired_set_id: Option<String>,
    last_successful_set_id: Option<String>,
    last_successful_deploy_id: Option<String>,
    state: String,
}

impl EnvRow {
    fn into_record(self) -> EnvRecord {
        EnvRecord {
            app: self.app_id,
            id: self.id,
            name: self.name,
            env_type: self.env_type,
            generation: self.generation,
            desired_set_id: self.desired_set_id,
            last_successful_set_id: self.last_successful_set_id,
            last_successful_deploy_id: self.last_successful_deploy_id,
            state: self.state,
        }
    }
}

const ENV_COLUMNS: &str = "app_id, id, name, type, generation, desired_set_id, \
     last_successful_set_id, last_successful_deploy_id, state";

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

pub(crate) fn unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.is_unique_violation())
}

fn new_guresid() -> Guresid {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    Guresid::from_bytes(bytes)
}

type DeltaRow = (String, Option<String>, String, Option<i64>, Option<String>);

fn delta_record((id, env, content, frozen_at, created_by): DeltaRow) -> Result<DeltaRecord> {
    Ok(DeltaRecord {
        content: serde_json::from_str(&content)
            .map_err(|e| StoreError::Corrupt(format!("delta `{id}`: {e}")))?,
        id,
        env,
        frozen: frozen_at.is_some(),
        created_by,
    })
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
        env: NewEnv,
        audit: AuditIntent,
    ) -> Result<()> {
        self.check_org(org)?;
        let NewEnv {
            id,
            name,
            env_type,
            draft,
        } = env;
        let (id, name, env_type) = (id.as_str(), name.as_str(), env_type.as_str());
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
        check_env_authority(&mut tx, org, audit.actor.as_deref(), app, env_type).await?;
        let now = now_ms();
        let res = sqlx::query(
            "INSERT INTO envs (org_id, app_id, id, name, type, created_at, created_by) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(org)
        .bind(app)
        .bind(id)
        .bind(name)
        .bind(env_type)
        .bind(now)
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
        if let Some(draft) = draft {
            let res = sqlx::query(
                "INSERT INTO deltas (org_id, app_id, id, env_id, content, created_at, created_by, \
                 updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(org)
            .bind(app)
            .bind(&draft.id)
            .bind(id)
            .bind(draft.content.to_string())
            .bind(now)
            .bind(&audit.actor)
            .bind(now)
            .execute(&mut *tx)
            .await;
            if let Err(e) = &res
                && unique_violation(e)
            {
                return Err(StoreError::AlreadyExists(format!("delta `{}`", draft.id)));
            }
            res?;
        }
        audit::insert(&mut tx, &audit, None, None).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn envs(&self, org: &str, app: &str) -> Result<Vec<EnvRecord>> {
        self.check_org(org)?;
        let rows: Vec<EnvRow> = sqlx::query_as(&format!(
            "SELECT {ENV_COLUMNS} FROM envs WHERE org_id = ? AND app_id = ? ORDER BY id"
        ))
        .bind(org)
        .bind(app)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(EnvRow::into_record).collect())
    }

    async fn delete_env(&self, org: &str, app: &str, env: &str, audit: AuditIntent) -> Result<()> {
        self.check_org(org)?;
        let mut tx = self.begin().await?;
        let env_type: Option<String> =
            sqlx::query_scalar("SELECT type FROM envs WHERE org_id = ? AND app_id = ? AND id = ?")
                .bind(org)
                .bind(app)
                .bind(env)
                .fetch_optional(&mut *tx)
                .await?;
        let Some(env_type) = env_type else {
            return Err(StoreError::NotFound(format!("environment `{app}/{env}`")));
        };
        check_env_authority(&mut tx, org, audit.actor.as_deref(), app, &env_type).await?;
        if env == "development" {
            return Err(StoreError::Conflict(
                "the `development` environment exists as long as its application".into(),
            ));
        }
        let (deployed,): (i64,) = sqlx::query_as(
            "SELECT (SELECT COUNT(*) FROM deployments WHERE org_id = ?1 AND app_id = ?2 AND env_id = ?3) \
                  + (SELECT COUNT(*) FROM active_resources WHERE org_id = ?1 AND app_id = ?2 AND env_id = ?3)",
        )
        .bind(org)
        .bind(app)
        .bind(env)
        .fetch_one(&mut *tx)
        .await?;
        if deployed > 0 {
            return Err(StoreError::Conflict(format!(
                "environment `{app}/{env}` has deployments; deleting deployed environments is \
                 not supported yet"
            )));
        }
        for table in [
            "app_values",
            "value_set_versions",
            "deltas",
            "secret_grants",
            "envs",
        ] {
            let column = if table == "envs" { "id" } else { "env_id" };
            sqlx::query(&format!(
                "DELETE FROM {table} WHERE org_id = ? AND app_id = ? AND {column} = ?"
            ))
            .bind(org)
            .bind(app)
            .bind(env)
            .execute(&mut *tx)
            .await?;
        }
        audit::insert(&mut tx, &audit, None, None).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn delete_env_type(&self, org: &str, id: &str, audit: AuditIntent) -> Result<()> {
        self.check_org(org)?;
        let mut tx = self.begin().await?;
        check_admin_authority(&mut tx, org, audit.actor.as_deref()).await?;
        let exists: Option<i64> =
            sqlx::query_scalar("SELECT 1 FROM env_types WHERE org_id = ? AND id = ?")
                .bind(org)
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        if exists.is_none() {
            return Err(StoreError::NotFound(format!("environment type `{id}`")));
        }
        let users: Vec<String> = sqlx::query_scalar(
            "SELECT app_id || '/' || id FROM envs WHERE org_id = ? AND type = ? ORDER BY app_id, id LIMIT 5",
        )
        .bind(org)
        .bind(id)
        .fetch_all(&mut *tx)
        .await?;
        if !users.is_empty() {
            return Err(StoreError::Conflict(format!(
                "environment type `{id}` is used by {}",
                users.join(", ")
            )));
        }
        sqlx::query("DELETE FROM env_type_roles WHERE org_id = ? AND env_type_id = ?")
            .bind(org)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM env_types WHERE org_id = ? AND id = ?")
            .bind(org)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        audit::insert(&mut tx, &audit, None, None).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn latest_deployment(
        &self,
        org: &str,
        app: &str,
        env: &str,
    ) -> Result<Option<DeploymentRecord>> {
        self.check_org(org)?;
        let row: Option<DeploymentRow> = sqlx::query_as(&format!(
            "SELECT {DEPLOYMENT_COLUMNS} FROM deployments \
             WHERE org_id = ? AND app_id = ? AND env_id = ? ORDER BY seq DESC LIMIT 1"
        ))
        .bind(org)
        .bind(app)
        .bind(env)
        .fetch_optional(&self.pool)
        .await?;
        row.map(DeploymentRow::into_record).transpose()
    }

    async fn deployments(
        &self,
        org: &str,
        app: &str,
        env: &str,
        limit: i64,
        before_seq: Option<i64>,
    ) -> Result<Vec<DeploymentRecord>> {
        self.check_org(org)?;
        let rows: Vec<DeploymentRow> = sqlx::query_as(&format!(
            "SELECT {DEPLOYMENT_COLUMNS} FROM deployments \
             WHERE org_id = ? AND app_id = ? AND env_id = ? AND kind = 'deploy' AND seq < ? \
             ORDER BY seq DESC LIMIT ?"
        ))
        .bind(org)
        .bind(app)
        .bind(env)
        .bind(before_seq.unwrap_or(i64::MAX))
        .bind(limit.max(0))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(DeploymentRow::into_record).collect()
    }

    async fn idempotent_deployment(
        &self,
        org: &str,
        app: &str,
        env: &str,
        actor: &str,
        key: &str,
    ) -> Result<Option<(DeploymentRecord, String)>> {
        self.check_org(org)?;
        let mut conn = self.pool.acquire().await?;
        let prior: Option<(String, Option<String>)> = sqlx::query_as(
            "SELECT id, request_fingerprint FROM deployments WHERE org_id = ? AND app_id = ? \
             AND env_id = ? AND actor_id = ? AND idempotency_key = ?",
        )
        .bind(org)
        .bind(app)
        .bind(env)
        .bind(actor)
        .bind(key)
        .fetch_optional(&mut *conn)
        .await?;
        let Some((id, fingerprint)) = prior else {
            return Ok(None);
        };
        let d = fetch_deployment(&mut conn, org, app, env, &id)
            .await?
            .ok_or_else(|| StoreError::Corrupt(format!("deployment `{id}`")))?;
        Ok(Some((d, fingerprint.unwrap_or_default())))
    }

    async fn deployment_snapshot(
        &self,
        org: &str,
        app: &str,
        env: &str,
        id: &str,
    ) -> Result<Option<serde_json::Value>> {
        self.check_org(org)?;
        let snapshot: Option<Option<String>> = sqlx::query_scalar(
            "SELECT snapshot FROM deployments \
             WHERE org_id = ? AND app_id = ? AND env_id = ? AND id = ?",
        )
        .bind(org)
        .bind(app)
        .bind(env)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        snapshot
            .flatten()
            .map(|s| {
                serde_json::from_str(&s)
                    .map_err(|e| StoreError::Corrupt(format!("snapshot of deployment `{id}`: {e}")))
            })
            .transpose()
    }

    async fn app_deployment(
        &self,
        org: &str,
        app: &str,
        id: &str,
    ) -> Result<Option<DeploymentRecord>> {
        self.check_org(org)?;
        let row: Option<DeploymentRow> = sqlx::query_as(&format!(
            "SELECT {DEPLOYMENT_COLUMNS} FROM deployments WHERE org_id = ? AND app_id = ? AND id = ?"
        ))
        .bind(org)
        .bind(app)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(DeploymentRow::into_record).transpose()
    }

    async fn delta(&self, org: &str, app: &str, id: &str) -> Result<Option<DeltaRecord>> {
        self.check_org(org)?;
        let row: Option<DeltaRow> = sqlx::query_as(
            "SELECT id, env_id, content, frozen_at, created_by FROM deltas \
                 WHERE org_id = ? AND app_id = ? AND id = ?",
        )
        .bind(org)
        .bind(app)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(delta_record).transpose()
    }

    async fn deltas(&self, org: &str, app: &str) -> Result<Vec<DeltaRecord>> {
        self.check_org(org)?;
        let rows: Vec<DeltaRow> = sqlx::query_as(
            "SELECT id, env_id, content, frozen_at, created_by FROM deltas \
                 WHERE org_id = ? AND app_id = ? ORDER BY created_at DESC, id",
        )
        .bind(org)
        .bind(app)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(delta_record).collect()
    }

    async fn set(&self, org: &str, app: &str, id: &str) -> Result<Option<DeploymentSet>> {
        self.check_org(org)?;
        let content: Option<String> = sqlx::query_scalar(
            "SELECT content FROM sets WHERE org_id = ? AND app_id = ? AND id = ?",
        )
        .bind(org)
        .bind(app)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        content
            .map(|c| {
                serde_json::from_str(&c)
                    .map_err(|e| StoreError::Corrupt(format!("set `{id}`: {e}")))
            })
            .transpose()
    }

    async fn env(&self, org: &str, app: &str, env: &str) -> Result<Option<EnvRecord>> {
        self.check_org(org)?;
        let row: Option<EnvRow> = sqlx::query_as(&format!(
            "SELECT {ENV_COLUMNS} FROM envs WHERE org_id = ? AND app_id = ? AND id = ?"
        ))
        .bind(org)
        .bind(app)
        .bind(env)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(EnvRow::into_record))
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

    async fn put_set_audited(
        &self,
        org: &str,
        app: &str,
        set: &DeploymentSet,
        audit: AuditIntent,
    ) -> Result<String> {
        self.check_org(org)?;
        let id = set.id();
        let mut tx = self.begin().await?;
        check_configure_authority(&mut tx, org, audit.actor.as_deref(), app).await?;
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
        .execute(&mut *tx)
        .await?;
        audit::insert(&mut tx, &audit, None, None).await?;
        tx.commit().await?;
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
        check_configure_authority(&mut tx, org, audit.actor.as_deref(), app).await?;
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

        let row: Option<(i64, String, String)> = sqlx::query_as(
            "SELECT generation, state, type FROM envs WHERE org_id = ? AND app_id = ? AND id = ?",
        )
        .bind(org)
        .bind(app)
        .bind(env)
        .fetch_optional(&mut *tx)
        .await?;
        let (generation, state, env_type) =
            row.ok_or_else(|| StoreError::NotFound(format!("environment `{app}/{env}`")))?;
        // The route checked the caller's roles; they may have changed since.
        crate::identity::check_env_authority(
            &mut tx,
            org,
            req.audit.actor.as_deref(),
            app,
            &env_type,
        )
        .await?;
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
        if let (Some(delta), Some(expected)) = (&req.delta_id, &req.delta_content) {
            let stored: Option<String> = sqlx::query_scalar(
                "SELECT content FROM deltas WHERE org_id = ? AND app_id = ? AND id = ?",
            )
            .bind(org)
            .bind(app)
            .bind(delta)
            .fetch_optional(&mut *tx)
            .await?;
            let stored = stored.ok_or_else(|| StoreError::NotFound(format!("delta `{delta}`")))?;
            let stored: serde_json::Value = serde_json::from_str(&stored)
                .map_err(|e| StoreError::Corrupt(format!("delta `{delta}`: {e}")))?;
            if &stored != expected {
                return Err(StoreError::Conflict(format!(
                    "delta `{delta}` changed while the deployment was being prepared"
                )));
            }
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
        let existing: Option<(String, String, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT guresid, status, driver_type, last_deploy_id FROM active_resources \
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
            Some((_, status, _, _)) if status == "unknown" || status == "recovery_required" => {
                return Err(StoreError::RecoveryRequired(descriptor));
            }
            // A different driver cannot take over a resource another driver
            // provisioned: that is a migration, which only an operator starts.
            Some((_, _, Some(current), Some(_)))
                if start.kind == OperationKind::Put
                    && start.driver_type.as_ref().is_some_and(|d| *d != current) =>
            {
                return Err(StoreError::MigrationRequired {
                    descriptor,
                    from: current,
                    to: start.driver_type.clone().unwrap_or_default(),
                });
            }
            Some((guresid, _, _, _)) => {
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
                    // The definition columns describe the last success; a
                    // resource that never provisioned shows its latest attempt.
                    "UPDATE active_resources SET status = ?1, \
                     definition_id = CASE WHEN last_deploy_id IS NULL \
                     THEN COALESCE(?2, definition_id) ELSE definition_id END, \
                     definition_version = CASE WHEN last_deploy_id IS NULL \
                     THEN COALESCE(?3, definition_version) ELSE definition_version END, \
                     driver_type = CASE WHEN last_deploy_id IS NULL \
                     THEN COALESCE(?4, driver_type) ELSE driver_type END, \
                     recovery = ?5, updated_at = ?6 WHERE guresid = ?7",
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
             input_fingerprint, state, attempt, recovery, started_at, definition_id, \
             definition_version, driver_type) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'started', ?, ?, ?, ?, ?, ?)",
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
        .bind(&start.definition_id)
        .bind(start.definition_version)
        .bind(&start.driver_type)
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
            (CheckpointResult::CleanupPending { .. }, "delete") if state == "started" => {}
            (
                CheckpointResult::Provisioned(_)
                | CheckpointResult::Deleted
                | CheckpointResult::CleanupPending { .. },
                _,
            ) => {
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
                    inputs,
                    dependencies,
                    snapshot,
                    account,
                    target,
                } = &**p;
                sqlx::query(
                    "UPDATE active_resources SET status = 'provisioned', outputs = ?, \
                     secret_outputs = ?, cookie_key_id = ?, cookie_nonce = ?, cookie = ?, \
                     recovery = ?, inputs = ?, dependencies = ?, snapshot = ?, driver_account = ?, target = ?, \
                     definition_id = COALESCE(o.definition_id, active_resources.definition_id), \
                     definition_version = COALESCE(o.definition_version, \
                     active_resources.definition_version), \
                     driver_type = COALESCE(o.driver_type, active_resources.driver_type), \
                     last_deploy_id = ?, observed_at = ?, updated_at = ? \
                     FROM (SELECT definition_id, definition_version, driver_type FROM operations \
                     WHERE id = ?) AS o WHERE guresid = ?",
                )
                .bind(serde_json::to_string(outputs)?)
                .bind(serde_json::to_string(secret_outputs)?)
                .bind(cookie.as_ref().map(|c| c.key_id.clone()))
                .bind(cookie.as_ref().map(|c| c.nonce.clone()))
                .bind(cookie.as_ref().map(|c| c.ciphertext.clone()))
                .bind(serde_json::to_string(recovery)?)
                .bind(serde_json::to_string(inputs)?)
                .bind(serde_json::to_string(dependencies)?)
                .bind(serde_json::to_string(snapshot)?)
                .bind(account)
                .bind(target)
                .bind(&deployment_id)
                .bind(now)
                .bind(now)
                .bind(operation_id)
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
            CheckpointResult::CleanupPending { error } => {
                sqlx::query(
                    "UPDATE active_resources SET recovery = json_set(recovery, \
                     '$.cleanup_pending', json('true')) WHERE guresid = ?",
                )
                .bind(&guresid)
                .execute(&mut *tx)
                .await?;
                (
                    "failed",
                    Some("deletion_failed"),
                    "failed",
                    Some(format!("cleanup pending: {error}")),
                    None,
                )
            }
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

    async fn record_operation_inputs(
        &self,
        operation_id: &str,
        recovery: serde_json::Value,
    ) -> Result<()> {
        let org: Option<(String,)> = sqlx::query_as("SELECT org_id FROM operations WHERE id = ?")
            .bind(operation_id)
            .fetch_optional(&self.pool)
            .await?;
        let (org,) =
            org.ok_or_else(|| StoreError::NotFound(format!("operation `{operation_id}`")))?;
        self.check_org(&org)?;
        let updated =
            sqlx::query("UPDATE operations SET recovery = ? WHERE id = ? AND state = 'started'")
                .bind(recovery.to_string())
                .bind(operation_id)
                .execute(&self.pool)
                .await?;
        if updated.rows_affected() == 0 {
            return Err(StoreError::Conflict(format!(
                "operation `{operation_id}` is no longer started"
            )));
        }
        Ok(())
    }

    async fn record_resource_status(
        &self,
        org: &str,
        app: &str,
        env: &str,
        deployment_id: &str,
        descriptor: &str,
        status: ResourceRunStatus,
        error: Option<String>,
    ) -> Result<()> {
        self.check_org(org)?;
        // A resource that never ran has a GUResID only if an earlier deployment
        // provisioned it; otherwise the row's GUResID is empty. A status an
        // operation recorded is kept.
        sqlx::query(
            "INSERT INTO deployment_resources (org_id, app_id, env_id, deployment_id, descriptor, \
             guresid, status, error, observed_at) VALUES (?1, ?2, ?3, ?4, ?5, \
             COALESCE((SELECT guresid FROM active_resources WHERE org_id = ?1 AND app_id = ?2 \
             AND env_id = ?3 AND descriptor = ?5), ''), ?6, ?7, ?8) \
             ON CONFLICT (org_id, app_id, env_id, deployment_id, descriptor) \
             DO UPDATE SET status = excluded.status, error = excluded.error, \
             observed_at = excluded.observed_at \
             WHERE deployment_resources.status IN ('pending', 'skipped')",
        )
        .bind(org)
        .bind(app)
        .bind(env)
        .bind(deployment_id)
        .bind(descriptor)
        .bind(status.as_str())
        .bind(error)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
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
        let row: Option<ResourceRow> = sqlx::query_as(&format!(
            "{RESOURCE_COLUMNS} WHERE org_id = ? AND app_id = ? AND env_id = ? AND descriptor = ?"
        ))
        .bind(org)
        .bind(app)
        .bind(env)
        .bind(descriptor)
        .fetch_optional(&self.pool)
        .await?;
        row.map(ResourceRow::into_resource).transpose()
    }

    async fn active_resources(
        &self,
        org: &str,
        app: &str,
        env: &str,
    ) -> Result<Vec<ActiveResource>> {
        self.check_org(org)?;
        let rows: Vec<ResourceRow> = sqlx::query_as(&format!(
            "{RESOURCE_COLUMNS} WHERE org_id = ? AND app_id = ? AND env_id = ? ORDER BY descriptor"
        ))
        .bind(org)
        .bind(app)
        .bind(env)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(ResourceRow::into_resource).collect()
    }

    async fn definition_resources(
        &self,
        org: &str,
        definition: &str,
    ) -> Result<Vec<ActiveResource>> {
        self.check_org(org)?;
        let rows: Vec<ResourceRow> = sqlx::query_as(&format!(
            "{RESOURCE_COLUMNS} WHERE org_id = ? AND definition_id = ? \
             ORDER BY app_id, env_id, descriptor"
        ))
        .bind(org)
        .bind(definition)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(ResourceRow::into_resource).collect()
    }

    async fn schedule_deletion(
        &self,
        org: &str,
        app: &str,
        env: &str,
        descriptor: &str,
        audit: AuditIntent,
    ) -> Result<()> {
        self.check_org(org)?;
        let mut tx = self.begin().await?;
        let env_type: Option<String> =
            sqlx::query_scalar("SELECT type FROM envs WHERE org_id = ? AND app_id = ? AND id = ?")
                .bind(org)
                .bind(app)
                .bind(env)
                .fetch_optional(&mut *tx)
                .await?;
        let env_type =
            env_type.ok_or_else(|| StoreError::NotFound(format!("environment `{app}/{env}`")))?;
        // The route checked the caller's roles; they may have changed since.
        check_env_authority(&mut tx, org, audit.actor.as_deref(), app, &env_type).await?;
        let row: Option<(String, bool)> = sqlx::query_as(
            "SELECT ownership, scheduled_deletion FROM active_resources \
             WHERE org_id = ? AND app_id = ? AND env_id = ? AND descriptor = ?",
        )
        .bind(org)
        .bind(app)
        .bind(env)
        .bind(descriptor)
        .fetch_optional(&mut *tx)
        .await?;
        let scheduled = row.as_ref().is_some_and(|(_, s)| *s);
        match row.as_ref().map(|(o, _)| o.as_str()) {
            None => {
                return Err(StoreError::NotFound(format!(
                    "active resource `{descriptor}`"
                )));
            }
            Some("external") => {
                return Err(StoreError::Conflict(format!(
                    "`{descriptor}` is external; openporch never deletes it"
                )));
            }
            Some(_) => {}
        }
        // Scheduling again changes nothing, so a retried request does not
        // advance the generation.
        if scheduled {
            audit::insert(&mut tx, &audit, None, None).await?;
            tx.commit().await?;
            return Ok(());
        }
        // A running deployment was accepted without this change; it must not
        // act on it.
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
        // What the next deployment does changes, so plans and permits made
        // against the current generation no longer apply.
        sqlx::query("UPDATE envs SET generation = generation + 1 WHERE org_id = ? AND app_id = ? AND id = ?")
            .bind(org)
            .bind(app)
            .bind(env)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE active_resources SET scheduled_deletion = 1, updated_at = ? \
             WHERE org_id = ? AND app_id = ? AND env_id = ? AND descriptor = ?",
        )
        .bind(now_ms())
        .bind(org)
        .bind(app)
        .bind(env)
        .bind(descriptor)
        .execute(&mut *tx)
        .await?;
        audit::insert(&mut tx, &audit, None, None).await?;
        tx.commit().await?;
        Ok(())
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
        let now = now_ms();
        // Removing the record of a deletion the driver already confirmed has a
        // known outcome: the record stays, still marked for cleanup.
        let cleanups: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT o.id, o.deployment_id, o.guresid FROM operations o \
             JOIN active_resources r ON r.guresid = o.guresid \
             WHERE o.org_id = ? AND o.state = 'started' AND o.kind = 'delete' \
             AND COALESCE(json_extract(r.recovery, '$.cleanup_pending'), 0) = 1",
        )
        .bind(&self.org)
        .fetch_all(&mut *tx)
        .await?;
        for (op, deployment, guresid) in &cleanups {
            let error = "interrupted by a restart; cleanup pending";
            sqlx::query(
                "UPDATE operations SET state = 'failed', error = ?, finished_at = ? WHERE id = ?",
            )
            .bind(error)
            .bind(now)
            .bind(op)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "UPDATE active_resources SET status = 'deletion_failed', updated_at = ? \
                 WHERE guresid = ?",
            )
            .bind(now)
            .bind(guresid)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "UPDATE deployment_resources SET status = 'failed', error = ? \
                 WHERE deployment_id = ? AND guresid = ?",
            )
            .bind(error)
            .bind(deployment)
            .bind(guresid)
            .execute(&mut *tx)
            .await?;
        }
        let uncertain: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT id, deployment_id, guresid FROM operations WHERE org_id = ? AND state = 'started' \
             ORDER BY started_at",
        )
        .bind(&self.org)
        .fetch_all(&mut *tx)
        .await?;
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

const RESOURCE_COLUMNS: &str = "SELECT app_id, env_id, descriptor, guresid, definition_id, \
     definition_version, driver_type, driver_account, target, status, inputs, outputs, secret_outputs, cookie_key_id, \
     cookie_nonce, cookie, snapshot, ownership, dependencies, last_deploy_id, scheduled_deletion, \
     COALESCE(json_extract(recovery, '$.cleanup_pending'), 0) AS cleanup_pending, \
     observed_at, created_at FROM active_resources";

/// An `active_resources` row.
#[derive(sqlx::FromRow)]
struct ResourceRow {
    app_id: String,
    env_id: String,
    descriptor: String,
    guresid: String,
    definition_id: Option<String>,
    definition_version: Option<i64>,
    driver_type: Option<String>,
    driver_account: Option<String>,
    target: Option<String>,
    status: String,
    inputs: String,
    outputs: String,
    secret_outputs: String,
    cookie_key_id: Option<String>,
    cookie_nonce: Option<Vec<u8>>,
    cookie: Option<Vec<u8>>,
    snapshot: String,
    ownership: String,
    dependencies: String,
    last_deploy_id: Option<String>,
    scheduled_deletion: bool,
    cleanup_pending: bool,
    observed_at: Option<i64>,
    created_at: i64,
}

impl ResourceRow {
    fn into_resource(self) -> Result<ActiveResource> {
        let cookie = match (self.cookie_key_id, self.cookie_nonce, self.cookie) {
            (Some(key_id), Some(nonce), Some(ciphertext)) => Some(Sealed {
                key_id,
                nonce,
                ciphertext,
            }),
            _ => None,
        };
        Ok(ActiveResource {
            app: self.app_id,
            env: self.env_id,
            descriptor: self.descriptor,
            guresid: self.guresid,
            definition_id: self.definition_id,
            definition_version: self.definition_version,
            driver_type: self.driver_type,
            driver_account: self.driver_account,
            target: self.target,
            status: self.status,
            inputs: serde_json::from_str(&self.inputs)?,
            outputs: serde_json::from_str(&self.outputs)?,
            secret_outputs: serde_json::from_str(&self.secret_outputs)?,
            cookie,
            snapshot: serde_json::from_str(&self.snapshot)?,
            ownership: self.ownership,
            dependencies: serde_json::from_str(&self.dependencies)?,
            last_deploy_id: self.last_deploy_id,
            scheduled_deletion: self.scheduled_deletion,
            cleanup_pending: self.cleanup_pending,
            observed_at: self.observed_at,
            created_at: self.created_at,
        })
    }
}

#[cfg(test)]
mod tests;
