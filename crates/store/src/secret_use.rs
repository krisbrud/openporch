//! Secret-use authorization. Holding a reference is not permission to use it: a
//! reference is usable only by the object that wrote it, or in a scope an
//! administrator granted. These checks run when an object is assigned a reference
//! and again when the executor resolves it, so removing a grant blocks later use.

use std::future::Future;

use openporch_core::model::SecretRef;
use serde_json::{Map, Value};
use sqlx::SqliteConnection;

use crate::secrets::{BUILTIN_STORE, written_name};
use crate::sqlite::SqliteStore;
use crate::sqlite_catalog::check_ref_stores;
use crate::sqlite_values::check_secret_use;
use crate::{Result, StoreError};

/// The object whose configuration holds a secret reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretOwner<'a> {
    /// Shared values of the consuming application or environment.
    Values,
    /// A resource definition.
    Definition(&'a str),
    /// A cloud account.
    Account(&'a str),
    /// An external driver's token.
    Driver(&'a str),
    /// The secret outputs of the resource with this GUResID.
    Resource(&'a str),
}

impl SecretOwner<'_> {
    /// The prefix under which the object's own `{value}` secrets are written,
    /// relative to the organization's namespace in a store.
    fn own_prefix(self) -> Option<String> {
        match self {
            Self::Values => None,
            Self::Definition(id) => Some(format!("defs/{id}/")),
            Self::Account(id) => Some(format!("accounts/{id}/")),
            Self::Driver(id) => Some(format!("drivers/{id}/")),
            Self::Resource(guresid) => Some(format!("resources/{guresid}/")),
        }
    }

    fn describe(self) -> String {
        match self {
            Self::Values => "shared values".to_owned(),
            Self::Definition(id) => format!("definition `{id}`"),
            Self::Account(id) => format!("cloud account `{id}`"),
            Self::Driver(id) => format!("driver `{id}`"),
            Self::Resource(guresid) => format!("resource `{guresid}`"),
        }
    }

    /// Whether `r` carries this object's own name.
    fn named_for(self, org: &str, r: &SecretRef) -> bool {
        written_name(org, r).is_some_and(|name| {
            self.own_prefix()
                .is_some_and(|p| name.starts_with(p.as_str()))
        })
    }

    /// Whether `r` is a secret openporch wrote for this object: its own name,
    /// and, in an external store, one openporch created there.
    async fn owns(self, conn: &mut SqliteConnection, org: &str, r: &SecretRef) -> Result<bool> {
        if !self.named_for(org, r) {
            return Ok(false);
        }
        created_externally(conn, org, r).await
    }
}

/// Resolution-time authorization of secret references.
pub trait SecretUseStore: Send + Sync {
    /// Whether `owner`'s reference `r` may be resolved for a deployment of `app` to
    /// `env`. The executor calls this immediately before it reads the secret, so a
    /// grant removed after assignment, or a reference replayed into another scope,
    /// is refused before any driver call.
    fn authorize_secret_use(
        &self,
        org: &str,
        app: &str,
        env: &str,
        owner: SecretOwner<'_>,
        r: &SecretRef,
    ) -> impl Future<Output = Result<()>> + Send;
}

impl SecretUseStore for SqliteStore {
    async fn authorize_secret_use(
        &self,
        org: &str,
        app: &str,
        env: &str,
        owner: SecretOwner<'_>,
        r: &SecretRef,
    ) -> Result<()> {
        self.check_org(org)?;
        let mut conn = self.pool().acquire().await?;
        check_use(&mut conn, org, app, env, owner, r).await
    }
}

/// Checks one use of `r` by `owner` for `app`/`env`. An object's own secrets need no
/// grant: for a definition, the consumer scope is its criteria, which the executor
/// matched before it asks.
pub(crate) async fn check_use(
    conn: &mut SqliteConnection,
    org: &str,
    app: &str,
    env: &str,
    owner: SecretOwner<'_>,
    r: &SecretRef,
) -> Result<()> {
    if owner == SecretOwner::Values {
        return check_secret_use(conn, org, app, Some(env), r).await;
    }
    // A resource's outputs are used only within its own environment.
    if let SecretOwner::Resource(guresid) = owner {
        let active: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM active_resources WHERE org_id = ? AND app_id = ? AND env_id = ? \
             AND guresid = ?",
        )
        .bind(org)
        .bind(app)
        .bind(env)
        .bind(guresid)
        .fetch_optional(&mut *conn)
        .await?;
        if active.is_none() {
            return Err(StoreError::Forbidden(format!(
                "resource `{guresid}` is not in `{app}/{env}`, so its secret outputs cannot be \
                 used there"
            )));
        }
    }
    let mut refs = Map::new();
    refs.insert("ref".into(), serde_json::to_value(r)?);
    check_ref_stores(conn, org, &refs).await?;
    if owner.owns(conn, org, r).await? {
        return Ok(());
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
            "no grant lets {} use secret `{}` in store `{}` for `{app}/{env}`",
            owner.describe(),
            r.reference,
            r.store
        ))),
    }
}

/// Checks the references a platform object is assigned. Each one is a secret the
/// object wrote itself, or an administrator granted it to some scope. Which scope
/// may use it is checked at resolution ([`check_use`]), when the consuming
/// application and environment are known. The caller has already checked that the
/// stores are registered.
pub(crate) async fn check_assigned(
    conn: &mut SqliteConnection,
    org: &str,
    owner: SecretOwner<'_>,
    refs: &Map<String, Value>,
) -> Result<()> {
    let mut found = Vec::new();
    collect(refs, &mut found);
    for r in found {
        if owner.owns(conn, org, &r).await? {
            continue;
        }
        let granted: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM secret_grants WHERE org_id = ? AND store_id = ? AND ref = ? \
             LIMIT 1",
        )
        .bind(org)
        .bind(&r.store)
        .bind(&r.reference)
        .fetch_optional(&mut *conn)
        .await?;
        if granted.is_none() {
            return Err(StoreError::Forbidden(format!(
                "{} cannot use secret `{}` in store `{}`: no grant covers it",
                owner.describe(),
                r.reference,
                r.store
            )));
        }
    }
    Ok(())
}

/// The references in `refs`, at any depth.
fn collect(refs: &Map<String, Value>, out: &mut Vec<SecretRef>) {
    for v in refs.values() {
        let Some(m) = v.as_object() else { continue };
        match (
            m.get("store").and_then(Value::as_str),
            m.get("ref").and_then(Value::as_str),
        ) {
            (Some(store), Some(reference)) => out.push(SecretRef {
                store: store.to_owned(),
                reference: reference.to_owned(),
                version: None,
            }),
            _ => collect(m, out),
        }
    }
}

/// Where an external store's secrets live: `<cloud account>/<region>` from its
/// configuration.
pub(crate) fn destination(config: &Map<String, Value>) -> String {
    let get = |k: &str| config.get(k).and_then(Value::as_str).unwrap_or_default();
    format!("{}/{}", get("cloud_account"), get("region"))
}

/// Whether `r` is in the built-in store, which only openporch writes, or is a
/// secret openporch created in an external store, in the store's current
/// destination.
pub(crate) async fn created_externally(
    conn: &mut SqliteConnection,
    org: &str,
    r: &SecretRef,
) -> Result<bool> {
    if r.store == BUILTIN_STORE {
        return Ok(true);
    }
    Ok(external_state(conn, org, &r.store, &r.reference).await? == Some(true))
}

/// What openporch recorded about `reference` in external store `store`, in
/// the store's current destination: `Some(true)` once created, `Some(false)`
/// while a create is pending, `None` when it never created it there.
pub(crate) async fn external_state(
    conn: &mut SqliteConnection,
    org: &str,
    store: &str,
    reference: &str,
) -> Result<Option<bool>> {
    let row: Option<(String, i64, String)> = sqlx::query_as(
        "SELECT e.destination, e.confirmed, s.config FROM external_secrets e \
         JOIN secret_stores s ON s.org_id = e.org_id AND s.id = e.store_id \
         WHERE e.org_id = ? AND e.store_id = ? AND e.ref = ?",
    )
    .bind(org)
    .bind(store)
    .bind(reference)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((recorded, confirmed, config)) = row else {
        return Ok(None);
    };
    let config: Map<String, Value> = serde_json::from_str(&config)?;
    Ok((recorded == destination(&config)).then_some(confirmed == 1))
}
