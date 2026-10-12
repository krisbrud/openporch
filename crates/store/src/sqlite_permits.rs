//! Previews of deployments and destructive-action permits.
//!
//! A permit is issued from a preview for one action on one resource. It expires an
//! hour after issue unless a deployment's acceptance binds it first, and it binds
//! only while the environment's generation and the resource's configuration are
//! still those of the preview. Once bound, it authorizes that action in that
//! deployment and its retries, as long as its issuer still holds the authority to
//! issue it.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;

use serde::Serialize;
use serde_json::Value;
use sqlx::SqliteConnection;

use crate::audit::{self, AuditIntent};
use crate::identity::{check_admin_authority, check_env_authority};
use crate::sqlite::SqliteStore;
use crate::{Result, StoreError, now_ms, random_hex};

/// How long a plan and an unbound permit stay valid, in milliseconds.
pub const PERMIT_TTL_MS: i64 = 60 * 60 * 1000;

/// The actions a permit can authorize.
const ACTIONS: [&str; 4] = ["delete", "detach", "replace", "unknown"];

/// What `bound_to` holds for a permit a detach used.
const DETACHED: &str = "detach";

/// The permits a deployment presents, and what its acceptance checks them
/// against.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PermitBinding {
    /// The permit IDs presented.
    pub presented: Vec<String>,
    /// The configuration digest of each resource the deployment changes or
    /// deletes, by descriptor, computed as the preview computes it.
    pub digests: BTreeMap<String, String>,
    /// The destructive actions known before the deployment runs, as
    /// `(descriptor, action)`: each needs a presented permit.
    pub required: Vec<(String, String)>,
}

/// A preview to store.
#[derive(Debug, Clone)]
pub struct NewPlan {
    /// Organization.
    pub org: String,
    /// Application.
    pub app: String,
    /// Environment.
    pub env: String,
    /// What the plan previews: `deploy`, `delete_env` or `detach`.
    pub kind: String,
    /// The environment generation the preview was made at.
    pub base_generation: i64,
    /// The previewed set, for a deployment.
    pub set_id: Option<String>,
    /// The preview.
    pub body: Value,
    /// The audit intent of the request; its actor made the plan.
    pub audit: AuditIntent,
}

/// A stored preview.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlanRecord {
    /// Plan ID.
    pub id: String,
    /// Application.
    pub app: String,
    /// Environment.
    pub env: String,
    /// Who made it.
    pub actor: String,
    /// `deploy`, `delete_env` or `detach`.
    pub kind: String,
    /// The environment generation it was made at.
    pub base_generation: i64,
    /// The previewed set.
    pub set_id: Option<String>,
    /// The preview.
    pub body: Value,
    /// When it was made.
    pub created_at: i64,
    /// When permits can no longer be issued from it.
    pub expires_at: i64,
}

/// A permit to issue from a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPermit {
    /// The resource.
    pub descriptor: String,
    /// Its identity, once it has one.
    pub guresid: Option<String>,
    /// `delete`, `detach`, `replace` or `unknown`.
    pub action: String,
    /// The driver plan it covers; any plan of the action when absent.
    pub plan_fingerprint: Option<String>,
    /// The resource's configuration digest in the plan.
    pub config_digest: String,
}

/// An issued permit. Its ID is what a deployment presents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PermitRecord {
    /// Permit ID.
    pub id: String,
    /// The plan it was issued from.
    pub plan: String,
    /// The resource.
    pub descriptor: String,
    /// Its identity.
    pub guresid: Option<String>,
    /// The action.
    pub action: String,
    /// The driver plan it covers.
    pub plan_fingerprint: Option<String>,
    /// The generation it is valid at.
    pub base_generation: i64,
    /// Who issued it.
    pub issuer: String,
    /// When it expires unless bound.
    pub expires_at: i64,
    /// The deployment it is bound to.
    pub bound_to: Option<String>,
}

/// A destructive action to authorize with a permit.
#[derive(Debug, Clone, Copy)]
pub struct PermitUse<'a> {
    /// Organization.
    pub org: &'a str,
    /// Application.
    pub app: &'a str,
    /// Environment.
    pub env: &'a str,
    /// The deployment the action runs in; `None` for a detach, which binds a
    /// presented permit itself.
    pub deployment: Option<&'a str>,
    /// The permits presented, for a detach.
    pub presented: &'a [String],
    /// The resource.
    pub descriptor: &'a str,
    /// Its identity.
    pub guresid: Option<&'a str>,
    /// The action.
    pub action: &'a str,
    /// The driver plan's fingerprint, when the action comes from one.
    pub fingerprint: Option<&'a str>,
}

/// A driver plan that needed a permit the deployment did not have.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApprovalRequest {
    /// The resource.
    pub descriptor: String,
    /// Its identity.
    pub guresid: String,
    /// The action the plan needs a permit for.
    pub action: String,
    /// The plan's fingerprint.
    pub fingerprint: String,
    /// The planned actions on named objects, as `(action, address)`.
    pub actions: Vec<(String, String)>,
    /// The deployment that found it.
    pub deployment_id: String,
}

/// Plans and permits.
pub trait PermitStore: Send + Sync {
    /// Stores a preview, which expires after [`PERMIT_TTL_MS`].
    fn put_plan(&self, plan: NewPlan) -> impl Future<Output = Result<PlanRecord>> + Send;

    /// A stored preview.
    fn plan(
        &self,
        org: &str,
        app: &str,
        env: &str,
        id: &str,
    ) -> impl Future<Output = Result<Option<PlanRecord>>> + Send;

    /// Issues permits from an unexpired plan, valid at its generation. The
    /// audit intent's actor is the issuer.
    fn issue_permits(
        &self,
        org: &str,
        app: &str,
        env: &str,
        plan: &str,
        permits: Vec<NewPermit>,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<Vec<PermitRecord>>> + Send;

    /// Authorizes a destructive action: a permit bound to the deployment, or
    /// for a detach a presented one, must cover it, and its issuer must still
    /// hold the authority that issued it. Marks the permit used.
    fn use_permit(&self, permit: PermitUse<'_>) -> impl Future<Output = Result<()>> + Send;

    /// Records a driver plan that needed a permit, replacing older ones for
    /// the resource.
    fn record_approval_request(
        &self,
        org: &str,
        app: &str,
        env: &str,
        request: &ApprovalRequest,
    ) -> impl Future<Output = Result<()>> + Send;

    /// The environment's recorded driver plans that needed a permit.
    fn approval_requests(
        &self,
        org: &str,
        app: &str,
        env: &str,
    ) -> impl Future<Output = Result<Vec<ApprovalRequest>>> + Send;
}

/// A permit as `use_permit` reads it: ID, issuer, GUResID, driver plan
/// fingerprint, base generation, expiry and binding.
type PermitRow = (
    String,
    String,
    Option<String>,
    Option<String>,
    i64,
    i64,
    Option<String>,
);

/// A permit as `bind_permits` reads it: descriptor, action, base
/// generation, configuration digest, expiry, binding and issuer.
type BindRow = (String, String, i64, String, i64, Option<String>, String);

type PlanRow = (
    String,
    String,
    String,
    String,
    String,
    i64,
    Option<String>,
    String,
    i64,
    i64,
);

fn plan_record(row: PlanRow) -> Result<PlanRecord> {
    let (id, app, env, actor, kind, base_generation, set_id, body, created_at, expires_at) = row;
    Ok(PlanRecord {
        id,
        app,
        env,
        actor,
        kind,
        base_generation,
        set_id,
        body: serde_json::from_str(&body)?,
        created_at,
        expires_at,
    })
}

impl PermitStore for SqliteStore {
    async fn put_plan(&self, plan: NewPlan) -> Result<PlanRecord> {
        self.check_org(&plan.org)?;
        let actor = plan
            .audit
            .actor
            .clone()
            .ok_or_else(|| StoreError::Forbidden("plans need an acting principal".into()))?;
        let now = now_ms();
        let record = PlanRecord {
            id: format!("pln-{}", random_hex(12)),
            app: plan.app.clone(),
            env: plan.env.clone(),
            actor,
            kind: plan.kind.clone(),
            base_generation: plan.base_generation,
            set_id: plan.set_id.clone(),
            body: plan.body.clone(),
            created_at: now,
            expires_at: now + PERMIT_TTL_MS,
        };
        let mut tx = self.begin().await?;
        sqlx::query(
            "INSERT INTO plans (org_id, app_id, env_id, id, actor_id, kind, base_generation, \
             set_id, body, created_at, expires_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&plan.org)
        .bind(&record.app)
        .bind(&record.env)
        .bind(&record.id)
        .bind(&record.actor)
        .bind(&record.kind)
        .bind(record.base_generation)
        .bind(&record.set_id)
        .bind(serde_json::to_string(&record.body)?)
        .bind(record.created_at)
        .bind(record.expires_at)
        .execute(&mut *tx)
        .await?;
        let audit = AuditIntent {
            target: Some(format!("{}/{}/{}", plan.app, plan.env, record.id)),
            ..plan.audit
        };
        audit::insert(&mut tx, &audit, None, None).await?;
        tx.commit().await?;
        Ok(record)
    }

    async fn plan(&self, org: &str, app: &str, env: &str, id: &str) -> Result<Option<PlanRecord>> {
        self.check_org(org)?;
        let row: Option<PlanRow> = sqlx::query_as(
            "SELECT id, app_id, env_id, actor_id, kind, base_generation, set_id, body, \
             created_at, expires_at FROM plans \
             WHERE org_id = ? AND app_id = ? AND env_id = ? AND id = ?",
        )
        .bind(org)
        .bind(app)
        .bind(env)
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        row.map(plan_record).transpose()
    }

    async fn issue_permits(
        &self,
        org: &str,
        app: &str,
        env: &str,
        plan: &str,
        permits: Vec<NewPermit>,
        audit: AuditIntent,
    ) -> Result<Vec<PermitRecord>> {
        self.check_org(org)?;
        let issuer = audit
            .actor
            .clone()
            .ok_or_else(|| StoreError::Forbidden("permits need an acting principal".into()))?;
        let mut tx = self.begin().await?;
        let row: Option<(i64, i64)> = sqlx::query_as(
            "SELECT base_generation, expires_at FROM plans \
             WHERE org_id = ? AND app_id = ? AND env_id = ? AND id = ?",
        )
        .bind(org)
        .bind(app)
        .bind(env)
        .bind(plan)
        .fetch_optional(&mut *tx)
        .await?;
        let (base_generation, plan_expires) =
            row.ok_or_else(|| StoreError::NotFound(format!("plan `{plan}`")))?;
        let now = now_ms();
        if plan_expires <= now {
            return Err(StoreError::PermitInvalid(format!(
                "plan `{plan}` has expired; preview again"
            )));
        }
        let generation: i64 = sqlx::query_scalar(
            "SELECT generation FROM envs WHERE org_id = ? AND app_id = ? AND id = ?",
        )
        .bind(org)
        .bind(app)
        .bind(env)
        .fetch_one(&mut *tx)
        .await?;
        if base_generation != generation {
            return Err(StoreError::PermitInvalid(format!(
                "plan `{plan}` was made at generation {base_generation}, but the environment is \
                 at {generation}; preview again"
            )));
        }
        if let Some(p) = permits
            .iter()
            .find(|p| !ACTIONS.contains(&p.action.as_str()))
        {
            return Err(StoreError::Invalid(format!(
                "`{}` is not a destructive action; use one of {}",
                p.action,
                ACTIONS.join(", ")
            )));
        }
        let env_type = env_type(&mut tx, org, app, env).await?;
        // The issuer needs the authority the actions need, checked with the
        // issue.
        check_env_authority(&mut tx, org, Some(&issuer), app, &env_type).await?;
        if permits.iter().any(|p| p.action == "detach") {
            check_admin_authority(&mut tx, org, Some(&issuer)).await?;
        }
        let mut records = Vec::new();
        for p in permits {
            let record = PermitRecord {
                id: format!("prm-{}", random_hex(16)),
                plan: plan.to_owned(),
                descriptor: p.descriptor,
                guresid: p.guresid,
                action: p.action,
                plan_fingerprint: p.plan_fingerprint,
                base_generation,
                issuer: issuer.clone(),
                expires_at: now + PERMIT_TTL_MS,
                bound_to: None,
            };
            sqlx::query(
                "INSERT INTO permits (org_id, id, app_id, env_id, plan_id, descriptor, guresid, \
                 action, plan_fingerprint, base_generation, config_digest, issuer_id, \
                 created_at, expires_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(org)
            .bind(&record.id)
            .bind(app)
            .bind(env)
            .bind(plan)
            .bind(&record.descriptor)
            .bind(&record.guresid)
            .bind(&record.action)
            .bind(&record.plan_fingerprint)
            .bind(base_generation)
            .bind(&p.config_digest)
            .bind(&issuer)
            .bind(now)
            .bind(record.expires_at)
            .execute(&mut *tx)
            .await?;
            records.push(record);
        }
        let audit = AuditIntent {
            target: Some(format!("{app}/{env}/{plan}")),
            ..audit
        };
        audit::insert(&mut tx, &audit, None, None).await?;
        tx.commit().await?;
        Ok(records)
    }

    async fn use_permit(&self, u: PermitUse<'_>) -> Result<()> {
        self.check_org(u.org)?;
        if u.deployment.is_none() {
            return Err(StoreError::Invalid(
                "a detach uses its permit inside the detach itself".into(),
            ));
        }
        let mut tx = self.begin().await?;
        consume_permit(&mut tx, &u).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn record_approval_request(
        &self,
        org: &str,
        app: &str,
        env: &str,
        request: &ApprovalRequest,
    ) -> Result<()> {
        self.check_org(org)?;
        let mut tx = self.begin().await?;
        sqlx::query("DELETE FROM approval_requests WHERE org_id = ? AND guresid = ?")
            .bind(org)
            .bind(&request.guresid)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO approval_requests (org_id, app_id, env_id, guresid, descriptor, action, \
             fingerprint, actions, deployment_id, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(org)
        .bind(app)
        .bind(env)
        .bind(&request.guresid)
        .bind(&request.descriptor)
        .bind(&request.action)
        .bind(&request.fingerprint)
        .bind(serde_json::to_string(&request.actions)?)
        .bind(&request.deployment_id)
        .bind(now_ms())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn approval_requests(
        &self,
        org: &str,
        app: &str,
        env: &str,
    ) -> Result<Vec<ApprovalRequest>> {
        self.check_org(org)?;
        let rows: Vec<(String, String, String, String, String, String)> = sqlx::query_as(
            "SELECT descriptor, guresid, action, fingerprint, actions, deployment_id \
             FROM approval_requests WHERE org_id = ? AND app_id = ? AND env_id = ? \
             ORDER BY descriptor",
        )
        .bind(org)
        .bind(app)
        .bind(env)
        .fetch_all(self.pool())
        .await?;
        rows.into_iter()
            .map(
                |(descriptor, guresid, action, fingerprint, actions, deployment_id)| {
                    Ok(ApprovalRequest {
                        descriptor,
                        guresid,
                        action,
                        fingerprint,
                        actions: serde_json::from_str(&actions)?,
                        deployment_id,
                    })
                },
            )
            .collect()
    }
}

/// Authorizes a destructive action with a permit inside the caller's
/// transaction, as [`PermitStore::use_permit`] describes; a detach passes
/// the permits it presents, which only a detach may consume this way.
pub(crate) async fn consume_permit(conn: &mut SqliteConnection, u: &PermitUse<'_>) -> Result<()> {
    if u.deployment.is_none() && u.action != "detach" {
        return Err(StoreError::Invalid(format!(
            "a {} permit is used only by the deployment it is bound to",
            u.action
        )));
    }
    let env_type = env_type(&mut *conn, u.org, u.app, u.env).await?;
    let candidates: Vec<PermitRow> = match u.deployment {
        Some(deployment) => {
            sqlx::query_as(
                "SELECT id, issuer_id, guresid, plan_fingerprint, base_generation, \
                     expires_at, bound_to FROM permits WHERE org_id = ? AND app_id = ? \
                     AND env_id = ? AND bound_to = ? AND descriptor = ? AND action = ? \
                     ORDER BY id",
            )
            .bind(u.org)
            .bind(u.app)
            .bind(u.env)
            .bind(deployment)
            .bind(u.descriptor)
            .bind(u.action)
            .fetch_all(&mut *conn)
            .await?
        }
        None => {
            let mut rows = Vec::new();
            for id in u.presented {
                let row: Option<PermitRow> = sqlx::query_as(
                    "SELECT id, issuer_id, guresid, plan_fingerprint, base_generation, \
                         expires_at, bound_to FROM permits WHERE org_id = ? AND app_id = ? \
                         AND env_id = ? AND id = ? AND descriptor = ? AND action = ?",
                )
                .bind(u.org)
                .bind(u.app)
                .bind(u.env)
                .bind(id)
                .bind(u.descriptor)
                .bind(u.action)
                .fetch_optional(&mut *conn)
                .await?;
                rows.extend(row);
            }
            rows
        }
    };
    let generation: i64 = sqlx::query_scalar(
        "SELECT generation FROM envs WHERE org_id = ? AND app_id = ? AND id = ?",
    )
    .bind(u.org)
    .bind(u.app)
    .bind(u.env)
    .fetch_one(&mut *conn)
    .await?;
    let now = now_ms();
    let mut refused = Vec::new();
    for (id, issuer, guresid, fingerprint, base_generation, expires_at, bound_to) in candidates {
        if guresid.is_some() && guresid.as_deref() != u.guresid {
            refused.push(format!("permit `{id}` is for another resource identity"));
            continue;
        }
        if fingerprint.is_some() && fingerprint.as_deref() != u.fingerprint {
            refused.push(format!(
                "permit `{id}` covers driver plan {}, not this one",
                fingerprint.as_deref().unwrap_or_default()
            ));
            continue;
        }
        if u.deployment.is_none() {
            // A detach binds the permit itself, with the checks an
            // acceptance makes.
            if bound_to.is_some() {
                refused.push(format!("permit `{id}` was already used"));
                continue;
            }
            if expires_at <= now {
                refused.push(format!("permit `{id}` has expired"));
                continue;
            }
            if base_generation != generation {
                refused.push(format!(
                    "permit `{id}` was issued at generation {base_generation}, but the \
                     environment is at {generation}"
                ));
                continue;
            }
        }
        // A permit is only as good as its issuer's authority now.
        if let Some(e) = issuer_lost(conn, u.org, u.app, &env_type, u.action, &issuer).await? {
            refused.push(format!("permit `{id}` no longer holds: {e}"));
            continue;
        }
        sqlx::query(
            "UPDATE permits SET used_at = COALESCE(used_at, ?), \
             bound_to = COALESCE(bound_to, ?), bound_at = COALESCE(bound_at, ?) \
             WHERE org_id = ? AND id = ?",
        )
        .bind(now)
        .bind(DETACHED)
        .bind(now)
        .bind(u.org)
        .bind(&id)
        .execute(&mut *conn)
        .await?;
        return Ok(());
    }
    let what = format!("{} of `{}`", u.action, u.descriptor);
    Err(StoreError::ApprovalRequired(if refused.is_empty() {
        format!("no permit covers the {what}")
    } else {
        format!("no permit covers the {what}: {}", refused.join("; "))
    }))
}

/// Why `issuer` no longer holds the authority an `action` permit needs, or
/// `None` while it does.
async fn issuer_lost(
    conn: &mut SqliteConnection,
    org: &str,
    app: &str,
    env_type: &str,
    action: &str,
    issuer: &str,
) -> Result<Option<String>> {
    let authority = if action == "detach" {
        check_admin_authority(&mut *conn, org, Some(issuer)).await
    } else {
        check_env_authority(&mut *conn, org, Some(issuer), app, env_type).await
    };
    match authority {
        Ok(()) => Ok(None),
        Err(StoreError::Forbidden(e)) => Ok(Some(e)),
        Err(e) => Err(e),
    }
}

async fn env_type(conn: &mut SqliteConnection, org: &str, app: &str, env: &str) -> Result<String> {
    sqlx::query_scalar("SELECT type FROM envs WHERE org_id = ? AND app_id = ? AND id = ?")
        .bind(org)
        .bind(app)
        .bind(env)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(|| StoreError::NotFound(format!("environment `{app}/{env}`")))
}

/// Checks the permits a deployment presents and binds them to it, inside its
/// acceptance and before the generation moves: each must be for the
/// environment, unexpired, unused, issued at `generation` for a resource
/// whose configuration is still the previewed one, and every known
/// destructive action must have one.
pub(crate) async fn bind_permits(
    conn: &mut SqliteConnection,
    org: &str,
    app: &str,
    env: &str,
    generation: i64,
    deployment: &str,
    binding: &PermitBinding,
) -> Result<()> {
    let now = now_ms();
    let mut covered: BTreeSet<(String, String)> = BTreeSet::new();
    let presented: BTreeSet<&String> = binding.presented.iter().collect();
    let env_type = if presented.is_empty() {
        String::new()
    } else {
        env_type(conn, org, app, env).await?
    };
    for id in presented {
        let row: Option<BindRow> = sqlx::query_as(
            "SELECT descriptor, action, base_generation, config_digest, expires_at, \
                 bound_to, issuer_id FROM permits \
                 WHERE org_id = ? AND app_id = ? AND env_id = ? AND id = ?",
        )
        .bind(org)
        .bind(app)
        .bind(env)
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
        let Some((descriptor, action, base_generation, digest, expires_at, bound_to, issuer)) = row
        else {
            return Err(StoreError::PermitInvalid(format!(
                "permit `{id}` was not issued for `{app}/{env}`"
            )));
        };
        if bound_to.is_some() {
            // A permit authorizes one deployment and its retries; a later
            // deployment previews again.
            return Err(StoreError::PermitInvalid(format!(
                "permit `{id}` was already bound to another deployment; preview again"
            )));
        }
        if expires_at <= now {
            return Err(StoreError::PermitInvalid(format!(
                "permit `{id}` has expired; preview again"
            )));
        }
        if base_generation != generation {
            return Err(StoreError::PermitInvalid(format!(
                "permit `{id}` was issued at generation {base_generation}, but the environment \
                 is at {generation}; preview again"
            )));
        }
        if action == "detach" {
            return Err(StoreError::PermitInvalid(format!(
                "permit `{id}` is for a detach, which no deployment runs"
            )));
        }
        if let Some(e) = issuer_lost(conn, org, app, &env_type, &action, &issuer).await? {
            return Err(StoreError::PermitInvalid(format!(
                "permit `{id}` no longer holds: {e}"
            )));
        }
        if binding.digests.get(&descriptor) != Some(&digest) {
            return Err(StoreError::PermitInvalid(format!(
                "permit `{id}` is for `{descriptor}` as previewed, but its configuration has \
                 changed since or the deployment does not change it; preview again"
            )));
        }
        sqlx::query("UPDATE permits SET bound_to = ?, bound_at = ? WHERE org_id = ? AND id = ?")
            .bind(deployment)
            .bind(now)
            .bind(org)
            .bind(id)
            .execute(&mut *conn)
            .await?;
        covered.insert((descriptor, action));
    }
    let missing: Vec<String> = binding
        .required
        .iter()
        .filter(|r| !covered.contains(*r))
        .map(|(descriptor, action)| format!("{action} of `{descriptor}`"))
        .collect();
    if !missing.is_empty() {
        return Err(StoreError::ApprovalRequired(format!(
            "the deployment needs permits for: {}; preview it and request them",
            missing.join(", ")
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
