//! Shared values of applications and environments, and their value set versions.
//!
//! A value holds either plain text or a secret reference. Every change records a
//! new value set version of the changed scope (the application, or one
//! environment), holding the scope's values with secrets as references.

use std::future::Future;

use openporch_core::model::SecretRef;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sqlx::SqliteConnection;

use crate::audit::{self, AuditIntent};
use crate::identity::check_values_authority;
use crate::secrets::BUILTIN_STORE;
use crate::sqlite::SqliteStore;
use crate::sqlite_catalog::check_ref_stores;
use crate::{Result, StoreError, now_ms};

/// What a value holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValueContent {
    /// Plain text.
    Value(String),
    /// A secret, as a reference. Its value is never stored here.
    SecretRef(SecretRef),
}

/// A shared value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ValueRecord {
    /// The key, referenced as `${values.KEY}`.
    pub key: String,
    /// A description.
    pub description: String,
    /// Plain text or a secret reference.
    #[serde(flatten)]
    pub content: ValueContent,
    /// Whether the value is secret.
    pub is_secret: bool,
    /// Last change.
    pub updated_at: i64,
}

/// A value to create or replace.
#[derive(Debug, Clone)]
pub struct PutValue {
    /// The key.
    pub key: String,
    /// A description.
    pub description: String,
    /// Plain text or a secret reference.
    pub content: ValueContent,
    /// Create a new value (`409` if it exists) instead of replacing one (`404` if
    /// it does not).
    pub create: bool,
}

/// A value set version: the values of one scope after a change.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ValueSetVersion {
    /// Version ID, increasing within its scope.
    pub id: String,
    /// The scope's values by key, with secrets as references.
    pub values: Value,
    /// Creation time.
    pub created_at: i64,
    /// The changing principal.
    pub created_by: Option<String>,
}

/// Shared values. `env` is `None` for application values.
pub trait ValueStore: Send + Sync {
    /// The values of a scope, by key.
    fn values(
        &self,
        org: &str,
        app: &str,
        env: Option<&str>,
    ) -> impl Future<Output = Result<Vec<ValueRecord>>> + Send;

    /// One value.
    fn value(
        &self,
        org: &str,
        app: &str,
        env: Option<&str>,
        key: &str,
    ) -> impl Future<Output = Result<Option<ValueRecord>>> + Send;

    /// Creates or replaces a value and records a value set version. An environment
    /// value needs an application value with the same key. A secret reference must
    /// be one written for this scope or its application, or be covered by a grant.
    fn put_value(
        &self,
        org: &str,
        app: &str,
        env: Option<&str>,
        value: PutValue,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Deletes a value and records a value set version. An application value that
    /// environments override cannot be deleted.
    fn delete_value(
        &self,
        org: &str,
        app: &str,
        env: Option<&str>,
        key: &str,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<()>> + Send;

    /// The value set versions of a scope, newest first.
    fn value_versions(
        &self,
        org: &str,
        app: &str,
        env: Option<&str>,
    ) -> impl Future<Output = Result<Vec<ValueSetVersion>>> + Send;
}

/// The prefix of built-in secret names written for values of a scope.
pub fn value_secret_prefix(app: &str, env: Option<&str>) -> String {
    match env {
        None => format!("apps/{app}/values/"),
        Some(env) => format!("apps/{app}/envs/{env}/values/"),
    }
}

type ValueRow = (String, String, Option<String>, Option<String>, i64);

fn record((key, description, value, secret_ref, updated_at): ValueRow) -> Result<ValueRecord> {
    let content = match (value, secret_ref) {
        (Some(v), None) => ValueContent::Value(v),
        (None, Some(r)) => ValueContent::SecretRef(
            serde_json::from_str(&r)
                .map_err(|e| StoreError::Corrupt(format!("value `{key}`: {e}")))?,
        ),
        _ => return Err(StoreError::Corrupt(format!("value `{key}` has no content"))),
    };
    Ok(ValueRecord {
        is_secret: matches!(content, ValueContent::SecretRef(_)),
        key,
        description,
        content,
        updated_at,
    })
}

async fn scope_values(
    conn: &mut SqliteConnection,
    org: &str,
    app: &str,
    env: &str,
) -> Result<Vec<ValueRecord>> {
    let rows: Vec<ValueRow> = sqlx::query_as(
        "SELECT key, description, value, secret_ref, updated_at FROM app_values \
         WHERE org_id = ? AND app_id = ? AND env_id = ? ORDER BY key",
    )
    .bind(org)
    .bind(app)
    .bind(env)
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter().map(record).collect()
}

async fn check_scope(conn: &mut SqliteConnection, org: &str, app: &str, env: &str) -> Result<()> {
    let found: Option<i64> = if env.is_empty() {
        sqlx::query_scalar("SELECT 1 FROM apps WHERE org_id = ? AND id = ?")
            .bind(org)
            .bind(app)
            .fetch_optional(&mut *conn)
            .await?
    } else {
        sqlx::query_scalar("SELECT 1 FROM envs WHERE org_id = ? AND app_id = ? AND id = ?")
            .bind(org)
            .bind(app)
            .bind(env)
            .fetch_optional(&mut *conn)
            .await?
    };
    match found {
        Some(_) => Ok(()),
        None if env.is_empty() => Err(StoreError::NotFound(format!("application `{app}`"))),
        None => Err(StoreError::NotFound(format!("environment `{app}/{env}`"))),
    }
}

/// Whether `app` (and `env`, when set) may use `r`: it was written for this
/// scope's values or its application's, or a grant covers it. A grant for one
/// environment never covers application values.
pub(crate) async fn check_secret_use(
    conn: &mut SqliteConnection,
    org: &str,
    app: &str,
    env: Option<&str>,
    r: &SecretRef,
) -> Result<()> {
    let mut refs = Map::new();
    refs.insert("ref".into(), serde_json::to_value(r)?);
    check_ref_stores(conn, org, &refs).await?;
    if r.store == BUILTIN_STORE {
        let mut prefixes = vec![value_secret_prefix(app, None)];
        if env.is_some() {
            prefixes.push(value_secret_prefix(app, env));
        }
        if prefixes.iter().any(|p| r.reference.starts_with(p.as_str())) {
            return Ok(());
        }
    }
    let granted: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM secret_grants WHERE org_id = ? AND store_id = ? AND ref = ? \
         AND (org_wide = 1 OR (app_id = ? AND (env_id IS NULL OR env_id = ?)))",
    )
    .bind(org)
    .bind(&r.store)
    .bind(&r.reference)
    .bind(app)
    .bind(env)
    .fetch_optional(&mut *conn)
    .await?;
    match granted {
        Some(_) => Ok(()),
        None => Err(StoreError::Forbidden(format!(
            "no grant lets `{}` use secret `{}` in store `{}`",
            match env {
                Some(env) => format!("{app}/{env}"),
                None => app.to_owned(),
            },
            r.reference,
            r.store
        ))),
    }
}

/// Records the scope's values as a new version.
async fn record_version(
    conn: &mut SqliteConnection,
    org: &str,
    app: &str,
    env: &str,
    actor: Option<&str>,
) -> Result<()> {
    let mut values = Map::new();
    for v in scope_values(conn, org, app, env).await? {
        let mut entry = json!({"description": v.description});
        match v.content {
            ValueContent::Value(text) => entry["value"] = Value::String(text),
            ValueContent::SecretRef(r) => entry["secret_ref"] = serde_json::to_value(r)?,
        }
        values.insert(v.key, entry);
    }
    let latest: Option<i64> = sqlx::query_scalar(
        "SELECT MAX(CAST(id AS INTEGER)) FROM value_set_versions \
         WHERE org_id = ? AND app_id = ? AND env_id = ?",
    )
    .bind(org)
    .bind(app)
    .bind(env)
    .fetch_one(&mut *conn)
    .await?;
    sqlx::query(
        "INSERT INTO value_set_versions (org_id, app_id, env_id, id, content, created_at, \
         created_by) VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(org)
    .bind(app)
    .bind(env)
    .bind((latest.unwrap_or(0) + 1).to_string())
    .bind(Value::Object(values).to_string())
    .bind(now_ms())
    .bind(actor)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

async fn value_row(
    conn: &mut SqliteConnection,
    org: &str,
    app: &str,
    env: &str,
    key: &str,
) -> Result<Option<ValueRecord>> {
    let row: Option<ValueRow> = sqlx::query_as(
        "SELECT key, description, value, secret_ref, updated_at FROM app_values \
         WHERE org_id = ? AND app_id = ? AND env_id = ? AND key = ?",
    )
    .bind(org)
    .bind(app)
    .bind(env)
    .bind(key)
    .fetch_optional(&mut *conn)
    .await?;
    row.map(record).transpose()
}

fn scope_name(app: &str, env: &str) -> String {
    if env.is_empty() {
        format!("application `{app}`")
    } else {
        format!("environment `{app}/{env}`")
    }
}

impl ValueStore for SqliteStore {
    async fn values(&self, org: &str, app: &str, env: Option<&str>) -> Result<Vec<ValueRecord>> {
        self.check_org(org)?;
        let mut conn = self.pool().acquire().await?;
        scope_values(&mut conn, org, app, env.unwrap_or_default()).await
    }

    async fn value(
        &self,
        org: &str,
        app: &str,
        env: Option<&str>,
        key: &str,
    ) -> Result<Option<ValueRecord>> {
        self.check_org(org)?;
        let mut conn = self.pool().acquire().await?;
        value_row(&mut conn, org, app, env.unwrap_or_default(), key).await
    }

    async fn put_value(
        &self,
        org: &str,
        app: &str,
        env: Option<&str>,
        value: PutValue,
        audit: AuditIntent,
    ) -> Result<()> {
        self.check_org(org)?;
        let scope = env.unwrap_or_default();
        let key = value.key.as_str();
        let mut tx = self.begin().await?;
        check_scope(&mut tx, org, app, scope).await?;
        check_values_authority(&mut tx, org, audit.actor.as_deref(), app, env).await?;
        let existing = value_row(&mut tx, org, app, scope, key).await?;
        match (existing.is_some(), value.create) {
            (true, true) => {
                return Err(StoreError::AlreadyExists(format!(
                    "value `{key}` in {}",
                    scope_name(app, scope)
                )));
            }
            (false, false) => {
                return Err(StoreError::NotFound(format!(
                    "value `{key}` in {}",
                    scope_name(app, scope)
                )));
            }
            _ => {}
        }
        if env.is_some() && value_row(&mut tx, org, app, "", key).await?.is_none() {
            return Err(StoreError::Invalid(format!(
                "environment value `{key}` overrides an application value, but `{app}` has \
                 no value `{key}`"
            )));
        }
        let (text, secret_ref) = match &value.content {
            ValueContent::Value(v) => (Some(v.clone()), None),
            ValueContent::SecretRef(r) => {
                check_secret_use(&mut tx, org, app, env, r).await?;
                (None, Some(serde_json::to_string(r)?))
            }
        };
        sqlx::query(
            "INSERT INTO app_values (org_id, app_id, env_id, key, description, value, \
             secret_ref, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT (org_id, app_id, env_id, key) DO UPDATE SET \
             description = excluded.description, value = excluded.value, \
             secret_ref = excluded.secret_ref, updated_at = excluded.updated_at",
        )
        .bind(org)
        .bind(app)
        .bind(scope)
        .bind(key)
        .bind(&value.description)
        .bind(text)
        .bind(secret_ref)
        .bind(now_ms())
        .execute(&mut *tx)
        .await?;
        record_version(&mut tx, org, app, scope, audit.actor.as_deref()).await?;
        audit::insert(&mut tx, &audit, None, None).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn delete_value(
        &self,
        org: &str,
        app: &str,
        env: Option<&str>,
        key: &str,
        audit: AuditIntent,
    ) -> Result<()> {
        self.check_org(org)?;
        let scope = env.unwrap_or_default();
        let mut tx = self.begin().await?;
        check_values_authority(&mut tx, org, audit.actor.as_deref(), app, env).await?;
        if value_row(&mut tx, org, app, scope, key).await?.is_none() {
            return Err(StoreError::NotFound(format!(
                "value `{key}` in {}",
                scope_name(app, scope)
            )));
        }
        if env.is_none() {
            let overriding: Vec<String> = sqlx::query_scalar(
                "SELECT env_id FROM app_values WHERE org_id = ? AND app_id = ? AND key = ? \
                 AND env_id <> '' ORDER BY env_id",
            )
            .bind(org)
            .bind(app)
            .bind(key)
            .fetch_all(&mut *tx)
            .await?;
            if !overriding.is_empty() {
                return Err(StoreError::Conflict(format!(
                    "value `{key}` is overridden in environments {}; delete the overrides first",
                    overriding
                        .iter()
                        .map(|e| format!("`{e}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
        }
        sqlx::query(
            "DELETE FROM app_values WHERE org_id = ? AND app_id = ? AND env_id = ? AND key = ?",
        )
        .bind(org)
        .bind(app)
        .bind(scope)
        .bind(key)
        .execute(&mut *tx)
        .await?;
        record_version(&mut tx, org, app, scope, audit.actor.as_deref()).await?;
        audit::insert(&mut tx, &audit, None, None).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn value_versions(
        &self,
        org: &str,
        app: &str,
        env: Option<&str>,
    ) -> Result<Vec<ValueSetVersion>> {
        self.check_org(org)?;
        let rows: Vec<(String, String, i64, Option<String>)> = sqlx::query_as(
            "SELECT id, content, created_at, created_by FROM value_set_versions \
             WHERE org_id = ? AND app_id = ? AND env_id = ? ORDER BY seq DESC",
        )
        .bind(org)
        .bind(app)
        .bind(env.unwrap_or_default())
        .fetch_all(self.pool())
        .await?;
        rows.into_iter()
            .map(|(id, content, created_at, created_by)| {
                Ok(ValueSetVersion {
                    values: serde_json::from_str(&content).map_err(|e| {
                        StoreError::Corrupt(format!("value set version `{id}`: {e}"))
                    })?,
                    id,
                    created_at,
                    created_by,
                })
            })
            .collect()
    }
}
