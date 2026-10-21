//! The resource catalog: resource types, classes, the driver registry, resource
//! definitions and their criteria. Members read it; administrators change it.
//!
//! Custom type and driver IDs contain a `/`, so clients percent-encode it in paths
//! (`acme%2Ffeature-flags`).

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use openporch_core::catalog::{
    BUILTIN_DRIVER_NAMESPACE, BUILTIN_DRIVERS, ResourceClass, ResourceType, ResourceUse,
    builtin_type, builtin_types,
};
use openporch_core::ids::{self, ClassId, ResourceTypeId};
use openporch_core::model::{Criterion, DriverInputs, Provision, ResourceDefinition, SecretRef};
use openporch_store::{
    CatalogStore, DefinitionRecord, DefinitionUpdate, DriverCapabilities, DriverRecord,
};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::check_id;
use crate::api::{Ctx, JsonBody};
use crate::error::{ApiError, ApiResult};
use crate::secret_inputs::{self, PlannedInputs, PlannedSecret};
use crate::state::AppState;

// Resource types ------------------------------------------------------------------

async fn type_or_404(state: &AppState, id: &str) -> ApiResult<ResourceType> {
    if let Some(t) = builtin_type(id) {
        return Ok(t);
    }
    state
        .store
        .custom_type(state.org(), id)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("resource type `{id}` not found")))
}

pub async fn types(State(state): State<AppState>) -> ApiResult<Json<Vec<ResourceType>>> {
    let mut all = builtin_types();
    all.extend(state.store.custom_types(state.org()).await?);
    Ok(Json(all))
}

pub async fn get_type(
    State(state): State<AppState>,
    Path((_, id)): Path<(String, String)>,
) -> ApiResult<Json<ResourceType>> {
    Ok(Json(type_or_404(&state, &id).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TypeBody {
    /// Required on create; must match the path on update.
    id: Option<String>,
    name: Option<String>,
    #[serde(rename = "use")]
    use_: ResourceUse,
    category: Option<String>,
    inputs_schema: Option<Value>,
    outputs_schema: Option<Value>,
}

/// Builds a custom type from a request body, checking the ID and the schemas.
fn custom_type(org: &str, id: &str, body: TypeBody) -> ApiResult<ResourceType> {
    let parsed = ResourceTypeId::new(id).map_err(|e| ApiError::invalid_id(e.to_string()))?;
    if builtin_type(id).is_some() {
        return Err(ApiError::conflict(format!(
            "built-in type `{id}` cannot be created or modified"
        )));
    }
    if !id
        .strip_prefix(org)
        .is_some_and(|rest| rest.starts_with('/'))
    {
        return Err(ApiError::invalid_id(format!(
            "custom type IDs are `{org}/{{name}}`, not `{id}`"
        )));
    }
    let inputs_schema = body
        .inputs_schema
        .unwrap_or_else(|| json!({"type": "object"}));
    if !inputs_schema.is_object() {
        return Err(ApiError::invalid("`inputs_schema` must be a JSON object"));
    }
    let outputs_schema = body.outputs_schema.unwrap_or_else(|| {
        json!({
            "type": "object",
            "properties": {"values": {"type": "object"}, "secrets": {"type": "object"}},
        })
    });
    let sections_ok = outputs_schema.as_object().is_some_and(|o| {
        o.get("properties").is_none_or(|p| {
            p.as_object().is_some_and(|p| {
                p.iter()
                    .all(|(k, v)| (k == "values" || k == "secrets") && v.is_object())
            })
        })
    });
    if !sections_ok {
        return Err(ApiError::invalid(
            "`outputs_schema` must be a JSON object whose properties are `values` and `secrets` schemas",
        ));
    }
    Ok(ResourceType {
        id: parsed,
        name: body.name.unwrap_or_else(|| id.to_owned()),
        use_: body.use_,
        category: body.category.unwrap_or_default(),
        inputs_schema,
        outputs_schema,
    })
}

pub async fn create_type(
    State(state): State<AppState>,
    ctx: Ctx,
    JsonBody(mut body): JsonBody<TypeBody>,
) -> ApiResult<(StatusCode, Json<ResourceType>)> {
    let org = state.org();
    let id = body
        .id
        .take()
        .ok_or_else(|| ApiError::invalid("`id` is required"))?;
    let ty = custom_type(org, &id, body)?;
    state
        .store
        .create_type(org, &ty, ctx.audit(org, "resource_type.create", &id))
        .await?;
    Ok((StatusCode::CREATED, Json(ty)))
}

pub async fn update_type(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, id)): Path<(String, String)>,
    JsonBody(mut body): JsonBody<TypeBody>,
) -> ApiResult<Json<ResourceType>> {
    let org = state.org();
    if body.id.take().is_some_and(|b| b != id) {
        return Err(ApiError::invalid("`id` cannot be changed"));
    }
    let ty = custom_type(org, &id, body)?;
    state
        .store
        .update_type(org, &ty, ctx.audit(org, "resource_type.update", &id))
        .await?;
    Ok(Json(ty))
}

pub async fn delete_type(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, id)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let org = state.org();
    state
        .store
        .delete_type(org, &id, ctx.audit(org, "resource_type.delete", &id))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

// Resource classes ----------------------------------------------------------------

pub async fn all_classes(State(state): State<AppState>) -> ApiResult<Json<Vec<ResourceClass>>> {
    Ok(Json(state.store.classes(state.org(), None).await?))
}

/// The classes of one type, starting with the implicit `default`.
pub async fn classes(
    State(state): State<AppState>,
    Path((_, ty)): Path<(String, String)>,
) -> ApiResult<Json<Vec<ResourceClass>>> {
    let t = type_or_404(&state, &ty).await?;
    let mut out = vec![ResourceClass {
        id: ClassId::default_class(),
        resource_type: t.id,
        description: "The default class, which exists for every type.".to_owned(),
    }];
    out.extend(state.store.classes(state.org(), Some(&ty)).await?);
    Ok(Json(out))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClassBody {
    id: String,
    #[serde(default)]
    description: String,
}

pub async fn create_class(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, ty)): Path<(String, String)>,
    JsonBody(body): JsonBody<ClassBody>,
) -> ApiResult<(StatusCode, Json<ResourceClass>)> {
    let org = state.org();
    let class = ResourceClass {
        id: ClassId::new(&body.id).map_err(|e| ApiError::invalid_id(e.to_string()))?,
        resource_type: type_or_404(&state, &ty).await?.id,
        description: body.description,
    };
    state
        .store
        .create_class(
            org,
            &class,
            ctx.audit(org, "resource_class.create", format!("{ty}/{}", body.id)),
        )
        .await?;
    Ok((StatusCode::CREATED, Json(class)))
}

pub async fn delete_class(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, ty, class)): Path<(String, String, String)>,
) -> ApiResult<StatusCode> {
    let org = state.org();
    state
        .store
        .delete_class(
            org,
            &ty,
            &class,
            ctx.audit(org, "resource_class.delete", format!("{ty}/{class}")),
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

// Drivers -------------------------------------------------------------------------

/// A driver as listed: built-in or registered.
#[derive(Serialize)]
pub struct DriverView {
    id: String,
    builtin: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    inputs_schema: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    token: Option<SecretRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    protocol_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cookie_version: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    outputs_schema: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    capabilities: Option<DriverCapabilities>,
}

fn builtin_driver(id: &str) -> DriverView {
    DriverView {
        id: id.to_owned(),
        builtin: true,
        target: None,
        inputs_schema: None,
        token: None,
        protocol_version: None,
        cookie_version: None,
        outputs_schema: None,
        capabilities: None,
    }
}

impl From<DriverRecord> for DriverView {
    fn from(d: DriverRecord) -> Self {
        Self {
            id: d.id,
            builtin: false,
            target: Some(d.target),
            inputs_schema: d.inputs_schema,
            token: d.token,
            protocol_version: Some(d.protocol_version),
            cookie_version: Some(d.cookie_version),
            outputs_schema: d.outputs_schema,
            capabilities: Some(d.capabilities),
        }
    }
}

pub async fn drivers(State(state): State<AppState>) -> ApiResult<Json<Vec<DriverView>>> {
    let mut out: Vec<DriverView> = BUILTIN_DRIVERS.iter().map(|d| builtin_driver(d)).collect();
    out.extend(
        state
            .store
            .drivers(state.org())
            .await?
            .into_iter()
            .map(DriverView::from),
    );
    Ok(Json(out))
}

pub async fn get_driver(
    State(state): State<AppState>,
    Path((_, id)): Path<(String, String)>,
) -> ApiResult<Json<DriverView>> {
    if BUILTIN_DRIVERS.contains(&id.as_str()) {
        return Ok(Json(builtin_driver(&id)));
    }
    let d = state
        .store
        .driver(state.org(), &id)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("driver `{id}` not found")))?;
    Ok(Json(d.into()))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DriverBody {
    /// Required on create; must match the path on replace.
    id: Option<String>,
    target: String,
    inputs_schema: Option<Value>,
    /// The bearer token sent to the driver: a reference or `{value}`.
    token: Option<Value>,
    /// The protocol the driver speaks; `openporch-http/1` when left out.
    protocol_version: Option<String>,
    /// The cookie format version the driver writes; `1` when left out.
    cookie_version: Option<u32>,
    outputs_schema: Option<Value>,
    /// The optional capabilities; none when left out.
    #[serde(default)]
    capabilities: DriverCapabilities,
}

/// Checks a registration body for driver `id` and plans its token. `current` is
/// the token the registration holds now, which a replacement may keep; no other
/// secret under `drivers/{id}/` may be referenced. Error messages never show the
/// token's value.
fn plan_driver(
    id: &str,
    body: DriverBody,
    current: Option<&SecretRef>,
) -> ApiResult<(DriverRecord, Option<PlannedSecret>)> {
    let valid = id
        .split_once('/')
        .is_some_and(|(ns, name)| ids::is_valid_object_id(ns) && ids::is_valid_object_id(name));
    if !valid {
        return Err(ApiError::invalid_id(format!(
            "driver ID `{id}` is invalid: driver IDs are `<namespace>/<name>`; {}",
            ids::OBJECT_ID_RULE
        )));
    }
    if id.starts_with(&format!("{BUILTIN_DRIVER_NAMESPACE}/")) {
        return Err(ApiError::invalid_id(format!(
            "the `{BUILTIN_DRIVER_NAMESPACE}` driver namespace is reserved for built-in drivers"
        )));
    }
    let target = Url::parse(&body.target)
        .map_err(|e| ApiError::invalid(format!("`target` is not a URL: {e}")))?;
    if target.scheme() != "https" || target.host_str().is_none() {
        return Err(ApiError::invalid("`target` must be an https URL"));
    }
    if !target.username().is_empty() || target.password().is_some() {
        return Err(ApiError::invalid(
            "`target` must not contain credentials; use `token`",
        ));
    }
    // The adapter appends the GUResID to the path, so it must accept the target.
    openporch_drivers::check_target(&body.target)
        .map_err(|e| ApiError::invalid(format!("`target`: {e}")))?;
    if body.inputs_schema.as_ref().is_some_and(|s| !s.is_object()) {
        return Err(ApiError::invalid("`inputs_schema` must be a JSON object"));
    }
    if body.outputs_schema.as_ref().is_some_and(|s| !s.is_object()) {
        return Err(ApiError::invalid("`outputs_schema` must be a JSON object"));
    }
    let protocol_version = body
        .protocol_version
        .unwrap_or_else(|| openporch_drivers::PROTOCOL_VERSION.to_owned());
    if protocol_version != openporch_drivers::PROTOCOL_VERSION {
        return Err(ApiError::invalid(format!(
            "`protocol_version` `{protocol_version}` is not supported; this server speaks `{}`",
            openporch_drivers::PROTOCOL_VERSION
        )));
    }
    // The protocol has no preview call, so a driver claiming one would make
    // previews promise what deployments cannot check.
    if body.capabilities.preview {
        return Err(ApiError::invalid(format!(
            "`capabilities.preview` must be false: `{}` has no preview call",
            openporch_drivers::PROTOCOL_VERSION
        )));
    }
    let token = body
        .token
        .as_ref()
        .map(|t| PlannedSecret::new("`token`", t))
        .transpose()?;
    // A driver's own secrets are written by its registration. One left by an
    // earlier registration under the same ID belongs to that driver, and must
    // not reach a new target without being entered again. A replacement may
    // keep the token it holds, exactly as it is.
    if let Some(r) = token.as_ref().and_then(|t| t.reference())
        && r.store == openporch_store::secrets::BUILTIN_STORE
        && r.reference.starts_with(&format!("drivers/{id}/"))
        && current != Some(r)
    {
        return Err(ApiError::invalid(
            "`token` must not reference this driver's own secrets other than the token it \
             holds now; give the token as `{value}`",
        ));
    }
    let record = DriverRecord {
        id: id.to_owned(),
        target: target.to_string(),
        inputs_schema: body.inputs_schema,
        token: token.as_ref().and_then(|t| t.reference().cloned()),
        protocol_version,
        cookie_version: body.cookie_version.unwrap_or(1),
        outputs_schema: body.outputs_schema,
        capabilities: body.capabilities,
    };
    Ok((record, token))
}

/// Writes a `{value}` token as a new version of `drivers/{id}/token` and creates
/// or replaces the registration. Everything is checked before the token is
/// written; the store checks again with the change, and a token written for a
/// rejected registration is discarded.
async fn write_driver(
    state: &AppState,
    ctx: &Ctx,
    mut record: DriverRecord,
    token: Option<PlannedSecret>,
    create: bool,
) -> ApiResult<DriverRecord> {
    let org = state.org();
    let id = record.id.clone();
    state
        .store
        .check_driver(org, &record, create, Some(&ctx.principal.id))
        .await?;
    let primary = secret_inputs::primary(state)?;
    let mut written = Vec::new();
    let result = async {
        if let Some(t) = token {
            let name = format!("drivers/{id}/token");
            record.token = Some(t.materialize(&primary, &name, &mut written).await?);
        }
        if create {
            let audit = ctx.audit(org, "driver.create", &id);
            state.store.create_driver(org, &record, audit).await?;
        } else {
            let audit = ctx.audit(org, "driver.update", &id);
            state.store.replace_driver(org, &record, audit).await?;
        }
        Ok::<_, ApiError>(())
    }
    .await;
    if let Err(e) = result {
        secret_inputs::discard(&primary, written).await;
        return Err(e);
    }
    Ok(record)
}

/// Registers an external driver. The ID is `<namespace>/<name>`, `openporch` is
/// reserved and the target must be an HTTPS URL without credentials. The
/// registration pins the protocol and cookie versions and the capabilities.
pub async fn create_driver(
    State(state): State<AppState>,
    ctx: Ctx,
    JsonBody(body): JsonBody<Value>,
) -> ApiResult<(StatusCode, Json<DriverView>)> {
    let mut body: DriverBody = secret_inputs::parse_body(body)?;
    let id = body
        .id
        .take()
        .ok_or_else(|| ApiError::invalid("`id` is required"))?;
    let (record, token) = plan_driver(&id, body, None)?;
    let record = write_driver(&state, &ctx, record, token, true).await?;
    Ok((StatusCode::CREATED, Json(record.into())))
}

/// Replaces an external driver's registration: its target, token, pins and
/// capabilities, checked like a new one. The token it holds is kept when given
/// back as its reference, and `{value}` writes a new version. Refused while an
/// operation the driver ran has an outcome that is not known yet, or when the
/// driver would no longer target the type of a definition using it.
pub async fn replace_driver(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, id)): Path<(String, String)>,
    JsonBody(body): JsonBody<Value>,
) -> ApiResult<Json<DriverView>> {
    let mut body: DriverBody = secret_inputs::parse_body(body)?;
    if BUILTIN_DRIVERS.contains(&id.as_str()) {
        return Err(ApiError::conflict(format!(
            "built-in driver `{id}` cannot be modified"
        )));
    }
    if body.id.take().is_some_and(|b| b != id) {
        return Err(ApiError::invalid("`id` cannot be changed"));
    }
    let current = state
        .store
        .driver(state.org(), &id)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("driver `{id}` not found")))?;
    let (record, token) = plan_driver(&id, body, current.token.as_ref())?;
    let record = write_driver(&state, &ctx, record, token, false).await?;
    Ok(Json(record.into()))
}

pub async fn delete_driver(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, id)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let org = state.org();
    state
        .store
        .delete_driver(org, &id, ctx.audit(org, "driver.delete", &id))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

// Definitions ---------------------------------------------------------------------

pub(super) async fn definition_or_404(state: &AppState, id: &str) -> ApiResult<DefinitionRecord> {
    state
        .store
        .definition(state.org(), id)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("resource definition `{id}` not found")))
}

pub async fn definitions(State(state): State<AppState>) -> ApiResult<Json<Vec<DefinitionRecord>>> {
    Ok(Json(state.store.definitions(state.org()).await?))
}

pub async fn get_definition(
    State(state): State<AppState>,
    Path((_, id)): Path<(String, String)>,
) -> ApiResult<Json<DefinitionRecord>> {
    Ok(Json(definition_or_404(&state, &id).await?))
}

/// Checks a built-in driver's inputs, so mistakes fail with `400` when the
/// definition is written rather than at deployment.
fn check_driver_inputs(driver_type: &str, inputs: &DriverInputs) -> ApiResult<()> {
    for secrets in [&inputs.secret_refs, &inputs.secrets] {
        openporch_drivers::check_definition_inputs(driver_type, &inputs.values, secrets)
            .map_err(|m| ApiError::invalid(format!("`driver_inputs`: {m}")))?;
    }
    Ok(())
}

/// Creates a definition. Inline secrets and `{value}` references are written to the
/// primary store; the stored definition and the response hold references only.
pub async fn create_definition(
    State(state): State<AppState>,
    ctx: Ctx,
    JsonBody(body): JsonBody<Value>,
) -> ApiResult<(StatusCode, Json<DefinitionRecord>)> {
    let mut def: ResourceDefinition = secret_inputs::parse_body(body)?;
    let org = state.org();
    let id = def.id.to_string();
    check_driver_inputs(&def.driver_type, &def.driver_inputs)?;
    let planned = PlannedInputs::new(std::mem::take(&mut def.driver_inputs))?;
    def.driver_inputs = planned.references();
    // Check everything before secrets are written, so a request that will be
    // rejected leaves nothing behind. The store checks again with the change.
    state
        .store
        .check_definition(org, &def, Some(&ctx.principal.id))
        .await?;
    if state.store.definition(org, &id).await?.is_some() {
        return Err(ApiError::conflict(format!(
            "resource definition `{id}` already exists"
        )));
    }
    if def.name.is_empty() {
        def.name.clone_from(&id);
    }
    for c in &mut def.criteria {
        c.id = None;
    }
    // Values are written under the definition; versions written for a rejected
    // change are discarded.
    let primary = secret_inputs::primary(&state)?;
    let mut written = Vec::new();
    let result = async {
        def.driver_inputs = planned
            .write(&primary, &format!("defs/{id}"), &mut written)
            .await?;
        state
            .store
            .create_definition(org, &def, ctx.audit(org, "definition.create", &id))
            .await?;
        Ok::<_, ApiError>(())
    }
    .await;
    if let Err(e) = result {
        secret_inputs::discard(&primary, written).await;
        return Err(e);
    }
    Ok((
        StatusCode::CREATED,
        Json(definition_or_404(&state, &id).await?),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateDefinition {
    #[serde(default)]
    name: Option<String>,
    driver_type: String,
    #[serde(default)]
    driver_account: Option<String>,
    #[serde(default)]
    driver_inputs: DriverInputs,
    #[serde(default)]
    provision: BTreeMap<String, Provision>,
}

/// Replaces a definition's name, driver, account, inputs and co-provisioning. The
/// type and criteria stay; criteria are managed one by one.
pub async fn update_definition(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, id)): Path<(String, String)>,
    JsonBody(body): JsonBody<Value>,
) -> ApiResult<Json<DefinitionRecord>> {
    let body: UpdateDefinition = secret_inputs::parse_body(body)?;
    let org = state.org();
    check_driver_inputs(&body.driver_type, &body.driver_inputs)?;
    let planned = PlannedInputs::new(body.driver_inputs)?;
    let current = definition_or_404(&state, &id).await?.definition;
    let mut update = DefinitionUpdate {
        name: body.name.unwrap_or(current.name),
        driver_type: body.driver_type,
        driver_account: body.driver_account,
        driver_inputs: planned.references(),
        provision: body.provision,
    };
    let check = ResourceDefinition {
        id: current.id,
        name: update.name.clone(),
        ty: current.ty,
        driver_type: update.driver_type.clone(),
        driver_account: update.driver_account.clone(),
        driver_inputs: std::mem::take(&mut update.driver_inputs),
        provision: update.provision.clone(),
        criteria: Vec::new(),
    };
    state
        .store
        .check_definition(org, &check, Some(&ctx.principal.id))
        .await?;
    let primary = secret_inputs::primary(&state)?;
    let mut written = Vec::new();
    let result = async {
        update.driver_inputs = planned
            .write(&primary, &format!("defs/{id}"), &mut written)
            .await?;
        state
            .store
            .update_definition(org, &id, &update, ctx.audit(org, "definition.update", &id))
            .await?;
        Ok::<_, ApiError>(())
    }
    .await;
    if let Err(e) = result {
        secret_inputs::discard(&primary, written).await;
        return Err(e);
    }
    Ok(Json(definition_or_404(&state, &id).await?))
}

pub async fn delete_definition(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, id)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let org = state.org();
    state
        .store
        .delete_definition(org, &id, ctx.audit(org, "definition.delete", &id))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Adds a criterion and returns it with its generated ID.
pub async fn add_criterion(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, def)): Path<(String, String)>,
    JsonBody(mut criterion): JsonBody<Criterion>,
) -> ApiResult<(StatusCode, Json<Criterion>)> {
    let org = state.org();
    check_id("resource definition", &def)?;
    criterion.id = None;
    let id = state
        .store
        .add_criterion(
            org,
            &def,
            &criterion,
            ctx.audit(org, "criterion.create", &def),
        )
        .await?;
    criterion.id = Some(id);
    Ok((StatusCode::CREATED, Json(criterion)))
}

pub async fn delete_criterion(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, def, id)): Path<(String, String, String)>,
) -> ApiResult<StatusCode> {
    let org = state.org();
    state
        .store
        .delete_criterion(
            org,
            &def,
            &id,
            ctx.audit(org, "criterion.delete", format!("{def}/{id}")),
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
