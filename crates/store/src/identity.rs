//! Principals, API tokens, sessions, invitations and role assignments.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use sqlx::SqliteConnection;

use crate::audit::{self, AuditIntent};
use crate::{Result, StoreError, now_ms};

/// An organization role.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, serde::Deserialize,
)]
pub enum OrgRole {
    /// Every permission.
    #[serde(rename = "administrator")]
    Administrator,
    /// Invites and removes non-administrators and creates applications.
    #[serde(rename = "manager")]
    Manager,
    /// Sees only applications where they hold a role.
    #[serde(rename = "member")]
    Member,
    /// Reads everything.
    #[serde(rename = "orgViewer")]
    OrgViewer,
}

impl OrgRole {
    /// The role's ID.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Administrator => "administrator",
            Self::Manager => "manager",
            Self::Member => "member",
            Self::OrgViewer => "orgViewer",
        }
    }

    /// Parses a role ID.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "administrator" => Self::Administrator,
            "manager" => Self::Manager,
            "member" => Self::Member,
            "orgViewer" => Self::OrgViewer,
            _ => return None,
        })
    }
}

/// An application role, ordered from weakest to strongest.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum AppRole {
    /// Reads the application.
    Viewer,
    /// Also edits sets, deltas, values and environments.
    Developer,
    /// Also manages application users and deletes the application.
    Owner,
}

impl AppRole {
    /// The role's ID.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
            Self::Developer => "developer",
            Self::Owner => "owner",
        }
    }

    /// Parses a role ID.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "viewer" => Self::Viewer,
            "developer" => Self::Developer,
            "owner" => Self::Owner,
            _ => return None,
        })
    }
}

/// A user or service user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Principal {
    /// ID.
    pub id: String,
    /// `user` or `service`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Display name.
    pub name: String,
    /// Email, for users.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Organization role.
    pub role: OrgRole,
    /// The linked external identity, such as `github:583231`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    /// Creation time.
    pub created_at: i64,
    /// The creating principal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
}

/// A new principal.
#[derive(Debug, Clone)]
pub struct NewPrincipal {
    /// ID.
    pub id: String,
    /// `user` or `service`.
    pub kind: String,
    /// Display name.
    pub name: String,
    /// Email.
    pub email: Option<String>,
    /// Organization role.
    pub role: OrgRole,
}

/// Token metadata. The token value is never stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TokenInfo {
    /// Token ID, unique per principal.
    pub id: String,
    /// Description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Expiry.
    pub expires_at: Option<i64>,
    /// Creation time.
    pub created_at: i64,
    /// The issuing principal.
    pub created_by: String,
    /// Last successful use.
    pub last_used_at: Option<i64>,
    /// Revocation time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<i64>,
}

/// A principal's roles at every level.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct Roles {
    /// Organization role.
    pub org: Option<OrgRole>,
    /// Application roles by application ID.
    pub apps: BTreeMap<String, AppRole>,
    /// Environment types with the `deployer` role.
    pub deployer: BTreeSet<String>,
}

/// An invitation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Invitation {
    /// ID.
    pub id: String,
    /// The role granted on acceptance.
    pub role: OrgRole,
    /// The invited email.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// The identity provider of an identity invitation, such as `github`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The immutable subject, such as a GitHub numeric user ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// The login name at issuance, for display.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub login: Option<String>,
    /// Expiry.
    pub expires_at: i64,
    /// Creation time.
    pub created_at: i64,
    /// The inviting principal.
    pub created_by: String,
    /// When it was accepted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub consumed_at: Option<i64>,
}

/// A new invitation. `code_hash` is the SHA-256 of the one-time code.
#[derive(Debug, Clone)]
pub struct NewInvitation {
    /// ID.
    pub id: String,
    /// Role.
    pub role: OrgRole,
    /// Email.
    pub email: Option<String>,
    /// Provider.
    pub provider: Option<String>,
    /// Subject.
    pub subject: Option<String>,
    /// Login name.
    pub login: Option<String>,
    /// Hash of the one-time code.
    pub code_hash: String,
    /// Expiry.
    pub expires_at: i64,
}

/// A verified external identity presented at login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalIdentity {
    /// Provider ID, such as `github`.
    pub provider: String,
    /// The immutable subject.
    pub subject: String,
    /// The current login name, for display.
    pub login: Option<String>,
    /// A provider-verified email address.
    pub verified_email: Option<String>,
    /// A display name.
    pub name: String,
}

pub(crate) const PRINCIPAL_COLUMNS: &str = "p.id, p.type, p.name, p.email, p.org_role, \
     (SELECT i.provider || ':' || i.subject FROM identities i \
      WHERE i.org_id = p.org_id AND i.principal_id = p.id), p.created_at, p.created_by";

type PrincipalRow = (
    String,
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    i64,
    Option<String>,
);

pub(crate) fn principal_from_row(r: PrincipalRow) -> Result<Principal> {
    Ok(Principal {
        role: OrgRole::parse(&r.4).ok_or_else(|| StoreError::Corrupt(format!("role `{}`", r.4)))?,
        id: r.0,
        kind: r.1,
        name: r.2,
        email: r.3,
        identity: r.5,
        created_at: r.6,
        created_by: r.7,
    })
}

pub(crate) async fn fetch_principal(
    conn: &mut SqliteConnection,
    org: &str,
    id: &str,
) -> Result<Option<Principal>> {
    let row: Option<PrincipalRow> = sqlx::query_as(&format!(
        "SELECT {PRINCIPAL_COLUMNS} FROM principals p WHERE p.org_id = ? AND p.id = ?"
    ))
    .bind(org)
    .bind(id)
    .fetch_optional(conn)
    .await?;
    row.map(principal_from_row).transpose()
}

pub(crate) async fn insert_principal(
    conn: &mut SqliteConnection,
    org: &str,
    p: &NewPrincipal,
    created_by: Option<&str>,
) -> Result<()> {
    let res = sqlx::query(
        "INSERT INTO principals (org_id, id, type, name, email, org_role, created_at, created_by) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(org)
    .bind(&p.id)
    .bind(&p.kind)
    .bind(&p.name)
    .bind(&p.email)
    .bind(p.role.as_str())
    .bind(now_ms())
    .bind(created_by)
    .execute(conn)
    .await;
    match res {
        Err(sqlx::Error::Database(db)) if db.is_unique_violation() => {
            Err(StoreError::AlreadyExists(format!("user `{}`", p.id)))
        }
        other => other.map(|_| ()).map_err(Into::into),
    }
}

/// Fails with a conflict if the change would leave no administrator.
pub(crate) async fn guard_last_admin(
    conn: &mut SqliteConnection,
    org: &str,
    id: &str,
) -> Result<()> {
    let others: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM principals WHERE org_id = ? AND org_role = 'administrator' AND id <> ?",
    )
    .bind(org)
    .bind(id)
    .fetch_one(conn)
    .await?;
    if others == 0 {
        Err(StoreError::Conflict(
            "the organization must keep at least one administrator".into(),
        ))
    } else {
        Ok(())
    }
}

pub(crate) async fn audit_in(conn: &mut SqliteConnection, intent: &AuditIntent) -> Result<()> {
    audit::insert(conn, intent, None, None).await.map(|_| ())
}

/// Filters for audit queries.
#[derive(Debug, Clone, Default)]
pub struct AuditFilter {
    /// Entries at or after this time.
    pub from: Option<i64>,
    /// Entries before this time.
    pub to: Option<i64>,
    /// Entries by this principal.
    pub user_id: Option<String>,
    /// Entries whose path starts with this prefix.
    pub path_prefix: Option<String>,
    /// Entries with this method.
    pub method: Option<String>,
    /// Entries older than this cursor (an entry sequence number).
    pub before: Option<i64>,
    /// Page size.
    pub limit: u32,
}

/// A principal's roles at every level, read on `conn`; empty for non-members.
pub(crate) async fn roles_in(
    conn: &mut SqliteConnection,
    org: &str,
    principal: &str,
) -> Result<Roles> {
    let Some(p) = fetch_principal(conn, org, principal).await? else {
        return Ok(Roles::default());
    };
    let apps: Vec<(String, String)> =
        sqlx::query_as("SELECT app_id, role FROM app_roles WHERE org_id = ? AND principal_id = ?")
            .bind(org)
            .bind(principal)
            .fetch_all(&mut *conn)
            .await?;
    let deployer: Vec<String> = sqlx::query_scalar(
        "SELECT env_type_id FROM env_type_roles WHERE org_id = ? AND principal_id = ?",
    )
    .bind(org)
    .bind(principal)
    .fetch_all(&mut *conn)
    .await?;
    Ok(Roles {
        org: Some(p.role),
        apps: apps
            .into_iter()
            .filter_map(|(a, r)| AppRole::parse(&r).map(|r| (a, r)))
            .collect(),
        deployer: deployer.into_iter().collect(),
    })
}

/// Whether an organization role may be held by someone acting on behalf of `issuer`.
fn org_role_covers(issuer: OrgRole, target: OrgRole) -> bool {
    issuer == OrgRole::Administrator || target == OrgRole::Member || issuer == target
}

/// Whether `issuer` holds at least every permission `target` holds, so that acting for
/// `target` (issuing it a token, for example) cannot increase `issuer`'s authority.
pub fn covers(issuer: &Roles, target: &Roles) -> bool {
    if issuer.org == Some(OrgRole::Administrator) {
        return true;
    }
    let (Some(i), Some(t)) = (issuer.org, target.org) else {
        return target.org.is_none() && target.apps.is_empty() && target.deployer.is_empty();
    };
    org_role_covers(i, t)
        && target
            .apps
            .iter()
            .all(|(app, role)| issuer.apps.get(app).is_some_and(|r| r >= role))
        && target.deployer.is_subset(&issuer.deployer)
}

/// The delegation ceiling, checked inside the transaction that makes the change:
/// administrators may do anything; managers may act only on non-administrators whose
/// current and resulting roles they cover; nobody else may. Changes without an actor
/// (made by the server itself) are not limited.
pub(crate) async fn check_authority(
    conn: &mut SqliteConnection,
    org: &str,
    actor: Option<&str>,
    target: Option<&str>,
    resulting: Option<OrgRole>,
) -> Result<()> {
    let Some(actor) = actor else {
        return Ok(());
    };
    let issuer = roles_in(conn, org, actor).await?;
    if issuer.org == Some(OrgRole::Administrator) {
        return Ok(());
    }
    let denied = || {
        StoreError::Forbidden(format!(
            "`{actor}` does not hold every permission this change would grant or affect"
        ))
    };
    if issuer.org != Some(OrgRole::Manager) {
        return Err(denied());
    }
    let current = match target {
        Some(t) => roles_in(conn, org, t).await?,
        None => Roles::default(),
    };
    if current.org == Some(OrgRole::Administrator) || !covers(&issuer, &current) {
        return Err(denied());
    }
    if let Some(role) = resulting {
        let after = Roles {
            org: Some(role),
            ..current
        };
        if role == OrgRole::Administrator || !covers(&issuer, &after) {
            return Err(denied());
        }
    }
    Ok(())
}
