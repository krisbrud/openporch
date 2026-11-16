//! OpenID Connect login providers managed through the API. Their settings are
//! checked by the server; the store keeps them, their client secret's built-in
//! reference, and a generation that rises with every change, so a server can tell
//! a provider it built from an older row.

use std::future::Future;

use openporch_core::model::SecretRef;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::audit::{self, AuditIntent};
use crate::identity::check_admin_authority;
use crate::sqlite::{SqliteStore, unique_violation};
use crate::{Result, StoreError, now_ms};

/// A stored login provider.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LoginProviderRecord {
    /// The provider ID, used in `/auth/oidc/{id}/...`.
    pub id: String,
    /// The provider's settings, without the client secret.
    pub config: Map<String, Value>,
    /// The client secret in the built-in store, for a confidential client.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<SecretRef>,
    /// Raised by every change.
    #[serde(skip)]
    pub generation: i64,
    /// Creation time.
    pub created_at: i64,
}

/// Login providers.
pub trait LoginProviderStore: Send + Sync {
    /// Every provider, by ID.
    fn login_providers(
        &self,
        org: &str,
    ) -> impl Future<Output = Result<Vec<LoginProviderRecord>>> + Send;

    /// One provider.
    fn login_provider(
        &self,
        org: &str,
        id: &str,
    ) -> impl Future<Output = Result<Option<LoginProviderRecord>>> + Send;

    /// Creates a provider; an administrator's action.
    fn create_login_provider(
        &self,
        org: &str,
        provider: &LoginProviderRecord,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Replaces a provider's settings and client secret, raising its generation.
    fn update_login_provider(
        &self,
        org: &str,
        provider: &LoginProviderRecord,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Deletes a provider.
    fn delete_login_provider(
        &self,
        org: &str,
        id: &str,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<()>> + Send;
}

type Row = (String, String, Option<String>, i64, i64);

fn from_row((id, config, secret, generation, created_at): Row) -> Result<LoginProviderRecord> {
    Ok(LoginProviderRecord {
        id,
        config: serde_json::from_str(&config)?,
        client_secret: secret.map(|s| serde_json::from_str(&s)).transpose()?,
        generation,
        created_at,
    })
}

fn secret_json(provider: &LoginProviderRecord) -> Result<Option<String>> {
    Ok(provider
        .client_secret
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?)
}

impl LoginProviderStore for SqliteStore {
    async fn login_providers(&self, org: &str) -> Result<Vec<LoginProviderRecord>> {
        self.check_org(org)?;
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT id, config, client_secret, generation, created_at FROM login_providers \
             WHERE org_id = ? ORDER BY id",
        )
        .bind(org)
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(from_row).collect()
    }

    async fn login_provider(&self, org: &str, id: &str) -> Result<Option<LoginProviderRecord>> {
        self.check_org(org)?;
        let row: Option<Row> = sqlx::query_as(
            "SELECT id, config, client_secret, generation, created_at FROM login_providers \
             WHERE org_id = ? AND id = ?",
        )
        .bind(org)
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        row.map(from_row).transpose()
    }

    async fn create_login_provider(
        &self,
        org: &str,
        provider: &LoginProviderRecord,
        audit: AuditIntent,
    ) -> Result<()> {
        self.check_org(org)?;
        let mut tx = self.begin().await?;
        check_admin_authority(&mut tx, org, audit.actor.as_deref()).await?;
        let res = sqlx::query(
            "INSERT INTO login_providers \
             (org_id, id, config, client_secret, generation, created_at, created_by) \
             VALUES (?, ?, ?, ?, 0, ?, ?)",
        )
        .bind(org)
        .bind(&provider.id)
        .bind(serde_json::to_string(&provider.config)?)
        .bind(secret_json(provider)?)
        .bind(now_ms())
        .bind(&audit.actor)
        .execute(&mut *tx)
        .await;
        if let Err(e) = &res
            && unique_violation(e)
        {
            return Err(StoreError::AlreadyExists(format!(
                "login provider `{}`",
                provider.id
            )));
        }
        res?;
        audit::insert(&mut tx, &audit, None, None).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn update_login_provider(
        &self,
        org: &str,
        provider: &LoginProviderRecord,
        audit: AuditIntent,
    ) -> Result<()> {
        self.check_org(org)?;
        let mut tx = self.begin().await?;
        check_admin_authority(&mut tx, org, audit.actor.as_deref()).await?;
        let updated = sqlx::query(
            "UPDATE login_providers SET config = ?, client_secret = ?, generation = generation + 1 \
             WHERE org_id = ? AND id = ?",
        )
        .bind(serde_json::to_string(&provider.config)?)
        .bind(secret_json(provider)?)
        .bind(org)
        .bind(&provider.id)
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!(
                "login provider `{}`",
                provider.id
            )));
        }
        audit::insert(&mut tx, &audit, None, None).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn delete_login_provider(&self, org: &str, id: &str, audit: AuditIntent) -> Result<()> {
        self.check_org(org)?;
        let mut tx = self.begin().await?;
        check_admin_authority(&mut tx, org, audit.actor.as_deref()).await?;
        let deleted = sqlx::query("DELETE FROM login_providers WHERE org_id = ? AND id = ?")
            .bind(org)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        if deleted.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("login provider `{id}`")));
        }
        audit::insert(&mut tx, &audit, None, None).await?;
        tx.commit().await?;
        Ok(())
    }
}
