//! Cloud accounts, secret stores and secret-use grants.
//!
//! Account and store configuration forms a credential graph: an account may read
//! its secrets from a store, and an external store reads through an account. Every
//! change keeps that graph acyclic, so credentials always resolve to the host
//! identity or the built-in store. Mutations recheck administrator authority and
//! their references inside their transaction.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;

use openporch_core::ids::AccountId;
use openporch_core::model::{CloudAccount, CloudAccountType, SecretRef};
use serde::Serialize;
use serde_json::{Map, Value};
use sqlx::SqliteConnection;

use crate::audit::{self, AuditIntent};
use crate::identity::check_admin_authority;
use crate::secrets::BUILTIN_STORE;
use crate::sqlite::{SqliteStore, unique_violation};
use crate::sqlite_catalog::{check_ref_stores, corrupt, definitions_using, referenced_stores};
use crate::{Result, StoreError, now_ms, random_hex};

/// A stored cloud account.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AccountRecord {
    /// The account, with secrets as references.
    #[serde(flatten)]
    pub account: CloudAccount,
    /// Creation time.
    pub created_at: i64,
}

/// A registered secret store.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SecretStoreRecord {
    /// Store ID, used in references.
    pub id: String,
    /// Store type, such as `openporch` or `aws-secrets-manager`.
    #[serde(rename = "type")]
    pub ty: String,
    /// Type-specific configuration, such as `region` and `cloud_account`.
    pub config: Map<String, Value>,
    /// Whether new secrets are written to this store.
    pub primary: bool,
    /// Creation time.
    pub created_at: i64,
}

/// A secret-use grant: the scope allowed to use one reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GrantRecord {
    /// Grant ID.
    pub id: String,
    /// The store.
    pub store: String,
    /// The store-specific reference.
    #[serde(rename = "ref")]
    pub reference: String,
    /// The application allowed to use it, unless org-wide.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    /// Narrows the grant to one environment of the application.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env_id: Option<String>,
    /// Usable everywhere in the organization, for platform definitions.
    pub org_wide: bool,
    /// Creation time.
    pub created_at: i64,
    /// The granting administrator.
    pub created_by: String,
}

/// A grant to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewGrant {
    /// The store.
    pub store: String,
    /// The reference.
    pub reference: String,
    /// The application, unless org-wide.
    pub app_id: Option<String>,
    /// The environment, within the application.
    pub env_id: Option<String>,
    /// Org-wide scope.
    pub org_wide: bool,
}

/// Cloud accounts, secret stores and grants. Every mutation needs an administrator.
pub trait AccountStore: Send + Sync {
    /// All cloud accounts.
    fn accounts(&self, org: &str) -> impl Future<Output = Result<Vec<AccountRecord>>> + Send;

    /// A cloud account.
    fn account(
        &self,
        org: &str,
        id: &str,
    ) -> impl Future<Output = Result<Option<AccountRecord>>> + Send;

    /// Runs the checks of a create (`create`) or update without changing anything,
    /// so a request that would be rejected writes no secrets first.
    fn check_account(
        &self,
        org: &str,
        account: &CloudAccount,
        create: bool,
        actor: Option<&str>,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Creates a cloud account.
    fn create_account(
        &self,
        org: &str,
        account: &CloudAccount,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Replaces a cloud account's name, configuration and secrets. The type stays.
    fn update_account(
        &self,
        org: &str,
        account: &CloudAccount,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Deletes a cloud account no definition or secret store uses.
    fn delete_account(
        &self,
        org: &str,
        id: &str,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<()>> + Send;

    /// All secret stores, the built-in one included.
    fn secret_stores(
        &self,
        org: &str,
    ) -> impl Future<Output = Result<Vec<SecretStoreRecord>>> + Send;

    /// A secret store.
    fn secret_store(
        &self,
        org: &str,
        id: &str,
    ) -> impl Future<Output = Result<Option<SecretStoreRecord>>> + Send;

    /// Registers a secret store. It is never primary on creation.
    fn create_secret_store(
        &self,
        org: &str,
        store: &SecretStoreRecord,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Replaces a registered store's configuration.
    fn update_secret_store(
        &self,
        org: &str,
        id: &str,
        config: &Map<String, Value>,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Deletes a store nothing references: not the built-in or primary store, and
    /// not one named by definitions, accounts, drivers or grants.
    fn delete_secret_store(
        &self,
        org: &str,
        id: &str,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<()>> + Send;

    /// The grants of one store.
    fn grants(
        &self,
        org: &str,
        store: &str,
    ) -> impl Future<Output = Result<Vec<GrantRecord>>> + Send;

    /// Creates a grant and returns it.
    fn create_grant(
        &self,
        org: &str,
        grant: &NewGrant,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<GrantRecord>> + Send;

    /// Removes a grant; later resolutions in its scope are refused.
    fn delete_grant(
        &self,
        org: &str,
        store: &str,
        id: &str,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<()>> + Send;
}

/// A node of the credential graph.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Node {
    Account(String),
    Store(String),
}

impl std::fmt::Display for Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Node::Account(a) => write!(f, "account `{a}`"),
            Node::Store(s) => write!(f, "store `{s}`"),
        }
    }
}

/// The external stores an account reads its secrets from.
fn account_edges(secret_refs: &BTreeMap<String, SecretRef>) -> BTreeSet<Node> {
    secret_refs
        .values()
        .filter(|r| r.store != BUILTIN_STORE)
        .map(|r| Node::Store(r.store.clone()))
        .collect()
}

/// The account a store reads through.
fn store_edges(config: &Map<String, Value>) -> BTreeSet<Node> {
    config
        .get("cloud_account")
        .and_then(Value::as_str)
        .map(|a| Node::Account(a.to_owned()))
        .into_iter()
        .collect()
}

/// Rejects a change that would make credential resolution cyclic. `change` is the
/// node being written with its new outgoing edges.
async fn check_credential_graph(
    conn: &mut SqliteConnection,
    org: &str,
    change: (Node, BTreeSet<Node>),
) -> Result<()> {
    let mut graph: BTreeMap<Node, BTreeSet<Node>> = BTreeMap::new();
    let accounts: Vec<(String, String)> =
        sqlx::query_as("SELECT id, secret_refs FROM cloud_accounts WHERE org_id = ?")
            .bind(org)
            .fetch_all(&mut *conn)
            .await?;
    for (id, refs) in accounts {
        let refs: BTreeMap<String, SecretRef> = serde_json::from_str(&refs)?;
        graph.insert(Node::Account(id), account_edges(&refs));
    }
    let stores: Vec<(String, String)> =
        sqlx::query_as("SELECT id, config FROM secret_stores WHERE org_id = ?")
            .bind(org)
            .fetch_all(&mut *conn)
            .await?;
    for (id, config) in stores {
        let config: Map<String, Value> = serde_json::from_str(&config)?;
        graph.insert(Node::Store(id), store_edges(&config));
    }
    let (start, edges) = change;
    graph.insert(start.clone(), edges);
    // Depth-first search from the changed node; only it can close a new cycle.
    let mut stack = vec![(start.clone(), vec![start.clone()])];
    let mut seen = BTreeSet::new();
    while let Some((node, path)) = stack.pop() {
        for next in graph.get(&node).into_iter().flatten() {
            if *next == start {
                let chain: Vec<String> = path.iter().chain([next]).map(Node::to_string).collect();
                return Err(StoreError::Invalid(format!(
                    "credential cycle: {}",
                    chain.join(" -> ")
                )));
            }
            if seen.insert(next.clone()) {
                let mut p = path.clone();
                p.push(next.clone());
                stack.push((next.clone(), p));
            }
        }
    }
    Ok(())
}

fn type_name(ty: CloudAccountType) -> &'static str {
    match ty {
        CloudAccountType::Aws => "aws",
        CloudAccountType::AwsRole => "aws-role",
        CloudAccountType::AwsIdentity => "aws-identity",
    }
}

fn parse_type(s: &str) -> Result<CloudAccountType> {
    serde_json::from_value(Value::String(s.to_owned())).map_err(|e| corrupt("account type", e))
}

async fn account_row(
    conn: &mut SqliteConnection,
    org: &str,
    id: &str,
) -> Result<Option<AccountRecord>> {
    let row: Option<(String, String, String, String, String, i64)> = sqlx::query_as(
        "SELECT id, name, type, config, secret_refs, created_at FROM cloud_accounts \
         WHERE org_id = ? AND id = ?",
    )
    .bind(org)
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?;
    row.map(account_from_row).transpose()
}

fn account_from_row(
    (id, name, ty, config, secret_refs, created_at): (String, String, String, String, String, i64),
) -> Result<AccountRecord> {
    Ok(AccountRecord {
        account: CloudAccount {
            id: AccountId::new(id).map_err(|e| corrupt("account", e))?,
            name,
            ty: parse_type(&ty)?,
            config: serde_json::from_str(&config)?,
            secret_refs: serde_json::from_str(&secret_refs)?,
        },
        created_at,
    })
}

/// Checks a create or update of `account`: existence, the stores its references
/// name, and the credential graph.
async fn check_account_in(
    conn: &mut SqliteConnection,
    org: &str,
    account: &CloudAccount,
    create: bool,
) -> Result<()> {
    let id = account.id.as_str();
    let existing = account_row(conn, org, id).await?;
    match (&existing, create) {
        (Some(_), true) => {
            return Err(StoreError::AlreadyExists(format!("cloud account `{id}`")));
        }
        (None, false) => return Err(StoreError::NotFound(format!("cloud account `{id}`"))),
        (Some(e), false) if e.account.ty != account.ty => {
            return Err(StoreError::Invalid(format!(
                "the type of cloud account `{id}` cannot change from `{}`",
                type_name(e.account.ty)
            )));
        }
        _ => {}
    }
    let refs: Map<String, Value> = account
        .secret_refs
        .iter()
        .map(|(k, r)| Ok((k.clone(), serde_json::to_value(r)?)))
        .collect::<Result<_>>()?;
    check_ref_stores(conn, org, &refs).await?;
    check_credential_graph(
        conn,
        org,
        (
            Node::Account(id.to_owned()),
            account_edges(&account.secret_refs),
        ),
    )
    .await
}

async fn store_row(
    conn: &mut SqliteConnection,
    org: &str,
    id: &str,
) -> Result<Option<SecretStoreRecord>> {
    let row: Option<(String, String, String, i64, i64)> = sqlx::query_as(
        "SELECT id, type, config, is_primary, created_at FROM secret_stores \
         WHERE org_id = ? AND id = ?",
    )
    .bind(org)
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?;
    row.map(store_from_row).transpose()
}

fn store_from_row(
    (id, ty, config, primary, created_at): (String, String, String, i64, i64),
) -> Result<SecretStoreRecord> {
    Ok(SecretStoreRecord {
        id,
        ty,
        config: serde_json::from_str(&config)?,
        primary: primary != 0,
        created_at,
    })
}

/// Checks the account a store's configuration names: it must exist and be able
/// to hold credentials, and the credential graph must stay acyclic.
async fn check_store_config(
    conn: &mut SqliteConnection,
    org: &str,
    id: &str,
    config: &Map<String, Value>,
) -> Result<()> {
    if let Some(account) = config.get("cloud_account").and_then(Value::as_str) {
        match account_row(conn, org, account).await? {
            None => {
                return Err(StoreError::Invalid(format!(
                    "cloud account `{account}` does not exist"
                )));
            }
            Some(a) if a.account.ty == CloudAccountType::AwsIdentity => {
                return Err(StoreError::Invalid(format!(
                    "cloud account `{account}` of type `aws-identity` cannot read secret stores"
                )));
            }
            Some(_) => {}
        }
    }
    check_credential_graph(conn, org, (Node::Store(id.to_owned()), store_edges(config))).await
}

/// IDs of rows in `table` whose secret references, at `pointer` inside the JSON
/// `column`, name `store`. Only reference positions are scanned, so ordinary
/// values shaped like `{store, ref}` do not count.
async fn referencing_store(
    conn: &mut SqliteConnection,
    org: &str,
    table: &str,
    column: &str,
    pointer: &str,
    store: &str,
) -> Result<Vec<String>> {
    let rows: Vec<(String, String)> = sqlx::query_as(&format!(
        "SELECT id, {column} FROM {table} WHERE org_id = ? ORDER BY id"
    ))
    .bind(org)
    .fetch_all(&mut *conn)
    .await?;
    let mut out = Vec::new();
    for (id, body) in rows {
        let value: Value = serde_json::from_str(&body)?;
        let mut stores = Vec::new();
        match value.pointer(pointer) {
            // A single reference, such as a driver token.
            Some(Value::Object(m)) if m.get("ref").is_some_and(Value::is_string) => {
                if let Some(s) = m.get("store").and_then(Value::as_str) {
                    stores.push(s.to_owned());
                }
            }
            Some(Value::Object(m)) => referenced_stores(m, &mut stores),
            _ => {}
        }
        if stores.iter().any(|s| s == store) {
            out.push(id);
            if out.len() == 5 {
                break;
            }
        }
    }
    Ok(out)
}

fn grant_from_row(
    (id, store, reference, app_id, env_id, org_wide, created_at, created_by): GrantRow,
) -> GrantRecord {
    GrantRecord {
        id,
        store,
        reference,
        app_id,
        env_id,
        org_wide: org_wide != 0,
        created_at,
        created_by,
    }
}

type GrantRow = (
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    i64,
    i64,
    String,
);

impl AccountStore for SqliteStore {
    async fn accounts(&self, org: &str) -> Result<Vec<AccountRecord>> {
        self.check_org(org)?;
        let rows: Vec<(String, String, String, String, String, i64)> = sqlx::query_as(
            "SELECT id, name, type, config, secret_refs, created_at FROM cloud_accounts \
             WHERE org_id = ? ORDER BY id",
        )
        .bind(org)
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(account_from_row).collect()
    }

    async fn account(&self, org: &str, id: &str) -> Result<Option<AccountRecord>> {
        self.check_org(org)?;
        let mut conn = self.pool().acquire().await?;
        account_row(&mut conn, org, id).await
    }

    async fn check_account(
        &self,
        org: &str,
        account: &CloudAccount,
        create: bool,
        actor: Option<&str>,
    ) -> Result<()> {
        self.check_org(org)?;
        let mut conn = self.pool().acquire().await?;
        check_admin_authority(&mut conn, org, actor).await?;
        check_account_in(&mut conn, org, account, create).await
    }

    async fn create_account(
        &self,
        org: &str,
        account: &CloudAccount,
        audit: AuditIntent,
    ) -> Result<()> {
        self.check_org(org)?;
        let mut tx = self.begin().await?;
        check_admin_authority(&mut tx, org, audit.actor.as_deref()).await?;
        check_account_in(&mut tx, org, account, true).await?;
        let res = sqlx::query(
            "INSERT INTO cloud_accounts (org_id, id, name, type, config, secret_refs, created_at, created_by) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(org)
        .bind(account.id.as_str())
        .bind(&account.name)
        .bind(type_name(account.ty))
        .bind(serde_json::to_string(&account.config)?)
        .bind(serde_json::to_string(&account.secret_refs)?)
        .bind(now_ms())
        .bind(&audit.actor)
        .execute(&mut *tx)
        .await;
        if let Err(e) = &res
            && unique_violation(e)
        {
            return Err(StoreError::AlreadyExists(format!(
                "cloud account `{}`",
                account.id
            )));
        }
        res?;
        audit::insert(&mut tx, &audit, None, None).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn update_account(
        &self,
        org: &str,
        account: &CloudAccount,
        audit: AuditIntent,
    ) -> Result<()> {
        self.check_org(org)?;
        let mut tx = self.begin().await?;
        check_admin_authority(&mut tx, org, audit.actor.as_deref()).await?;
        check_account_in(&mut tx, org, account, false).await?;
        sqlx::query(
            "UPDATE cloud_accounts SET name = ?, config = ?, secret_refs = ? \
             WHERE org_id = ? AND id = ?",
        )
        .bind(&account.name)
        .bind(serde_json::to_string(&account.config)?)
        .bind(serde_json::to_string(&account.secret_refs)?)
        .bind(org)
        .bind(account.id.as_str())
        .execute(&mut *tx)
        .await?;
        audit::insert(&mut tx, &audit, None, None).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn delete_account(&self, org: &str, id: &str, audit: AuditIntent) -> Result<()> {
        self.check_org(org)?;
        let mut tx = self.begin().await?;
        check_admin_authority(&mut tx, org, audit.actor.as_deref()).await?;
        if account_row(&mut tx, org, id).await?.is_none() {
            return Err(StoreError::NotFound(format!("cloud account `{id}`")));
        }
        let defs = definitions_using(&mut tx, org, "driver_account", id).await?;
        if !defs.is_empty() {
            return Err(StoreError::Conflict(format!(
                "cloud account `{id}` is used by definitions {}",
                defs.join(", ")
            )));
        }
        let stores: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM secret_stores WHERE org_id = ? \
             AND json_extract(config, '$.cloud_account') = ? ORDER BY id LIMIT 5",
        )
        .bind(org)
        .bind(id)
        .fetch_all(&mut *tx)
        .await?;
        if !stores.is_empty() {
            return Err(StoreError::Conflict(format!(
                "cloud account `{id}` is used by secret stores {}",
                stores.join(", ")
            )));
        }
        sqlx::query("DELETE FROM cloud_accounts WHERE org_id = ? AND id = ?")
            .bind(org)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        audit::insert(&mut tx, &audit, None, None).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn secret_stores(&self, org: &str) -> Result<Vec<SecretStoreRecord>> {
        self.check_org(org)?;
        let rows: Vec<(String, String, String, i64, i64)> = sqlx::query_as(
            "SELECT id, type, config, is_primary, created_at FROM secret_stores \
             WHERE org_id = ? ORDER BY id",
        )
        .bind(org)
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(store_from_row).collect()
    }

    async fn secret_store(&self, org: &str, id: &str) -> Result<Option<SecretStoreRecord>> {
        self.check_org(org)?;
        let mut conn = self.pool().acquire().await?;
        store_row(&mut conn, org, id).await
    }

    async fn create_secret_store(
        &self,
        org: &str,
        store: &SecretStoreRecord,
        audit: AuditIntent,
    ) -> Result<()> {
        self.check_org(org)?;
        let mut tx = self.begin().await?;
        check_admin_authority(&mut tx, org, audit.actor.as_deref()).await?;
        if store_row(&mut tx, org, &store.id).await?.is_some() {
            return Err(StoreError::AlreadyExists(format!(
                "secret store `{}`",
                store.id
            )));
        }
        check_store_config(&mut tx, org, &store.id, &store.config).await?;
        sqlx::query(
            "INSERT INTO secret_stores (org_id, id, type, config, is_primary, created_at, created_by) \
             VALUES (?, ?, ?, ?, 0, ?, ?)",
        )
        .bind(org)
        .bind(&store.id)
        .bind(&store.ty)
        .bind(serde_json::to_string(&store.config)?)
        .bind(now_ms())
        .bind(&audit.actor)
        .execute(&mut *tx)
        .await?;
        audit::insert(&mut tx, &audit, None, None).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn update_secret_store(
        &self,
        org: &str,
        id: &str,
        config: &Map<String, Value>,
        audit: AuditIntent,
    ) -> Result<()> {
        self.check_org(org)?;
        let mut tx = self.begin().await?;
        check_admin_authority(&mut tx, org, audit.actor.as_deref()).await?;
        match store_row(&mut tx, org, id).await? {
            None => return Err(StoreError::NotFound(format!("secret store `{id}`"))),
            Some(s) if s.ty == BUILTIN_STORE => {
                return Err(StoreError::Conflict(
                    "the built-in store has no configuration to change".to_owned(),
                ));
            }
            Some(_) => {}
        }
        check_store_config(&mut tx, org, id, config).await?;
        sqlx::query("UPDATE secret_stores SET config = ? WHERE org_id = ? AND id = ?")
            .bind(serde_json::to_string(config)?)
            .bind(org)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        audit::insert(&mut tx, &audit, None, None).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn delete_secret_store(&self, org: &str, id: &str, audit: AuditIntent) -> Result<()> {
        self.check_org(org)?;
        let mut tx = self.begin().await?;
        check_admin_authority(&mut tx, org, audit.actor.as_deref()).await?;
        let store = store_row(&mut tx, org, id)
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("secret store `{id}`")))?;
        if store.id == BUILTIN_STORE || store.primary {
            return Err(StoreError::Conflict(format!(
                "secret store `{id}` is {} and cannot be deleted",
                if store.id == BUILTIN_STORE {
                    "the built-in store"
                } else {
                    "the primary store"
                }
            )));
        }
        let users = [
            (
                "definitions",
                referencing_store(
                    &mut tx,
                    org,
                    "definitions",
                    "driver_inputs",
                    "/secret_refs",
                    id,
                )
                .await?,
            ),
            (
                "cloud accounts",
                referencing_store(&mut tx, org, "cloud_accounts", "secret_refs", "", id).await?,
            ),
            (
                "drivers",
                referencing_store(&mut tx, org, "drivers", "body", "/token", id).await?,
            ),
            ("grants", {
                sqlx::query_scalar(
                    "SELECT id FROM secret_grants WHERE org_id = ? AND store_id = ? \
                     ORDER BY id LIMIT 5",
                )
                .bind(org)
                .bind(id)
                .fetch_all(&mut *tx)
                .await?
            }),
        ];
        let named: Vec<String> = users
            .iter()
            .filter(|(_, ids)| !ids.is_empty())
            .map(|(kind, ids)| format!("{kind} {}", ids.join(", ")))
            .collect();
        if !named.is_empty() {
            return Err(StoreError::Conflict(format!(
                "secret store `{id}` is referenced by {}",
                named.join("; ")
            )));
        }
        sqlx::query("DELETE FROM secret_stores WHERE org_id = ? AND id = ?")
            .bind(org)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        audit::insert(&mut tx, &audit, None, None).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn grants(&self, org: &str, store: &str) -> Result<Vec<GrantRecord>> {
        self.check_org(org)?;
        let rows: Vec<GrantRow> = sqlx::query_as(
            "SELECT id, store_id, ref, app_id, env_id, org_wide, created_at, created_by \
             FROM secret_grants WHERE org_id = ? AND store_id = ? ORDER BY created_at, id",
        )
        .bind(org)
        .bind(store)
        .fetch_all(self.pool())
        .await?;
        Ok(rows.into_iter().map(grant_from_row).collect())
    }

    async fn create_grant(
        &self,
        org: &str,
        grant: &NewGrant,
        audit: AuditIntent,
    ) -> Result<GrantRecord> {
        self.check_org(org)?;
        let actor = audit
            .actor
            .clone()
            .ok_or_else(|| StoreError::Forbidden("grants need an acting administrator".into()))?;
        let mut tx = self.begin().await?;
        check_admin_authority(&mut tx, org, Some(&actor)).await?;
        if store_row(&mut tx, org, &grant.store).await?.is_none() {
            return Err(StoreError::NotFound(format!(
                "secret store `{}`",
                grant.store
            )));
        }
        if let Some(app) = &grant.app_id {
            let exists: Option<i64> =
                sqlx::query_scalar("SELECT 1 FROM apps WHERE org_id = ? AND id = ?")
                    .bind(org)
                    .bind(app)
                    .fetch_optional(&mut *tx)
                    .await?;
            if exists.is_none() {
                return Err(StoreError::Invalid(format!(
                    "application `{app}` does not exist"
                )));
            }
            if let Some(env) = &grant.env_id {
                let exists: Option<i64> = sqlx::query_scalar(
                    "SELECT 1 FROM envs WHERE org_id = ? AND app_id = ? AND id = ?",
                )
                .bind(org)
                .bind(app)
                .bind(env)
                .fetch_optional(&mut *tx)
                .await?;
                if exists.is_none() {
                    return Err(StoreError::Invalid(format!(
                        "environment `{app}/{env}` does not exist"
                    )));
                }
            }
        }
        let duplicate: Option<String> = sqlx::query_scalar(
            "SELECT id FROM secret_grants WHERE org_id = ? AND store_id = ? AND ref = ? \
             AND app_id IS ? AND env_id IS ? AND org_wide = ?",
        )
        .bind(org)
        .bind(&grant.store)
        .bind(&grant.reference)
        .bind(&grant.app_id)
        .bind(&grant.env_id)
        .bind(i64::from(grant.org_wide))
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(existing) = duplicate {
            return Err(StoreError::AlreadyExists(format!(
                "an identical grant (`{existing}`)"
            )));
        }
        let record = GrantRecord {
            id: random_hex(8),
            store: grant.store.clone(),
            reference: grant.reference.clone(),
            app_id: grant.app_id.clone(),
            env_id: grant.env_id.clone(),
            org_wide: grant.org_wide,
            created_at: now_ms(),
            created_by: actor,
        };
        sqlx::query(
            "INSERT INTO secret_grants (org_id, id, store_id, ref, app_id, env_id, org_wide, created_at, created_by) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(org)
        .bind(&record.id)
        .bind(&record.store)
        .bind(&record.reference)
        .bind(&record.app_id)
        .bind(&record.env_id)
        .bind(i64::from(record.org_wide))
        .bind(record.created_at)
        .bind(&record.created_by)
        .execute(&mut *tx)
        .await?;
        audit::insert(&mut tx, &audit, None, None).await?;
        tx.commit().await?;
        Ok(record)
    }

    async fn delete_grant(
        &self,
        org: &str,
        store: &str,
        id: &str,
        audit: AuditIntent,
    ) -> Result<()> {
        self.check_org(org)?;
        let mut tx = self.begin().await?;
        check_admin_authority(&mut tx, org, audit.actor.as_deref()).await?;
        let deleted =
            sqlx::query("DELETE FROM secret_grants WHERE org_id = ? AND store_id = ? AND id = ?")
                .bind(org)
                .bind(store)
                .bind(id)
                .execute(&mut *tx)
                .await?;
        if deleted.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!(
                "grant `{id}` of secret store `{store}`"
            )));
        }
        audit::insert(&mut tx, &audit, None, None).await?;
        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_edges_skip_the_built_in_store() {
        let refs = BTreeMap::from([
            (
                "a".to_owned(),
                SecretRef {
                    store: BUILTIN_STORE.into(),
                    reference: "x".into(),
                    version: None,
                },
            ),
            (
                "b".to_owned(),
                SecretRef {
                    store: "prod-sm".into(),
                    reference: "y".into(),
                    version: None,
                },
            ),
        ]);
        assert_eq!(
            account_edges(&refs),
            BTreeSet::from([Node::Store("prod-sm".into())])
        );
    }
}
