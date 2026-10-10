use std::future::Future;

use serde::Serialize;

use crate::audit::{AuditEntry, AuditIntent};
use crate::identity::*;
use crate::secrets::Sealed;
use crate::sqlite::SqliteStore;
use crate::{Result, StoreError, now_ms};

/// An application as listed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AppRecord {
    /// ID.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Creation time.
    pub created_at: i64,
    /// The creating principal.
    pub created_by: Option<String>,
}

/// An environment type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EnvTypeRecord {
    /// ID.
    pub id: String,
    /// Description.
    pub description: String,
    /// Creation time.
    pub created_at: i64,
}

/// The identity, membership, role and audit side of the store, used by the API
/// server's authentication and authorization.
pub trait IdentityStore: Send + Sync {
    /// Creates a principal.
    fn create_principal(
        &self,
        org: &str,
        principal: NewPrincipal,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<Principal>> + Send;

    /// Creates the `bootstrap` administrator service user with its token in one
    /// transaction. `audit` has no actor: the server itself does this on first start.
    fn create_bootstrap_admin(
        &self,
        org: &str,
        token_hash: &str,
        expires_at: i64,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<Principal>> + Send;

    /// Reads a principal.
    fn principal(
        &self,
        org: &str,
        id: &str,
    ) -> impl Future<Output = Result<Option<Principal>>> + Send;

    /// Lists principals by ID.
    fn principals(&self, org: &str) -> impl Future<Output = Result<Vec<Principal>>> + Send;

    /// Changes a principal's organization role, keeping at least one administrator.
    fn set_org_role(
        &self,
        org: &str,
        id: &str,
        role: OrgRole,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<Principal>> + Send;

    /// Removes a principal with their identities, tokens, sessions and roles, keeping
    /// at least one administrator.
    fn remove_principal(
        &self,
        org: &str,
        id: &str,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<()>> + Send;

    /// A principal's roles at every level.
    fn roles(&self, org: &str, principal: &str) -> impl Future<Output = Result<Roles>> + Send;

    /// Stores a token's hash.
    #[allow(clippy::too_many_arguments)]
    fn create_token(
        &self,
        org: &str,
        principal: &str,
        id: &str,
        description: Option<&str>,
        expires_at: Option<i64>,
        token_hash: &str,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<TokenInfo>> + Send;

    /// Lists a principal's tokens.
    fn tokens(
        &self,
        org: &str,
        principal: &str,
    ) -> impl Future<Output = Result<Vec<TokenInfo>>> + Send;

    /// Revokes a token.
    fn revoke_token(
        &self,
        org: &str,
        principal: &str,
        id: &str,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Resolves an unexpired, unrevoked token hash to its principal and records the use.
    fn authenticate_token(
        &self,
        org: &str,
        token_hash: &str,
        now: i64,
    ) -> impl Future<Output = Result<Option<Principal>>> + Send;

    /// Records an issued session with its audit entry.
    fn record_session(
        &self,
        org: &str,
        jti: &str,
        principal: &str,
        expires_at: i64,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Resolves an unexpired, unrevoked session of a current member.
    fn authenticate_session(
        &self,
        org: &str,
        jti: &str,
        principal: &str,
        now: i64,
    ) -> impl Future<Output = Result<Option<Principal>>> + Send;

    /// Revokes a session.
    fn revoke_session(
        &self,
        org: &str,
        jti: &str,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Returns the sealed signing key for `purpose`, storing `generated` first if
    /// there is none.
    fn signing_key(
        &self,
        purpose: &str,
        algorithm: &str,
        generated: Sealed,
    ) -> impl Future<Output = Result<Sealed>> + Send;

    /// Creates an invitation.
    fn create_invitation(
        &self,
        org: &str,
        invitation: NewInvitation,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<Invitation>> + Send;

    /// Lists invitations, newest first.
    fn invitations(&self, org: &str) -> impl Future<Output = Result<Vec<Invitation>>> + Send;

    /// Finds the principal linked to an external identity.
    fn principal_by_identity(
        &self,
        org: &str,
        provider: &str,
        subject: &str,
    ) -> impl Future<Output = Result<Option<Principal>>> + Send;

    /// Accepts an invitation: the code must match an unexpired, unconsumed invitation
    /// for this identity (its provider subject, or its provider-verified email). Creates
    /// the user, links the identity and consumes the invitation atomically.
    fn accept_invitation(
        &self,
        org: &str,
        code_hash: &str,
        identity: &ExternalIdentity,
        user_id: &str,
        now: i64,
    ) -> impl Future<Output = Result<Principal>> + Send;

    /// Lists applications.
    fn apps(&self, org: &str) -> impl Future<Output = Result<Vec<AppRecord>>> + Send;

    /// Reads an application.
    fn app(&self, org: &str, id: &str) -> impl Future<Output = Result<Option<AppRecord>>> + Send;

    /// The environment types the application's environments use.
    fn app_env_types(
        &self,
        org: &str,
        app: &str,
    ) -> impl Future<Output = Result<Vec<String>>> + Send;

    /// Deletes an application that has never been deployed, with its environments,
    /// sets, deltas, values and roles. Applications with deployments or active
    /// resources are rejected until deprovisioning on delete exists.
    fn delete_app(
        &self,
        org: &str,
        app: &str,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Lists environment types.
    fn env_types(&self, org: &str) -> impl Future<Output = Result<Vec<EnvTypeRecord>>> + Send;

    /// Reads an environment type.
    fn env_type(
        &self,
        org: &str,
        id: &str,
    ) -> impl Future<Output = Result<Option<EnvTypeRecord>>> + Send;

    /// Sets a principal's role on an application.
    fn set_app_role(
        &self,
        org: &str,
        app: &str,
        principal: &str,
        role: AppRole,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Removes a principal's role on an application.
    fn remove_app_role(
        &self,
        org: &str,
        app: &str,
        principal: &str,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Lists an application's role assignments.
    fn app_roles(
        &self,
        org: &str,
        app: &str,
    ) -> impl Future<Output = Result<Vec<(String, AppRole)>>> + Send;

    /// Grants `deployer` on an environment type.
    fn set_env_type_role(
        &self,
        org: &str,
        env_type: &str,
        principal: &str,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Removes `deployer` on an environment type.
    fn remove_env_type_role(
        &self,
        org: &str,
        env_type: &str,
        principal: &str,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Lists the principals with `deployer` on an environment type.
    fn env_type_roles(
        &self,
        org: &str,
        env_type: &str,
    ) -> impl Future<Output = Result<Vec<String>>> + Send;

    /// Queries the audit log, newest first.
    fn audit_query(
        &self,
        org: &str,
        filter: &AuditFilter,
    ) -> impl Future<Output = Result<Vec<AuditEntry>>> + Send;
}

fn not_found(what: String) -> StoreError {
    StoreError::NotFound(what)
}

async fn require_principal(
    conn: &mut sqlx::SqliteConnection,
    org: &str,
    id: &str,
) -> Result<Principal> {
    fetch_principal(conn, org, id)
        .await?
        .ok_or_else(|| not_found(format!("user `{id}`")))
}

type TokenRow = (
    String,
    Option<String>,
    Option<i64>,
    i64,
    String,
    Option<i64>,
    Option<i64>,
);

fn token_from_row(r: TokenRow) -> TokenInfo {
    TokenInfo {
        id: r.0,
        description: r.1,
        expires_at: r.2,
        created_at: r.3,
        created_by: r.4,
        last_used_at: r.5,
        revoked_at: r.6,
    }
}

type InvitationRow = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    i64,
    i64,
    String,
    Option<i64>,
);

const INVITATION_COLUMNS: &str = "id, org_role, email, provider, subject, login, expires_at, created_at, created_by, consumed_at";

fn invitation_from_row(r: InvitationRow) -> Result<Invitation> {
    Ok(Invitation {
        id: r.0,
        role: OrgRole::parse(&r.1).ok_or_else(|| StoreError::Corrupt(format!("role `{}`", r.1)))?,
        email: r.2,
        provider: r.3,
        subject: r.4,
        login: r.5,
        expires_at: r.6,
        created_at: r.7,
        created_by: r.8,
        consumed_at: r.9,
    })
}

impl IdentityStore for SqliteStore {
    async fn create_principal(
        &self,
        org: &str,
        principal: NewPrincipal,
        audit: AuditIntent,
    ) -> Result<Principal> {
        self.check_org(org)?;
        let mut tx = self.begin().await?;
        check_authority(
            &mut tx,
            org,
            audit.actor.as_deref(),
            None,
            Some(principal.role),
        )
        .await?;
        insert_principal(&mut tx, org, &principal, audit.actor.as_deref()).await?;
        audit_in(&mut tx, &audit).await?;
        let p = require_principal(&mut tx, org, &principal.id).await?;
        tx.commit().await?;
        Ok(p)
    }

    async fn create_bootstrap_admin(
        &self,
        org: &str,
        token_hash: &str,
        expires_at: i64,
        audit: AuditIntent,
    ) -> Result<Principal> {
        self.check_org(org)?;
        let mut tx = self.begin().await?;
        let admin = NewPrincipal {
            id: "bootstrap".into(),
            kind: "service".into(),
            name: "Bootstrap administrator".into(),
            email: None,
            role: OrgRole::Administrator,
        };
        insert_principal(&mut tx, org, &admin, None).await?;
        sqlx::query(
            "INSERT INTO api_tokens (org_id, principal_id, id, description, token_hash, expires_at, \
             created_at, created_by) VALUES (?, 'bootstrap', 'bootstrap', 'Created on first start', \
             ?, ?, ?, 'system')",
        )
        .bind(org)
        .bind(token_hash)
        .bind(expires_at)
        .bind(now_ms())
        .execute(&mut *tx)
        .await?;
        audit_in(&mut tx, &audit).await?;
        let p = require_principal(&mut tx, org, "bootstrap").await?;
        tx.commit().await?;
        Ok(p)
    }

    async fn principal(&self, org: &str, id: &str) -> Result<Option<Principal>> {
        self.check_org(org)?;
        let mut conn = self.pool().acquire().await?;
        fetch_principal(&mut conn, org, id).await
    }

    async fn principals(&self, org: &str) -> Result<Vec<Principal>> {
        self.check_org(org)?;
        let rows: Vec<_> = sqlx::query_as(&format!(
            "SELECT {PRINCIPAL_COLUMNS} FROM principals p WHERE p.org_id = ? ORDER BY p.id"
        ))
        .bind(org)
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(principal_from_row).collect()
    }

    async fn set_org_role(
        &self,
        org: &str,
        id: &str,
        role: OrgRole,
        audit: AuditIntent,
    ) -> Result<Principal> {
        self.check_org(org)?;
        let mut tx = self.begin().await?;
        let current = require_principal(&mut tx, org, id).await?;
        check_authority(&mut tx, org, audit.actor.as_deref(), Some(id), Some(role)).await?;
        if current.role == OrgRole::Administrator && role != OrgRole::Administrator {
            guard_last_admin(&mut tx, org, id).await?;
        }
        sqlx::query("UPDATE principals SET org_role = ? WHERE org_id = ? AND id = ?")
            .bind(role.as_str())
            .bind(org)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        audit_in(&mut tx, &audit).await?;
        let p = require_principal(&mut tx, org, id).await?;
        tx.commit().await?;
        Ok(p)
    }

    async fn remove_principal(&self, org: &str, id: &str, audit: AuditIntent) -> Result<()> {
        self.check_org(org)?;
        let mut tx = self.begin().await?;
        let current = require_principal(&mut tx, org, id).await?;
        check_removal_authority(&mut tx, org, audit.actor.as_deref(), id).await?;
        if current.role == OrgRole::Administrator {
            guard_last_admin(&mut tx, org, id).await?;
        }
        // Tokens, sessions, identities and roles cascade.
        sqlx::query("DELETE FROM principals WHERE org_id = ? AND id = ?")
            .bind(org)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        audit_in(&mut tx, &audit).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn roles(&self, org: &str, principal: &str) -> Result<Roles> {
        self.check_org(org)?;
        let mut conn = self.pool().acquire().await?;
        roles_in(&mut conn, org, principal).await
    }

    async fn create_token(
        &self,
        org: &str,
        principal: &str,
        id: &str,
        description: Option<&str>,
        expires_at: Option<i64>,
        token_hash: &str,
        audit: AuditIntent,
    ) -> Result<TokenInfo> {
        self.check_org(org)?;
        // Tokens issued by the server itself, such as the bootstrap token, have no actor.
        let created_by = audit.actor.clone().unwrap_or_else(|| "system".to_owned());
        let mut tx = self.begin().await?;
        let target = require_principal(&mut tx, org, principal).await?;
        // Everyone may issue tokens for themselves; tokens of another human user never;
        // tokens of a service user within the delegation ceiling.
        match audit.actor.as_deref() {
            Some(actor) if actor == principal => {}
            Some(_) if target.kind != "service" => {
                return Err(StoreError::Forbidden(
                    "tokens of a user can only be created by that user".into(),
                ));
            }
            actor => check_authority(&mut tx, org, actor, Some(principal), None).await?,
        }
        let res = sqlx::query(
            "INSERT INTO api_tokens (org_id, principal_id, id, description, token_hash, expires_at, \
             created_at, created_by) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(org)
        .bind(principal)
        .bind(id)
        .bind(description)
        .bind(token_hash)
        .bind(expires_at)
        .bind(now_ms())
        .bind(&created_by)
        .execute(&mut *tx)
        .await;
        if let Err(sqlx::Error::Database(db)) = &res
            && db.is_unique_violation()
        {
            return Err(StoreError::AlreadyExists(format!("token `{id}`")));
        }
        res?;
        audit_in(&mut tx, &audit).await?;
        let row: TokenRow = sqlx::query_as(
            "SELECT id, description, expires_at, created_at, created_by, last_used_at, revoked_at \
             FROM api_tokens WHERE org_id = ? AND principal_id = ? AND id = ?",
        )
        .bind(org)
        .bind(principal)
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(token_from_row(row))
    }

    async fn tokens(&self, org: &str, principal: &str) -> Result<Vec<TokenInfo>> {
        self.check_org(org)?;
        let rows: Vec<TokenRow> = sqlx::query_as(
            "SELECT id, description, expires_at, created_at, created_by, last_used_at, revoked_at \
             FROM api_tokens WHERE org_id = ? AND principal_id = ? ORDER BY id",
        )
        .bind(org)
        .bind(principal)
        .fetch_all(self.pool())
        .await?;
        Ok(rows.into_iter().map(token_from_row).collect())
    }

    async fn revoke_token(
        &self,
        org: &str,
        principal: &str,
        id: &str,
        audit: AuditIntent,
    ) -> Result<()> {
        self.check_org(org)?;
        let mut tx = self.begin().await?;
        // Everyone may revoke their own tokens and administrators anyone's; managers
        // only those of service users within their delegation ceiling.
        if let Some(actor) = audit.actor.as_deref()
            && actor != principal
        {
            let target = require_principal(&mut tx, org, principal).await?;
            let issuer = roles_in(&mut tx, org, actor).await?;
            if issuer.org != Some(OrgRole::Administrator) {
                if target.kind != "service" {
                    return Err(StoreError::Forbidden(
                        "tokens of a user can only be revoked by that user or an administrator"
                            .into(),
                    ));
                }
                check_authority(&mut tx, org, Some(actor), Some(principal), None).await?;
            }
        }
        let n = sqlx::query(
            "UPDATE api_tokens SET revoked_at = COALESCE(revoked_at, ?) \
             WHERE org_id = ? AND principal_id = ? AND id = ?",
        )
        .bind(now_ms())
        .bind(org)
        .bind(principal)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        if n.rows_affected() == 0 {
            return Err(not_found(format!("token `{id}`")));
        }
        audit_in(&mut tx, &audit).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn authenticate_token(
        &self,
        org: &str,
        token_hash: &str,
        now: i64,
    ) -> Result<Option<Principal>> {
        self.check_org(org)?;
        let mut conn = self.pool().acquire().await?;
        let principal: Option<String> = sqlx::query_scalar(
            "SELECT principal_id FROM api_tokens WHERE org_id = ? AND token_hash = ? \
             AND revoked_at IS NULL AND (expires_at IS NULL OR expires_at > ?)",
        )
        .bind(org)
        .bind(token_hash)
        .bind(now)
        .fetch_optional(&mut *conn)
        .await?;
        let Some(principal) = principal else {
            return Ok(None);
        };
        sqlx::query("UPDATE api_tokens SET last_used_at = ? WHERE org_id = ? AND token_hash = ?")
            .bind(now)
            .bind(org)
            .bind(token_hash)
            .execute(&mut *conn)
            .await?;
        fetch_principal(&mut conn, org, &principal).await
    }

    async fn record_session(
        &self,
        org: &str,
        jti: &str,
        principal: &str,
        expires_at: i64,
        audit: AuditIntent,
    ) -> Result<()> {
        self.check_org(org)?;
        let mut tx = self.begin().await?;
        sqlx::query(
            "INSERT INTO sessions (jti, org_id, principal_id, issued_at, expires_at) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(jti)
        .bind(org)
        .bind(principal)
        .bind(now_ms())
        .bind(expires_at)
        .execute(&mut *tx)
        .await?;
        audit_in(&mut tx, &audit).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn authenticate_session(
        &self,
        org: &str,
        jti: &str,
        principal: &str,
        now: i64,
    ) -> Result<Option<Principal>> {
        self.check_org(org)?;
        let mut conn = self.pool().acquire().await?;
        let ok: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM sessions WHERE jti = ? AND org_id = ? AND principal_id = ? \
             AND revoked_at IS NULL AND expires_at > ?",
        )
        .bind(jti)
        .bind(org)
        .bind(principal)
        .bind(now)
        .fetch_optional(&mut *conn)
        .await?;
        if ok.is_none() {
            return Ok(None);
        }
        fetch_principal(&mut conn, org, principal).await
    }

    async fn revoke_session(&self, org: &str, jti: &str, audit: AuditIntent) -> Result<()> {
        self.check_org(org)?;
        let mut tx = self.begin().await?;
        sqlx::query(
            "UPDATE sessions SET revoked_at = COALESCE(revoked_at, ?) WHERE org_id = ? AND jti = ?",
        )
        .bind(now_ms())
        .bind(org)
        .bind(jti)
        .execute(&mut *tx)
        .await?;
        audit_in(&mut tx, &audit).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn signing_key(
        &self,
        purpose: &str,
        algorithm: &str,
        generated: Sealed,
    ) -> Result<Sealed> {
        let mut tx = self.begin().await?;
        let existing: Option<(String, Vec<u8>, Vec<u8>)> = sqlx::query_as(
            "SELECT key_id, nonce, ciphertext FROM signing_keys WHERE purpose = ? \
             AND retired_at IS NULL ORDER BY created_at DESC LIMIT 1",
        )
        .bind(purpose)
        .fetch_optional(&mut *tx)
        .await?;
        let sealed = match existing {
            Some((key_id, nonce, ciphertext)) => Sealed {
                key_id,
                nonce,
                ciphertext,
            },
            None => {
                sqlx::query(
                    "INSERT INTO signing_keys (id, purpose, algorithm, key_id, nonce, ciphertext, \
                     created_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(crate::random_hex(16))
                .bind(purpose)
                .bind(algorithm)
                .bind(&generated.key_id)
                .bind(&generated.nonce)
                .bind(&generated.ciphertext)
                .bind(now_ms())
                .execute(&mut *tx)
                .await?;
                generated
            }
        };
        tx.commit().await?;
        Ok(sealed)
    }

    async fn create_invitation(
        &self,
        org: &str,
        invitation: NewInvitation,
        audit: AuditIntent,
    ) -> Result<Invitation> {
        self.check_org(org)?;
        let created_by = audit
            .actor
            .clone()
            .ok_or_else(|| StoreError::Conflict("invitations need an inviting principal".into()))?;
        let mut tx = self.begin().await?;
        check_authority(&mut tx, org, Some(&created_by), None, Some(invitation.role)).await?;
        sqlx::query(
            "INSERT INTO invitations (org_id, id, org_role, email, provider, subject, login, \
             code_hash, expires_at, created_at, created_by) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(org)
        .bind(&invitation.id)
        .bind(invitation.role.as_str())
        .bind(&invitation.email)
        .bind(&invitation.provider)
        .bind(&invitation.subject)
        .bind(&invitation.login)
        .bind(&invitation.code_hash)
        .bind(invitation.expires_at)
        .bind(now_ms())
        .bind(&created_by)
        .execute(&mut *tx)
        .await?;
        audit_in(&mut tx, &audit).await?;
        let row: InvitationRow = sqlx::query_as(&format!(
            "SELECT {INVITATION_COLUMNS} FROM invitations WHERE org_id = ? AND id = ?"
        ))
        .bind(org)
        .bind(&invitation.id)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        invitation_from_row(row)
    }

    async fn invitations(&self, org: &str) -> Result<Vec<Invitation>> {
        self.check_org(org)?;
        let rows: Vec<InvitationRow> = sqlx::query_as(&format!(
            "SELECT {INVITATION_COLUMNS} FROM invitations WHERE org_id = ? ORDER BY created_at DESC"
        ))
        .bind(org)
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(invitation_from_row).collect()
    }

    async fn principal_by_identity(
        &self,
        org: &str,
        provider: &str,
        subject: &str,
    ) -> Result<Option<Principal>> {
        self.check_org(org)?;
        let mut conn = self.pool().acquire().await?;
        let id: Option<String> = sqlx::query_scalar(
            "SELECT principal_id FROM identities WHERE org_id = ? AND provider = ? AND subject = ?",
        )
        .bind(org)
        .bind(provider)
        .bind(subject)
        .fetch_optional(&mut *conn)
        .await?;
        match id {
            Some(id) => fetch_principal(&mut conn, org, &id).await,
            None => Ok(None),
        }
    }

    async fn accept_invitation(
        &self,
        org: &str,
        code_hash: &str,
        identity: &ExternalIdentity,
        user_id: &str,
        now: i64,
    ) -> Result<Principal> {
        self.check_org(org)?;
        let mut tx = self.begin().await?;
        let found: Option<InvitationRow> = sqlx::query_as(&format!(
            "SELECT {INVITATION_COLUMNS} FROM invitations WHERE org_id = ? AND code_hash = ?"
        ))
        .bind(org)
        .bind(code_hash)
        .fetch_optional(&mut *tx)
        .await?;
        let inv = found
            .map(invitation_from_row)
            .transpose()?
            .ok_or_else(|| StoreError::Conflict("an invitation is required".into()))?;
        if inv.consumed_at.is_some() {
            return Err(StoreError::Conflict(
                "the invitation was already used".into(),
            ));
        }
        if inv.expires_at <= now {
            return Err(StoreError::Conflict("the invitation has expired".into()));
        }
        // The inviter must still hold the authority to grant this role.
        if check_authority(&mut tx, org, Some(&inv.created_by), None, Some(inv.role))
            .await
            .is_err()
        {
            return Err(StoreError::Conflict(
                "the invitation is no longer valid; ask for a new one".into(),
            ));
        }
        // The member's email: the invited address when the invitation is by email
        // (any verified address of the identity may match it), otherwise the primary
        // verified address.
        let (matches, email) = match (&inv.provider, &inv.subject, &inv.email) {
            (Some(p), Some(s), _) => (
                *p == identity.provider && *s == identity.subject,
                identity.verified_emails.first().cloned(),
            ),
            (_, _, Some(invited)) => match identity
                .verified_emails
                .iter()
                .find(|v| v.eq_ignore_ascii_case(invited))
            {
                Some(v) => (true, Some(v.clone())),
                None => (false, None),
            },
            _ => (false, None),
        };
        if !matches {
            return Err(StoreError::Conflict(
                "the invitation is for a different identity or an unverified email".into(),
            ));
        }
        let linked: Option<String> = sqlx::query_scalar(
            "SELECT principal_id FROM identities WHERE org_id = ? AND provider = ? AND subject = ?",
        )
        .bind(org)
        .bind(&identity.provider)
        .bind(&identity.subject)
        .fetch_optional(&mut *tx)
        .await?;
        if linked.is_some() {
            return Err(StoreError::Conflict(
                "this identity already belongs to a user".into(),
            ));
        }
        // An email already used by a member stays bound to that member's identity; a
        // second provider cannot claim it, only an administrator can rebind it.
        for email in &identity.verified_emails {
            let taken: Option<String> = sqlx::query_scalar(
                "SELECT id FROM principals WHERE org_id = ? AND lower(email) = lower(?)",
            )
            .bind(org)
            .bind(email)
            .fetch_optional(&mut *tx)
            .await?;
            if taken.is_some() {
                return Err(StoreError::Conflict(
                    "this email already belongs to a member linked to another identity; an \
                     administrator must change that binding"
                        .into(),
                ));
            }
        }
        insert_principal(
            &mut tx,
            org,
            &NewPrincipal {
                id: user_id.to_owned(),
                kind: "user".into(),
                name: identity.name.clone(),
                email: email.clone(),
                role: inv.role,
            },
            Some(&inv.created_by),
        )
        .await?;
        sqlx::query(
            "INSERT INTO identities (org_id, provider, subject, principal_id, login, email, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(org)
        .bind(&identity.provider)
        .bind(&identity.subject)
        .bind(user_id)
        .bind(&identity.login)
        .bind(&email)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE invitations SET consumed_at = ?, consumed_by = ? WHERE org_id = ? AND id = ?",
        )
        .bind(now)
        .bind(user_id)
        .bind(org)
        .bind(&inv.id)
        .execute(&mut *tx)
        .await?;
        let mut audit = AuditIntent::accepted(
            org,
            user_id,
            "POST",
            &format!("/orgs/{org}/invitations/{}", inv.id),
            "invitation.accept",
        );
        audit.target = Some(user_id.to_owned());
        audit_in(&mut tx, &audit).await?;
        let p = require_principal(&mut tx, org, user_id).await?;
        tx.commit().await?;
        Ok(p)
    }

    async fn apps(&self, org: &str) -> Result<Vec<AppRecord>> {
        self.check_org(org)?;
        let rows: Vec<(String, String, i64, Option<String>)> = sqlx::query_as(
            "SELECT id, name, created_at, created_by FROM apps WHERE org_id = ? ORDER BY id",
        )
        .bind(org)
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| AppRecord {
                id: r.0,
                name: r.1,
                created_at: r.2,
                created_by: r.3,
            })
            .collect())
    }

    async fn app(&self, org: &str, id: &str) -> Result<Option<AppRecord>> {
        Ok(self.apps(org).await?.into_iter().find(|a| a.id == id))
    }

    async fn app_env_types(&self, org: &str, app: &str) -> Result<Vec<String>> {
        self.check_org(org)?;
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT DISTINCT type FROM envs WHERE org_id = ? AND app_id = ? ORDER BY type",
        )
        .bind(org)
        .bind(app)
        .fetch_all(self.pool())
        .await?;
        Ok(rows.into_iter().map(|r| r.0).collect())
    }

    async fn delete_app(&self, org: &str, app: &str, audit: AuditIntent) -> Result<()> {
        self.check_org(org)?;
        let mut tx = self.begin().await?;
        let exists: Option<(i64,)> =
            sqlx::query_as("SELECT 1 FROM apps WHERE org_id = ? AND id = ?")
                .bind(org)
                .bind(app)
                .fetch_optional(&mut *tx)
                .await?;
        if exists.is_none() {
            return Err(not_found(format!("application `{app}`")));
        }
        // Owner plus deployer on every environment type the application uses,
        // checked with the deletion.
        if let Some(actor) = audit.actor.as_deref() {
            let roles = roles_in(&mut tx, org, actor).await?;
            if roles.org != Some(OrgRole::Administrator) {
                check_app_authority(&mut tx, org, Some(actor), app).await?;
                let types: Vec<String> = sqlx::query_scalar(
                    "SELECT DISTINCT type FROM envs WHERE org_id = ? AND app_id = ?",
                )
                .bind(org)
                .bind(app)
                .fetch_all(&mut *tx)
                .await?;
                if let Some(t) = types.iter().find(|t| !roles.deployer.contains(*t)) {
                    return Err(StoreError::Forbidden(format!(
                        "requires the `deployer` role on environment type `{t}`"
                    )));
                }
            }
        }
        let (deployed,): (i64,) = sqlx::query_as(
            "SELECT (SELECT COUNT(*) FROM deployments WHERE org_id = ?1 AND app_id = ?2) \
                  + (SELECT COUNT(*) FROM active_resources WHERE org_id = ?1 AND app_id = ?2)",
        )
        .bind(org)
        .bind(app)
        .fetch_one(&mut *tx)
        .await?;
        if deployed > 0 {
            return Err(StoreError::Conflict(format!(
                "application `{app}` has deployments; deleting deployed applications is not \
                 supported yet"
            )));
        }
        for table in [
            "app_values",
            "value_set_versions",
            "deltas",
            "sets",
            "secret_grants",
            "envs",
            "apps",
        ] {
            let column = if table == "apps" { "id" } else { "app_id" };
            sqlx::query(&format!(
                "DELETE FROM {table} WHERE org_id = ? AND {column} = ?"
            ))
            .bind(org)
            .bind(app)
            .execute(&mut *tx)
            .await?;
        }
        audit_in(&mut tx, &audit).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn env_types(&self, org: &str) -> Result<Vec<EnvTypeRecord>> {
        self.check_org(org)?;
        let rows: Vec<(String, String, i64)> = sqlx::query_as(
            "SELECT id, description, created_at FROM env_types WHERE org_id = ? ORDER BY id",
        )
        .bind(org)
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| EnvTypeRecord {
                id: r.0,
                description: r.1,
                created_at: r.2,
            })
            .collect())
    }

    async fn env_type(&self, org: &str, id: &str) -> Result<Option<EnvTypeRecord>> {
        Ok(self.env_types(org).await?.into_iter().find(|t| t.id == id))
    }

    async fn set_app_role(
        &self,
        org: &str,
        app: &str,
        principal: &str,
        role: AppRole,
        audit: AuditIntent,
    ) -> Result<()> {
        self.check_org(org)?;
        let mut tx = self.begin().await?;
        check_app_authority(&mut tx, org, audit.actor.as_deref(), app).await?;
        require_principal(&mut tx, org, principal).await?;
        sqlx::query(
            "INSERT INTO app_roles (org_id, app_id, principal_id, role) VALUES (?, ?, ?, ?) \
             ON CONFLICT (org_id, app_id, principal_id) DO UPDATE SET role = excluded.role",
        )
        .bind(org)
        .bind(app)
        .bind(principal)
        .bind(role.as_str())
        .execute(&mut *tx)
        .await?;
        audit_in(&mut tx, &audit).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn remove_app_role(
        &self,
        org: &str,
        app: &str,
        principal: &str,
        audit: AuditIntent,
    ) -> Result<()> {
        self.check_org(org)?;
        let mut tx = self.begin().await?;
        check_app_authority(&mut tx, org, audit.actor.as_deref(), app).await?;
        let n = sqlx::query(
            "DELETE FROM app_roles WHERE org_id = ? AND app_id = ? AND principal_id = ?",
        )
        .bind(org)
        .bind(app)
        .bind(principal)
        .execute(&mut *tx)
        .await?;
        if n.rows_affected() == 0 {
            return Err(not_found(format!("role of `{principal}` on `{app}`")));
        }
        audit_in(&mut tx, &audit).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn app_roles(&self, org: &str, app: &str) -> Result<Vec<(String, AppRole)>> {
        self.check_org(org)?;
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT principal_id, role FROM app_roles WHERE org_id = ? AND app_id = ? \
             ORDER BY principal_id",
        )
        .bind(org)
        .bind(app)
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(p, r)| AppRole::parse(&r).map(|r| (p, r)))
            .collect())
    }

    async fn set_env_type_role(
        &self,
        org: &str,
        env_type: &str,
        principal: &str,
        audit: AuditIntent,
    ) -> Result<()> {
        self.check_org(org)?;
        let mut tx = self.begin().await?;
        check_admin_authority(&mut tx, org, audit.actor.as_deref()).await?;
        require_principal(&mut tx, org, principal).await?;
        sqlx::query(
            "INSERT INTO env_type_roles (org_id, env_type_id, principal_id, role) \
             VALUES (?, ?, ?, 'deployer') ON CONFLICT DO NOTHING",
        )
        .bind(org)
        .bind(env_type)
        .bind(principal)
        .execute(&mut *tx)
        .await?;
        audit_in(&mut tx, &audit).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn remove_env_type_role(
        &self,
        org: &str,
        env_type: &str,
        principal: &str,
        audit: AuditIntent,
    ) -> Result<()> {
        self.check_org(org)?;
        let mut tx = self.begin().await?;
        check_admin_authority(&mut tx, org, audit.actor.as_deref()).await?;
        let n = sqlx::query(
            "DELETE FROM env_type_roles WHERE org_id = ? AND env_type_id = ? AND principal_id = ?",
        )
        .bind(org)
        .bind(env_type)
        .bind(principal)
        .execute(&mut *tx)
        .await?;
        if n.rows_affected() == 0 {
            return Err(not_found(format!("role of `{principal}` on `{env_type}`")));
        }
        audit_in(&mut tx, &audit).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn env_type_roles(&self, org: &str, env_type: &str) -> Result<Vec<String>> {
        self.check_org(org)?;
        Ok(sqlx::query_scalar(
            "SELECT principal_id FROM env_type_roles WHERE org_id = ? AND env_type_id = ? \
             ORDER BY principal_id",
        )
        .bind(org)
        .bind(env_type)
        .fetch_all(self.pool())
        .await?)
    }

    async fn audit_query(&self, org: &str, filter: &AuditFilter) -> Result<Vec<AuditEntry>> {
        self.check_org(org)?;
        // `path_prefix` matches literally: LIKE wildcards in it are escaped.
        let prefix = filter.path_prefix.as_ref().map(|p| {
            let escaped = p
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            format!("{escaped}%")
        });
        Ok(sqlx::query_as(
            "SELECT seq, id, at, actor_id, request_id, operation_id, method, path, action, target, \
             outcome, status_code, reason, correlates FROM audit_log WHERE org_id = ? \
             AND (? IS NULL OR at >= ?) AND (? IS NULL OR at < ?) AND (? IS NULL OR actor_id = ?) \
             AND (? IS NULL OR path LIKE ? ESCAPE '\\') AND (? IS NULL OR method = ?) \
             AND (? IS NULL OR seq < ?) ORDER BY seq DESC LIMIT ?",
        )
        .bind(org)
        .bind(filter.from)
        .bind(filter.from)
        .bind(filter.to)
        .bind(filter.to)
        .bind(&filter.user_id)
        .bind(&filter.user_id)
        .bind(&prefix)
        .bind(&prefix)
        .bind(&filter.method)
        .bind(&filter.method)
        .bind(filter.before)
        .bind(filter.before)
        .bind(i64::from(filter.limit.clamp(1, 1000)))
        .fetch_all(self.pool())
        .await?)
    }
}
