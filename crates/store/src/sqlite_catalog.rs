//! The resource catalog: custom resource types, classes, the external driver
//! registry and resource definitions with their criteria.
//!
//! Every mutation rechecks administrator authority and the references it relies on
//! inside its transaction, so a concurrent deletion cannot leave a definition
//! pointing at a missing type, class, driver or cloud account.

use std::collections::BTreeMap;
use std::future::Future;

use openporch_core::catalog::{
    BUILTIN_DRIVERS, DriverTarget, ResourceClass, ResourceType, builtin_type,
};
use openporch_core::ids::{ClassId, CriterionId, DefinitionId, ResourceTypeId};
use openporch_core::model::{Criterion, DriverInputs, Provision, ResourceDefinition, SecretRef};
use openporch_core::placeholder::{Dialect, Expr, ResourceRef, Template};
use serde::{Deserialize, Serialize};
use sqlx::SqliteConnection;

use crate::audit::{self, AuditIntent};
use crate::identity::check_admin_authority;
use crate::secret_use::{SecretOwner, check_assigned};
use crate::sqlite::{SqliteStore, unique_violation};
use crate::{Result, StoreError, now_ms, random_hex};

/// An external driver registration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DriverRecord {
    /// `<namespace>/<name>`.
    pub id: String,
    /// The HTTPS URL the driver protocol is spoken to.
    pub target: String,
    /// JSON Schema of the driver inputs, if published.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs_schema: Option<serde_json::Value>,
    /// The bearer token sent to the driver, as a reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<SecretRef>,
    /// The protocol the driver speaks, such as `openporch-http/1`.
    pub protocol_version: String,
    /// The cookie format version the driver writes.
    pub cookie_version: u32,
    /// JSON Schema of the driver's outputs, if published.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outputs_schema: Option<serde_json::Value>,
    /// What the driver supports beyond provisioning and removing resources.
    #[serde(default)]
    pub capabilities: DriverCapabilities,
}

/// Which write of a driver registration to check.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DriverChange<'a> {
    /// A new registration.
    Create,
    /// A replacement of the registration `based_on`, as the request read it.
    Replace {
        /// The registration the replacement was built from, which must still
        /// be the stored one.
        based_on: &'a DriverRecord,
    },
}

/// The optional capabilities an external driver declares.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DriverCapabilities {
    /// What the driver provisions.
    pub targets: DriverTargets,
    /// `GET` reports the real state of the resource.
    pub observe: bool,
    /// A retried operation identity converges instead of duplicating effects.
    pub recover: bool,
    /// The driver previews its changes.
    pub preview: bool,
    /// A timed-out operation can be cancelled.
    pub cancel: bool,
}

/// What an external driver provisions. Registrations that do not say
/// provision resources other than workloads only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DriverTargets {
    /// Resources other than workloads.
    pub resources: bool,
    /// Workloads.
    pub workloads: bool,
    /// Workload triggers. No resource type is a trigger, so this is recorded
    /// but does not decide which definitions may use the driver.
    pub triggers: bool,
}

impl Default for DriverTargets {
    fn default() -> Self {
        Self {
            resources: true,
            workloads: false,
            triggers: false,
        }
    }
}

impl DriverTargets {
    /// Whether the driver can provision resources of type `ty`.
    pub fn covers(&self, ty: &str) -> bool {
        match DriverTarget::of_type(ty) {
            DriverTarget::Resource => self.resources,
            DriverTarget::Workload => self.workloads,
        }
    }
}

/// A stored resource definition.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DefinitionRecord {
    /// The definition, with stored criteria and secret inputs as references.
    #[serde(flatten)]
    pub definition: ResourceDefinition,
    /// Incremented on every update.
    pub version: i64,
    /// Creation time.
    pub created_at: i64,
    /// The creating principal.
    pub created_by: Option<String>,
    /// Last update time.
    pub updated_at: i64,
}

/// The fields of a definition that an update replaces. The ID, type and criteria
/// stay; criteria are managed one by one.
#[derive(Debug, Clone, PartialEq)]
pub struct DefinitionUpdate {
    /// Display name.
    pub name: String,
    /// The driver.
    pub driver_type: String,
    /// A cloud account ID or placeholder.
    pub driver_account: Option<String>,
    /// Driver inputs, with secrets already converted to references.
    pub driver_inputs: DriverInputs,
    /// Co-provisioned resources.
    pub provision: BTreeMap<String, Provision>,
}

/// The catalog side of the store. Every mutation needs an administrator.
pub trait CatalogStore: Send + Sync {
    /// Custom resource types; built-in types come from the core catalog.
    fn custom_types(&self, org: &str) -> impl Future<Output = Result<Vec<ResourceType>>> + Send;

    /// A custom resource type.
    fn custom_type(
        &self,
        org: &str,
        id: &str,
    ) -> impl Future<Output = Result<Option<ResourceType>>> + Send;

    /// Creates a custom resource type.
    fn create_type(
        &self,
        org: &str,
        ty: &ResourceType,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Replaces a custom resource type.
    fn update_type(
        &self,
        org: &str,
        ty: &ResourceType,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Deletes a custom resource type no definition uses, with its classes.
    fn delete_type(
        &self,
        org: &str,
        id: &str,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Defined classes, of one type or of all types. The implicit `default` class
    /// is not stored.
    fn classes(
        &self,
        org: &str,
        ty: Option<&str>,
    ) -> impl Future<Output = Result<Vec<ResourceClass>>> + Send;

    /// Creates a class of an existing type.
    fn create_class(
        &self,
        org: &str,
        class: &ResourceClass,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Deletes a class no criterion references.
    fn delete_class(
        &self,
        org: &str,
        ty: &str,
        id: &str,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Registered external drivers.
    fn drivers(&self, org: &str) -> impl Future<Output = Result<Vec<DriverRecord>>> + Send;

    /// A registered external driver.
    fn driver(
        &self,
        org: &str,
        id: &str,
    ) -> impl Future<Output = Result<Option<DriverRecord>>> + Send;

    /// Registers an external driver.
    fn create_driver(
        &self,
        org: &str,
        driver: &DriverRecord,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Replaces a registered driver's target, token, pins and capabilities. Refused
    /// while an operation the driver ran has an outcome that is not known yet, and
    /// when the driver would no longer target the type of a definition using it.
    ///
    /// `based_on` is the registration the replacement was built from, as read
    /// before; a registration changed since is refused with a conflict, so
    /// overlapping replacements cannot undo each other. The cookie version
    /// cannot change while the driver has active resources.
    fn replace_driver(
        &self,
        org: &str,
        driver: &DriverRecord,
        based_on: &DriverRecord,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Runs the checks of [`CatalogStore::create_driver`] or
    /// [`CatalogStore::replace_driver`] without changing anything, so a request
    /// that would be rejected writes no token first.
    fn check_driver(
        &self,
        org: &str,
        driver: &DriverRecord,
        change: DriverChange<'_>,
        actor: Option<&str>,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Removes an external driver no definition uses.
    fn delete_driver(
        &self,
        org: &str,
        id: &str,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<()>> + Send;

    /// All definitions with their criteria.
    fn definitions(&self, org: &str) -> impl Future<Output = Result<Vec<DefinitionRecord>>> + Send;

    /// A definition with its criteria.
    fn definition(
        &self,
        org: &str,
        id: &str,
    ) -> impl Future<Output = Result<Option<DefinitionRecord>>> + Send;

    /// Runs the checks of [`CatalogStore::create_definition`] without changing
    /// anything, so a request that would be rejected writes no secrets first.
    fn check_definition(
        &self,
        org: &str,
        def: &ResourceDefinition,
        actor: Option<&str>,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Creates a definition and its criteria, generating criterion IDs.
    fn create_definition(
        &self,
        org: &str,
        def: &ResourceDefinition,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Updates a definition and increments its version.
    fn update_definition(
        &self,
        org: &str,
        id: &str,
        update: &DefinitionUpdate,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Deletes a definition no active resource was provisioned by.
    fn delete_definition(
        &self,
        org: &str,
        id: &str,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Adds a criterion and returns its generated ID.
    fn add_criterion(
        &self,
        org: &str,
        def: &str,
        criterion: &Criterion,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<CriterionId>> + Send;

    /// Removes a criterion. Active resources are unaffected until the next
    /// deployment.
    fn delete_criterion(
        &self,
        org: &str,
        def: &str,
        id: &str,
        audit: AuditIntent,
    ) -> impl Future<Output = Result<()>> + Send;
}

pub(crate) fn corrupt(what: &str, e: impl std::fmt::Display) -> StoreError {
    StoreError::Corrupt(format!("{what}: {e}"))
}

/// Whether a type exists, built-in or custom.
async fn type_exists(conn: &mut SqliteConnection, org: &str, ty: &str) -> Result<bool> {
    if builtin_type(ty).is_some() {
        return Ok(true);
    }
    let row: Option<i64> =
        sqlx::query_scalar("SELECT 1 FROM resource_types WHERE org_id = ? AND id = ?")
            .bind(org)
            .bind(ty)
            .fetch_optional(&mut *conn)
            .await?;
    Ok(row.is_some())
}

async fn class_exists(
    conn: &mut SqliteConnection,
    org: &str,
    ty: &str,
    class: &ClassId,
) -> Result<bool> {
    if class.is_default() {
        return Ok(true);
    }
    let row: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM resource_classes WHERE org_id = ? AND resource_type = ? AND id = ?",
    )
    .bind(org)
    .bind(ty)
    .bind(class.as_str())
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.is_some())
}

/// Checks that the driver and the cloud account a definition of type `ty`
/// names exist, and that the driver can provision that type.
async fn check_driver_and_account(
    conn: &mut SqliteConnection,
    org: &str,
    ty: &str,
    driver_type: &str,
    driver_account: Option<&str>,
) -> Result<()> {
    if !BUILTIN_DRIVERS.contains(&driver_type) {
        let body: Option<String> =
            sqlx::query_scalar("SELECT body FROM drivers WHERE org_id = ? AND id = ?")
                .bind(org)
                .bind(driver_type)
                .fetch_optional(&mut *conn)
                .await?;
        let Some(body) = body else {
            return Err(StoreError::Invalid(format!(
                "driver `{driver_type}` is not registered"
            )));
        };
        #[derive(Deserialize)]
        struct Declared {
            #[serde(default)]
            capabilities: DriverCapabilities,
        }
        let declared: Declared =
            serde_json::from_str(&body).map_err(|e| corrupt("driver registration", e))?;
        if !declared.capabilities.targets.covers(ty) {
            return Err(StoreError::Invalid(format!(
                "driver `{driver_type}` cannot provision resources of type `{ty}`: its \
                 registration does not declare the `{}` target",
                DriverTarget::of_type(ty).as_str()
            )));
        }
    }
    let Some(account) = driver_account else {
        return Ok(());
    };
    // A template is resolved at deployment time; it must be valid now.
    let template = Template::parse(account).map_err(|e| {
        StoreError::Invalid(format!("`driver_account` `{account}` is invalid: {e}"))
    })?;
    if !template.is_literal() {
        for raw in template.placeholders() {
            Expr::interpret(raw, Dialect::DriverAccount).map_err(|e| {
                StoreError::Invalid(format!(
                    "`driver_account` placeholder `{}` is invalid: {e}",
                    raw.text
                ))
            })?;
        }
        return Ok(());
    }
    {
        let row: Option<i64> =
            sqlx::query_scalar("SELECT 1 FROM cloud_accounts WHERE org_id = ? AND id = ?")
                .bind(org)
                .bind(account)
                .fetch_optional(&mut *conn)
                .await?;
        if row.is_none() {
            return Err(StoreError::Invalid(format!(
                "cloud account `{account}` does not exist"
            )));
        }
    }
    Ok(())
}

/// The stores named by the references in `secret_refs`.
pub(crate) fn referenced_stores(
    refs: &serde_json::Map<String, serde_json::Value>,
    out: &mut Vec<String>,
) {
    for v in refs.values() {
        if let Some(m) = v.as_object() {
            match m.get("store").and_then(serde_json::Value::as_str) {
                Some(store) if m.get("ref").is_some_and(serde_json::Value::is_string) => {
                    out.push(store.to_owned())
                }
                _ => referenced_stores(m, out),
            }
        }
    }
}

/// Checks that every store the references name is registered and that `owner` may
/// use each reference.
async fn check_secret_stores(
    conn: &mut SqliteConnection,
    org: &str,
    owner: SecretOwner<'_>,
    inputs: &DriverInputs,
) -> Result<()> {
    check_ref_stores(conn, org, &inputs.secret_refs).await?;
    check_assigned(conn, org, owner, &inputs.secret_refs).await
}

/// Checks that every store the references in `refs` name is registered.
pub(crate) async fn check_ref_stores(
    conn: &mut SqliteConnection,
    org: &str,
    refs: &serde_json::Map<String, serde_json::Value>,
) -> Result<()> {
    let mut stores = Vec::new();
    referenced_stores(refs, &mut stores);
    stores.sort();
    stores.dedup();
    for store in stores {
        let row: Option<i64> =
            sqlx::query_scalar("SELECT 1 FROM secret_stores WHERE org_id = ? AND id = ?")
                .bind(org)
                .bind(&store)
                .fetch_optional(&mut *conn)
                .await?;
        if row.is_none() {
            return Err(StoreError::Invalid(format!(
                "secret store `{store}` is not registered"
            )));
        }
    }
    Ok(())
}

/// Checks a driver registration: on create that its ID is free; on replace that
/// it exists unchanged since the replacement read it, has no operation of
/// unknown outcome, keeps its cookie version while it has active resources and
/// still targets the types of the definitions using it; and that its token's
/// store is registered.
async fn check_driver_record(
    conn: &mut SqliteConnection,
    org: &str,
    driver: &DriverRecord,
    change: DriverChange<'_>,
) -> Result<()> {
    let id = driver.id.as_str();
    let create = matches!(change, DriverChange::Create);
    if !create && BUILTIN_DRIVERS.contains(&id) {
        return Err(StoreError::Conflict(format!(
            "built-in driver `{id}` cannot be modified"
        )));
    }
    let stored: Option<String> =
        sqlx::query_scalar("SELECT body FROM drivers WHERE org_id = ? AND id = ?")
            .bind(org)
            .bind(id)
            .fetch_optional(&mut *conn)
            .await?;
    if create && (BUILTIN_DRIVERS.contains(&id) || stored.is_some()) {
        return Err(StoreError::AlreadyExists(format!("driver `{id}`")));
    }
    if let DriverChange::Replace { based_on } = change {
        let Some(stored) = stored else {
            return Err(StoreError::NotFound(format!("driver `{id}`")));
        };
        let stored: DriverRecord =
            serde_json::from_str(&stored).map_err(|e| corrupt("driver registration", e))?;
        // A replacement fills what it leaves out, and checks the token it keeps,
        // from the registration it read; checked here, in the transaction, so a
        // concurrent change, such as a token rotation, is never undone.
        if stored != *based_on {
            return Err(StoreError::Conflict(format!(
                "driver `{id}` changed while this request was being handled; retry"
            )));
        }
        // Cookies and recovery depend on the registration an operation ran with.
        check_no_open_operations(conn, org, id).await?;
        // Active resources hold cookies of the version the driver writes now.
        if driver.cookie_version != stored.cookie_version {
            // Named by GUResID, the identity the cookies belong to; the first
            // ten, with a count of the rest.
            const SHOWN: i64 = 10;
            let total: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM active_resources WHERE org_id = ? AND driver_type = ?",
            )
            .bind(org)
            .bind(id)
            .fetch_one(&mut *conn)
            .await?;
            if total > 0 {
                let shown: Vec<String> = sqlx::query_scalar(
                    "SELECT '`' || guresid || '` (' || app_id || '/' || env_id || ')' \
                     FROM active_resources WHERE org_id = ? AND driver_type = ? \
                     ORDER BY guresid LIMIT ?",
                )
                .bind(org)
                .bind(id)
                .bind(SHOWN)
                .fetch_all(&mut *conn)
                .await?;
                let more = if total > SHOWN {
                    format!(" and {} more", total - SHOWN)
                } else {
                    String::new()
                };
                return Err(StoreError::Conflict(format!(
                    "the cookie version of driver `{id}` cannot change from {} while it has \
                     active resources, whose cookies are of that version: {}{more}",
                    stored.cookie_version,
                    shown.join(", ")
                )));
            }
        }
        let users: Vec<(String, String)> = sqlx::query_as(
            "SELECT id, type FROM definitions WHERE org_id = ? AND driver_type = ? ORDER BY id",
        )
        .bind(org)
        .bind(id)
        .fetch_all(&mut *conn)
        .await?;
        let uncovered: Vec<String> = users
            .into_iter()
            .filter(|(_, ty)| !driver.capabilities.targets.covers(ty))
            .map(|(def, ty)| format!("`{def}` (`{ty}`)"))
            .take(5)
            .collect();
        if !uncovered.is_empty() {
            return Err(StoreError::Invalid(format!(
                "driver `{id}` would no longer provision the types of definitions {} that use it",
                uncovered.join(", ")
            )));
        }
    }
    if let Some(token) = &driver.token {
        let mut inputs = DriverInputs::default();
        inputs.secret_refs.insert(
            "token".to_owned(),
            serde_json::to_value(token).map_err(|e| corrupt("driver token", e))?,
        );
        check_secret_stores(conn, org, SecretOwner::Driver(id), &inputs).await?;
    }
    Ok(())
}

/// Refuses a change to driver `id` while an operation it ran is still running or
/// of unknown outcome: such an operation is observed and recovered with the
/// registration it ran, which may no longer be the one its resource records.
async fn check_no_open_operations(conn: &mut SqliteConnection, org: &str, id: &str) -> Result<()> {
    let open: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT app_id || '/' || env_id FROM operations \
         WHERE org_id = ? AND driver_type = ? AND state IN ('started', 'unknown') \
         ORDER BY 1 LIMIT 5",
    )
    .bind(org)
    .bind(id)
    .fetch_all(&mut *conn)
    .await?;
    if !open.is_empty() {
        return Err(StoreError::Conflict(format!(
            "driver `{id}` has operations in {} whose outcome is not known yet",
            open.join(", ")
        )));
    }
    Ok(())
}

/// Checks a criterion's class against the definition's type and rejects a criterion
/// identical to one the definition already has.
async fn check_criterion(
    conn: &mut SqliteConnection,
    org: &str,
    ty: &str,
    criterion: &Criterion,
    existing: &[Criterion],
) -> Result<()> {
    if let Some(class) = &criterion.class
        && !class_exists(conn, org, ty, class).await?
    {
        return Err(StoreError::Invalid(format!(
            "class `{class}` is not defined for type `{ty}`"
        )));
    }
    let key = |c: &Criterion| Criterion {
        id: None,
        ..c.clone()
    };
    if existing.iter().any(|c| key(c) == key(criterion)) {
        return Err(StoreError::Conflict(
            "the definition already has an identical criterion".to_owned(),
        ));
    }
    Ok(())
}

/// Checks that every co-provisioning key names an existing type.
async fn check_provision(
    conn: &mut SqliteConnection,
    org: &str,
    provision: &BTreeMap<String, Provision>,
) -> Result<()> {
    for key in provision.keys() {
        let target = ResourceRef::parse(key)
            .map_err(|e| StoreError::Invalid(format!("`provision` key `{key}` is invalid: {e}")))?;
        if !type_exists(conn, org, target.ty.as_str()).await? {
            return Err(StoreError::Invalid(format!(
                "`provision` key `{key}` names resource type `{}`, which does not exist",
                target.ty
            )));
        }
    }
    Ok(())
}

/// Up to five definitions that co-provision type `ty`.
async fn definitions_provisioning(
    conn: &mut SqliteConnection,
    org: &str,
    ty: &str,
) -> Result<Vec<String>> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT id, provision FROM definitions WHERE org_id = ? ORDER BY id")
            .bind(org)
            .fetch_all(&mut *conn)
            .await?;
    let mut out = Vec::new();
    for (id, provision) in rows {
        let provision: BTreeMap<String, Provision> = serde_json::from_str(&provision)?;
        let uses = provision
            .keys()
            .any(|k| ResourceRef::parse(k).is_ok_and(|r| r.ty.as_str() == ty));
        if uses {
            out.push(id);
            if out.len() == 5 {
                break;
            }
        }
    }
    Ok(out)
}

/// Checks everything a definition refers to: its type, driver, cloud account,
/// secret stores and criterion classes, and that no two criteria are identical.
async fn check_definition_refs(
    conn: &mut SqliteConnection,
    org: &str,
    def: &ResourceDefinition,
) -> Result<()> {
    let ty = def.ty.as_str();
    if !type_exists(conn, org, ty).await? {
        return Err(StoreError::Invalid(format!(
            "resource type `{ty}` does not exist"
        )));
    }
    check_driver_and_account(
        conn,
        org,
        ty,
        &def.driver_type,
        def.driver_account.as_deref(),
    )
    .await?;
    check_secret_stores(
        conn,
        org,
        SecretOwner::Definition(def.id.as_str()),
        &def.driver_inputs,
    )
    .await?;
    check_provision(conn, org, &def.provision).await?;
    for (i, criterion) in def.criteria.iter().enumerate() {
        check_criterion(conn, org, ty, criterion, &def.criteria[..i]).await?;
    }
    Ok(())
}

async fn insert_criterion(
    conn: &mut SqliteConnection,
    org: &str,
    def: &str,
    criterion: &Criterion,
) -> Result<CriterionId> {
    let id = CriterionId::new(random_hex(8)).map_err(|e| corrupt("criterion ID", e))?;
    sqlx::query(
        "INSERT INTO criteria (org_id, definition_id, id, env_type, app_id, env_id, res_id, class) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(org)
    .bind(def)
    .bind(id.as_str())
    .bind(criterion.env_type.as_ref().map(|v| v.as_str()))
    .bind(criterion.app_id.as_ref().map(|v| v.as_str()))
    .bind(criterion.env_id.as_ref().map(|v| v.as_str()))
    .bind(criterion.res_id.as_ref().map(|v| v.as_str()))
    .bind(criterion.class.as_ref().map(|v| v.as_str()))
    .execute(&mut *conn)
    .await?;
    Ok(id)
}

type CriterionRow = (
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

async fn criteria_of(conn: &mut SqliteConnection, org: &str, def: &str) -> Result<Vec<Criterion>> {
    let rows: Vec<CriterionRow> = sqlx::query_as(
        "SELECT id, env_type, app_id, env_id, res_id, class FROM criteria \
         WHERE org_id = ? AND definition_id = ? ORDER BY rowid",
    )
    .bind(org)
    .bind(def)
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter()
        .map(|(id, env_type, app_id, env_id, res_id, class)| {
            // Stored values were validated on write; decode them through serde so
            // every ID type keeps its own rule.
            serde_json::from_value(serde_json::json!({
                "id": id,
                "env_type": env_type,
                "app_id": app_id,
                "env_id": env_id,
                "res_id": res_id,
                "class": class,
            }))
            .map_err(|e| corrupt("criterion", e))
        })
        .collect()
}

#[derive(sqlx::FromRow)]
struct DefinitionRow {
    id: String,
    name: String,
    #[sqlx(rename = "type")]
    ty: String,
    driver_type: String,
    driver_account: Option<String>,
    driver_inputs: String,
    provision: String,
    version: i64,
    created_at: i64,
    created_by: Option<String>,
    updated_at: i64,
}

const DEFINITION_COLUMNS: &str = "id, name, type, driver_type, driver_account, driver_inputs, \
     provision, version, created_at, created_by, updated_at";

async fn definition_from_row(
    conn: &mut SqliteConnection,
    org: &str,
    r: DefinitionRow,
) -> Result<DefinitionRecord> {
    let criteria = criteria_of(conn, org, &r.id).await?;
    Ok(DefinitionRecord {
        definition: ResourceDefinition {
            id: DefinitionId::new(&r.id).map_err(|e| corrupt("definition", e))?,
            name: r.name,
            ty: ResourceTypeId::new(&r.ty).map_err(|e| corrupt("definition", e))?,
            driver_type: r.driver_type,
            driver_account: r.driver_account,
            driver_inputs: serde_json::from_str(&r.driver_inputs)?,
            provision: serde_json::from_str(&r.provision)?,
            criteria,
        },
        version: r.version,
        created_at: r.created_at,
        created_by: r.created_by,
        updated_at: r.updated_at,
    })
}

async fn definition_exists(conn: &mut SqliteConnection, org: &str, id: &str) -> Result<String> {
    let ty: Option<String> =
        sqlx::query_scalar("SELECT type FROM definitions WHERE org_id = ? AND id = ?")
            .bind(org)
            .bind(id)
            .fetch_optional(&mut *conn)
            .await?;
    ty.ok_or_else(|| StoreError::NotFound(format!("resource definition `{id}`")))
}

/// Up to five definitions matching `filter`, for in-use errors.
pub(crate) async fn definitions_using(
    conn: &mut SqliteConnection,
    org: &str,
    column: &str,
    value: &str,
) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar(&format!(
        "SELECT id FROM definitions WHERE org_id = ? AND {column} = ? ORDER BY id LIMIT 5"
    ))
    .bind(org)
    .bind(value)
    .fetch_all(&mut *conn)
    .await?)
}

impl CatalogStore for SqliteStore {
    async fn custom_types(&self, org: &str) -> Result<Vec<ResourceType>> {
        self.check_org(org)?;
        let bodies: Vec<String> =
            sqlx::query_scalar("SELECT body FROM resource_types WHERE org_id = ? ORDER BY id")
                .bind(org)
                .fetch_all(self.pool())
                .await?;
        bodies
            .iter()
            .map(|b| serde_json::from_str(b).map_err(Into::into))
            .collect()
    }

    async fn custom_type(&self, org: &str, id: &str) -> Result<Option<ResourceType>> {
        self.check_org(org)?;
        let body: Option<String> =
            sqlx::query_scalar("SELECT body FROM resource_types WHERE org_id = ? AND id = ?")
                .bind(org)
                .bind(id)
                .fetch_optional(self.pool())
                .await?;
        body.map(|b| serde_json::from_str(&b).map_err(Into::into))
            .transpose()
    }

    async fn create_type(&self, org: &str, ty: &ResourceType, audit: AuditIntent) -> Result<()> {
        self.check_org(org)?;
        let id = ty.id.as_str();
        if builtin_type(id).is_some() {
            return Err(StoreError::AlreadyExists(format!("built-in type `{id}`")));
        }
        let mut tx = self.begin().await?;
        check_admin_authority(&mut tx, org, audit.actor.as_deref()).await?;
        let res = sqlx::query(
            "INSERT INTO resource_types (org_id, id, body, created_at, created_by) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(org)
        .bind(id)
        .bind(serde_json::to_string(ty)?)
        .bind(now_ms())
        .bind(&audit.actor)
        .execute(&mut *tx)
        .await;
        if let Err(e) = &res
            && unique_violation(e)
        {
            return Err(StoreError::AlreadyExists(format!("resource type `{id}`")));
        }
        res?;
        audit::insert(&mut tx, &audit, None, None).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn update_type(&self, org: &str, ty: &ResourceType, audit: AuditIntent) -> Result<()> {
        self.check_org(org)?;
        let id = ty.id.as_str();
        if builtin_type(id).is_some() {
            return Err(StoreError::Conflict(format!(
                "built-in type `{id}` cannot be modified"
            )));
        }
        let mut tx = self.begin().await?;
        check_admin_authority(&mut tx, org, audit.actor.as_deref()).await?;
        let updated = sqlx::query("UPDATE resource_types SET body = ? WHERE org_id = ? AND id = ?")
            .bind(serde_json::to_string(ty)?)
            .bind(org)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        if updated.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("resource type `{id}`")));
        }
        audit::insert(&mut tx, &audit, None, None).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn delete_type(&self, org: &str, id: &str, audit: AuditIntent) -> Result<()> {
        self.check_org(org)?;
        if builtin_type(id).is_some() {
            return Err(StoreError::Conflict(format!(
                "built-in type `{id}` cannot be deleted"
            )));
        }
        let mut tx = self.begin().await?;
        check_admin_authority(&mut tx, org, audit.actor.as_deref()).await?;
        if !type_exists(&mut tx, org, id).await? {
            return Err(StoreError::NotFound(format!("resource type `{id}`")));
        }
        let users = definitions_using(&mut tx, org, "type", id).await?;
        if !users.is_empty() {
            return Err(StoreError::Conflict(format!(
                "resource type `{id}` is used by definitions {}",
                users.join(", ")
            )));
        }
        let users = definitions_provisioning(&mut tx, org, id).await?;
        if !users.is_empty() {
            return Err(StoreError::Conflict(format!(
                "resource type `{id}` is co-provisioned by definitions {}",
                users.join(", ")
            )));
        }
        sqlx::query("DELETE FROM resource_classes WHERE org_id = ? AND resource_type = ?")
            .bind(org)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM resource_types WHERE org_id = ? AND id = ?")
            .bind(org)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        audit::insert(&mut tx, &audit, None, None).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn classes(&self, org: &str, ty: Option<&str>) -> Result<Vec<ResourceClass>> {
        self.check_org(org)?;
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT resource_type, id, description FROM resource_classes \
             WHERE org_id = ? AND (? IS NULL OR resource_type = ?) ORDER BY resource_type, id",
        )
        .bind(org)
        .bind(ty)
        .bind(ty)
        .fetch_all(self.pool())
        .await?;
        rows.into_iter()
            .map(|(ty, id, description)| {
                Ok(ResourceClass {
                    id: ClassId::new(id).map_err(|e| corrupt("class", e))?,
                    resource_type: ResourceTypeId::new(ty).map_err(|e| corrupt("class", e))?,
                    description,
                })
            })
            .collect()
    }

    async fn create_class(
        &self,
        org: &str,
        class: &ResourceClass,
        audit: AuditIntent,
    ) -> Result<()> {
        self.check_org(org)?;
        let ty = class.resource_type.as_str();
        if class.id.is_default() {
            return Err(StoreError::AlreadyExists(format!(
                "class `default` of type `{ty}`"
            )));
        }
        let mut tx = self.begin().await?;
        check_admin_authority(&mut tx, org, audit.actor.as_deref()).await?;
        if !type_exists(&mut tx, org, ty).await? {
            return Err(StoreError::NotFound(format!("resource type `{ty}`")));
        }
        let res = sqlx::query(
            "INSERT INTO resource_classes (org_id, resource_type, id, description, created_at, created_by) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(org)
        .bind(ty)
        .bind(class.id.as_str())
        .bind(&class.description)
        .bind(now_ms())
        .bind(&audit.actor)
        .execute(&mut *tx)
        .await;
        if let Err(e) = &res
            && unique_violation(e)
        {
            return Err(StoreError::AlreadyExists(format!(
                "class `{}` of type `{ty}`",
                class.id
            )));
        }
        res?;
        audit::insert(&mut tx, &audit, None, None).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn delete_class(&self, org: &str, ty: &str, id: &str, audit: AuditIntent) -> Result<()> {
        self.check_org(org)?;
        if id == "default" {
            return Err(StoreError::Conflict(
                "class `default` exists for every type and cannot be deleted".to_owned(),
            ));
        }
        let mut tx = self.begin().await?;
        check_admin_authority(&mut tx, org, audit.actor.as_deref()).await?;
        let users: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT d.id FROM criteria c JOIN definitions d \
             ON d.org_id = c.org_id AND d.id = c.definition_id \
             WHERE c.org_id = ? AND d.type = ? AND c.class = ? ORDER BY d.id LIMIT 5",
        )
        .bind(org)
        .bind(ty)
        .bind(id)
        .fetch_all(&mut *tx)
        .await?;
        if !users.is_empty() {
            return Err(StoreError::Conflict(format!(
                "class `{id}` of type `{ty}` is referenced by the criteria of {}",
                users.join(", ")
            )));
        }
        let deleted = sqlx::query(
            "DELETE FROM resource_classes WHERE org_id = ? AND resource_type = ? AND id = ?",
        )
        .bind(org)
        .bind(ty)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        if deleted.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("class `{id}` of type `{ty}`")));
        }
        audit::insert(&mut tx, &audit, None, None).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn drivers(&self, org: &str) -> Result<Vec<DriverRecord>> {
        self.check_org(org)?;
        let bodies: Vec<String> =
            sqlx::query_scalar("SELECT body FROM drivers WHERE org_id = ? ORDER BY id")
                .bind(org)
                .fetch_all(self.pool())
                .await?;
        bodies
            .iter()
            .map(|b| serde_json::from_str(b).map_err(Into::into))
            .collect()
    }

    async fn driver(&self, org: &str, id: &str) -> Result<Option<DriverRecord>> {
        self.check_org(org)?;
        let body: Option<String> =
            sqlx::query_scalar("SELECT body FROM drivers WHERE org_id = ? AND id = ?")
                .bind(org)
                .bind(id)
                .fetch_optional(self.pool())
                .await?;
        body.map(|b| serde_json::from_str(&b).map_err(Into::into))
            .transpose()
    }

    async fn create_driver(
        &self,
        org: &str,
        driver: &DriverRecord,
        audit: AuditIntent,
    ) -> Result<()> {
        self.check_org(org)?;
        let id = driver.id.as_str();
        let mut tx = self.begin().await?;
        check_admin_authority(&mut tx, org, audit.actor.as_deref()).await?;
        check_driver_record(&mut tx, org, driver, DriverChange::Create).await?;
        let res = sqlx::query(
            "INSERT INTO drivers (org_id, id, body, created_at, created_by) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(org)
        .bind(id)
        .bind(serde_json::to_string(driver)?)
        .bind(now_ms())
        .bind(&audit.actor)
        .execute(&mut *tx)
        .await;
        if let Err(e) = &res
            && unique_violation(e)
        {
            return Err(StoreError::AlreadyExists(format!("driver `{id}`")));
        }
        res?;
        audit::insert(&mut tx, &audit, None, None).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn replace_driver(
        &self,
        org: &str,
        driver: &DriverRecord,
        based_on: &DriverRecord,
        audit: AuditIntent,
    ) -> Result<()> {
        self.check_org(org)?;
        let id = driver.id.as_str();
        let mut tx = self.begin().await?;
        check_admin_authority(&mut tx, org, audit.actor.as_deref()).await?;
        check_driver_record(&mut tx, org, driver, DriverChange::Replace { based_on }).await?;
        let updated = sqlx::query("UPDATE drivers SET body = ? WHERE org_id = ? AND id = ?")
            .bind(serde_json::to_string(driver)?)
            .bind(org)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        if updated.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("driver `{id}`")));
        }
        audit::insert(&mut tx, &audit, None, None).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn check_driver(
        &self,
        org: &str,
        driver: &DriverRecord,
        change: DriverChange<'_>,
        actor: Option<&str>,
    ) -> Result<()> {
        self.check_org(org)?;
        let mut conn = self.pool().acquire().await?;
        check_admin_authority(&mut conn, org, actor).await?;
        check_driver_record(&mut conn, org, driver, change).await
    }

    async fn delete_driver(&self, org: &str, id: &str, audit: AuditIntent) -> Result<()> {
        self.check_org(org)?;
        if BUILTIN_DRIVERS.contains(&id) {
            return Err(StoreError::Conflict(format!(
                "built-in driver `{id}` cannot be deleted"
            )));
        }
        let mut tx = self.begin().await?;
        check_admin_authority(&mut tx, org, audit.actor.as_deref()).await?;
        let users = definitions_using(&mut tx, org, "driver_type", id).await?;
        if !users.is_empty() {
            return Err(StoreError::Conflict(format!(
                "driver `{id}` is used by definitions {}",
                users.join(", ")
            )));
        }
        // Active resources need their driver to be observed, recovered and
        // deprovisioned, even after their definition moved to another driver.
        let envs: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT app_id || '/' || env_id FROM active_resources \
             WHERE org_id = ? AND driver_type = ? ORDER BY 1 LIMIT 5",
        )
        .bind(org)
        .bind(id)
        .fetch_all(&mut *tx)
        .await?;
        if !envs.is_empty() {
            return Err(StoreError::Conflict(format!(
                "driver `{id}` provisioned active resources in {}",
                envs.join(", ")
            )));
        }
        check_no_open_operations(&mut tx, org, id).await?;
        let deleted = sqlx::query("DELETE FROM drivers WHERE org_id = ? AND id = ?")
            .bind(org)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        if deleted.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("driver `{id}`")));
        }
        audit::insert(&mut tx, &audit, None, None).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn definitions(&self, org: &str) -> Result<Vec<DefinitionRecord>> {
        self.check_org(org)?;
        let mut conn = self.pool().acquire().await?;
        let rows: Vec<DefinitionRow> = sqlx::query_as(&format!(
            "SELECT {DEFINITION_COLUMNS} FROM definitions WHERE org_id = ? ORDER BY id"
        ))
        .bind(org)
        .fetch_all(&mut *conn)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            out.push(definition_from_row(&mut conn, org, r).await?);
        }
        Ok(out)
    }

    async fn definition(&self, org: &str, id: &str) -> Result<Option<DefinitionRecord>> {
        self.check_org(org)?;
        let mut conn = self.pool().acquire().await?;
        let row: Option<DefinitionRow> = sqlx::query_as(&format!(
            "SELECT {DEFINITION_COLUMNS} FROM definitions WHERE org_id = ? AND id = ?"
        ))
        .bind(org)
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
        match row {
            Some(r) => Ok(Some(definition_from_row(&mut conn, org, r).await?)),
            None => Ok(None),
        }
    }

    async fn check_definition(
        &self,
        org: &str,
        def: &ResourceDefinition,
        actor: Option<&str>,
    ) -> Result<()> {
        self.check_org(org)?;
        let mut conn = self.pool().acquire().await?;
        check_admin_authority(&mut conn, org, actor).await?;
        check_definition_refs(&mut conn, org, def).await
    }

    async fn create_definition(
        &self,
        org: &str,
        def: &ResourceDefinition,
        audit: AuditIntent,
    ) -> Result<()> {
        self.check_org(org)?;
        let id = def.id.as_str();
        let ty = def.ty.as_str();
        let now = now_ms();
        let mut tx = self.begin().await?;
        check_admin_authority(&mut tx, org, audit.actor.as_deref()).await?;
        check_definition_refs(&mut tx, org, def).await?;
        let res = sqlx::query(
            "INSERT INTO definitions (org_id, id, name, type, driver_type, driver_account, \
             driver_inputs, provision, version, created_at, created_by, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, 1, ?, ?, ?)",
        )
        .bind(org)
        .bind(id)
        .bind(&def.name)
        .bind(ty)
        .bind(&def.driver_type)
        .bind(&def.driver_account)
        .bind(serde_json::to_string(&def.driver_inputs)?)
        .bind(serde_json::to_string(&def.provision)?)
        .bind(now)
        .bind(&audit.actor)
        .bind(now)
        .execute(&mut *tx)
        .await;
        if let Err(e) = &res
            && unique_violation(e)
        {
            return Err(StoreError::AlreadyExists(format!(
                "resource definition `{id}`"
            )));
        }
        res?;
        for criterion in &def.criteria {
            insert_criterion(&mut tx, org, id, criterion).await?;
        }
        audit::insert(&mut tx, &audit, None, None).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn update_definition(
        &self,
        org: &str,
        id: &str,
        update: &DefinitionUpdate,
        audit: AuditIntent,
    ) -> Result<()> {
        self.check_org(org)?;
        let mut tx = self.begin().await?;
        check_admin_authority(&mut tx, org, audit.actor.as_deref()).await?;
        let ty = definition_exists(&mut tx, org, id).await?;
        check_driver_and_account(
            &mut tx,
            org,
            &ty,
            &update.driver_type,
            update.driver_account.as_deref(),
        )
        .await?;
        check_secret_stores(
            &mut tx,
            org,
            SecretOwner::Definition(id),
            &update.driver_inputs,
        )
        .await?;
        check_provision(&mut tx, org, &update.provision).await?;
        sqlx::query(
            "UPDATE definitions SET name = ?, driver_type = ?, driver_account = ?, \
             driver_inputs = ?, provision = ?, version = version + 1, updated_at = ? \
             WHERE org_id = ? AND id = ?",
        )
        .bind(&update.name)
        .bind(&update.driver_type)
        .bind(&update.driver_account)
        .bind(serde_json::to_string(&update.driver_inputs)?)
        .bind(serde_json::to_string(&update.provision)?)
        .bind(now_ms())
        .bind(org)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        audit::insert(&mut tx, &audit, None, None).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn delete_definition(&self, org: &str, id: &str, audit: AuditIntent) -> Result<()> {
        self.check_org(org)?;
        let mut tx = self.begin().await?;
        check_admin_authority(&mut tx, org, audit.actor.as_deref()).await?;
        definition_exists(&mut tx, org, id).await?;
        let users: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT app_id || '/' || env_id FROM active_resources \
             WHERE org_id = ? AND definition_id = ? ORDER BY 1 LIMIT 5",
        )
        .bind(org)
        .bind(id)
        .fetch_all(&mut *tx)
        .await?;
        if !users.is_empty() {
            return Err(StoreError::Conflict(format!(
                "resource definition `{id}` provisioned active resources in {}",
                users.join(", ")
            )));
        }
        sqlx::query("DELETE FROM criteria WHERE org_id = ? AND definition_id = ?")
            .bind(org)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM definitions WHERE org_id = ? AND id = ?")
            .bind(org)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        audit::insert(&mut tx, &audit, None, None).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn add_criterion(
        &self,
        org: &str,
        def: &str,
        criterion: &Criterion,
        audit: AuditIntent,
    ) -> Result<CriterionId> {
        self.check_org(org)?;
        let mut tx = self.begin().await?;
        check_admin_authority(&mut tx, org, audit.actor.as_deref()).await?;
        let ty = definition_exists(&mut tx, org, def).await?;
        let existing = criteria_of(&mut tx, org, def).await?;
        check_criterion(&mut tx, org, &ty, criterion, &existing).await?;
        let id = insert_criterion(&mut tx, org, def, criterion).await?;
        audit::insert(&mut tx, &audit, None, None).await?;
        tx.commit().await?;
        Ok(id)
    }

    async fn delete_criterion(
        &self,
        org: &str,
        def: &str,
        id: &str,
        audit: AuditIntent,
    ) -> Result<()> {
        self.check_org(org)?;
        let mut tx = self.begin().await?;
        check_admin_authority(&mut tx, org, audit.actor.as_deref()).await?;
        definition_exists(&mut tx, org, def).await?;
        let deleted =
            sqlx::query("DELETE FROM criteria WHERE org_id = ? AND definition_id = ? AND id = ?")
                .bind(org)
                .bind(def)
                .bind(id)
                .execute(&mut *tx)
                .await?;
        if deleted.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!(
                "criterion `{id}` of definition `{def}`"
            )));
        }
        audit::insert(&mut tx, &audit, None, None).await?;
        tx.commit().await?;
        Ok(())
    }
}
