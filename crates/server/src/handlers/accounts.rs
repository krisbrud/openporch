//! Cloud accounts, secret stores and secret-use grants. Members read accounts and
//! stores, administrators change them and manage grants.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use openporch_core::ids::AccountId;
use openporch_core::model::{CloudAccount, CloudAccountType, SecretRef};
use openporch_engine::cloud::CloudError;
use openporch_store::secrets::{BUILTIN_STORE, validate_reference};
use openporch_store::{AccountRecord, AccountStore, GrantRecord, NewGrant, SecretStoreRecord};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::check_id;
use crate::api::{Ctx, JsonBody};
use crate::error::{ApiError, ApiResult, code};
use crate::secret_inputs::{self, PlannedSecret};
use crate::state::AppState;

// Cloud accounts -----------------------------------------------------------------

async fn account_or_404(state: &AppState, id: &str) -> ApiResult<AccountRecord> {
    state
        .store
        .account(state.org(), id)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("cloud account `{id}` not found")))
}

pub async fn accounts(State(state): State<AppState>) -> ApiResult<Json<Vec<AccountRecord>>> {
    Ok(Json(state.store.accounts(state.org()).await?))
}

pub async fn get_account(
    State(state): State<AppState>,
    Path((_, id)): Path<(String, String)>,
) -> ApiResult<Json<AccountRecord>> {
    Ok(Json(account_or_404(&state, &id).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountBody {
    /// Required on create; must match the path on update.
    id: Option<String>,
    name: Option<String>,
    #[serde(rename = "type")]
    ty: CloudAccountType,
    #[serde(default)]
    config: Map<String, Value>,
    /// Secret values, written to the primary store.
    #[serde(default)]
    secrets: Map<String, Value>,
    /// Secret references, or `{value}` to write.
    #[serde(default)]
    secret_refs: Map<String, Value>,
}

/// The configuration and secret keys an account type takes.
struct Shape {
    required: &'static [&'static str],
    optional: &'static [&'static str],
    secrets: &'static [&'static str],
}

fn shape(ty: CloudAccountType) -> ApiResult<Shape> {
    match ty {
        CloudAccountType::Aws => Ok(Shape {
            required: &["access_key_id"],
            optional: &["region"],
            secrets: &["secret_access_key"],
        }),
        CloudAccountType::AwsRole => Ok(Shape {
            required: &["role_arn", "external_id"],
            optional: &["session_duration", "region"],
            secrets: &[],
        }),
        CloudAccountType::AwsIdentity => Err(ApiError::invalid(
            "cloud account type `aws-identity` needs the openporch OIDC provider, which this \
             server does not offer yet; use `aws` or `aws-role`",
        )),
    }
}

/// Checks an account body against its type and plans its secrets. Error messages
/// name keys, never values.
fn plan_account(
    id: &str,
    body: AccountBody,
) -> ApiResult<(CloudAccount, BTreeMap<String, PlannedSecret>)> {
    let account_id = AccountId::new(id).map_err(|e| ApiError::invalid_id(e.to_string()))?;
    let shape = shape(body.ty)?;
    for key in body.config.keys() {
        if !shape.required.contains(&key.as_str()) && !shape.optional.contains(&key.as_str()) {
            return Err(ApiError::invalid(format!(
                "unknown `config.{key}` for this account type; expected {}",
                shape
                    .required
                    .iter()
                    .chain(shape.optional)
                    .map(|k| format!("`{k}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }
    for key in shape.required {
        match body.config.get(*key).and_then(Value::as_str) {
            Some(v) if !v.trim().is_empty() => {}
            _ => {
                return Err(ApiError::invalid(format!(
                    "`config.{key}` is required and must be a non-empty string"
                )));
            }
        }
    }
    for key in ["region", "role_arn", "external_id"] {
        if body.config.get(key).is_some_and(|v| !v.is_string()) {
            return Err(ApiError::invalid(format!(
                "`config.{key}` must be a string"
            )));
        }
    }
    if let Some(arn) = body.config.get("role_arn").and_then(Value::as_str)
        && !arn.starts_with("arn:")
    {
        return Err(ApiError::invalid(
            "`config.role_arn` must be an IAM role ARN",
        ));
    }
    if let Some(d) = body.config.get("session_duration")
        && !d.as_u64().is_some_and(|d| (900..=43_200).contains(&d))
    {
        return Err(ApiError::invalid(
            "`config.session_duration` must be a number of seconds from 900 to 43200",
        ));
    }
    let mut planned = BTreeMap::new();
    for (key, value) in &body.secrets {
        let what = format!("`secrets.{key}`");
        if !value.is_string() {
            return Err(ApiError::invalid(format!("{what} must be a string")));
        }
        planned.insert(
            key.clone(),
            PlannedSecret::new(&what, &json!({"value": value}))?,
        );
    }
    for (key, value) in &body.secret_refs {
        if planned.contains_key(key) {
            return Err(ApiError::invalid(format!(
                "secret `{key}` is given in both `secrets` and `secret_refs`"
            )));
        }
        planned.insert(
            key.clone(),
            PlannedSecret::new(&format!("`secret_refs.{key}`"), value)?,
        );
    }
    for key in planned.keys() {
        if !shape.secrets.contains(&key.as_str()) {
            return Err(ApiError::invalid(format!(
                "unknown secret `{key}` for this account type"
            )));
        }
    }
    for key in shape.secrets {
        if !planned.contains_key(*key) {
            return Err(ApiError::invalid(format!(
                "secret `{key}` is required, in `secrets` or `secret_refs`"
            )));
        }
    }
    let account = CloudAccount {
        id: account_id,
        name: body.name.unwrap_or_else(|| id.to_owned()),
        ty: body.ty,
        config: body.config,
        // The given references, so their stores can be checked before writing.
        secret_refs: planned
            .iter()
            .filter_map(|(k, p)| p.reference().map(|r| (k.clone(), r.clone())))
            .collect(),
    };
    Ok((account, planned))
}

/// Writes planned secrets as `accounts/{id}/{key}` and creates or updates the
/// account; versions written for a rejected change are discarded.
async fn write_account(
    state: &AppState,
    ctx: &Ctx,
    mut account: CloudAccount,
    planned: BTreeMap<String, PlannedSecret>,
    create: bool,
) -> ApiResult<()> {
    let org = state.org();
    let primary = secret_inputs::primary(state)?;
    let mut written = Vec::new();
    let result: ApiResult<()> = async {
        for (key, secret) in planned {
            let name = format!("accounts/{}/{key}", account.id);
            let r = secret.materialize(&primary, &name, &mut written).await?;
            account.secret_refs.insert(key, r);
        }
        let id = account.id.to_string();
        if create {
            let audit = ctx.audit(org, "cloud_account.create", &id);
            state.store.create_account(org, &account, audit).await?;
        } else {
            let audit = ctx.audit(org, "cloud_account.update", &id);
            state.store.update_account(org, &account, audit).await?;
            state.cloud.forget(&id);
        }
        Ok(())
    }
    .await;
    if result.is_err() {
        secret_inputs::discard(&primary, written).await;
    }
    result
}

pub async fn create_account(
    State(state): State<AppState>,
    ctx: Ctx,
    JsonBody(body): JsonBody<Value>,
) -> ApiResult<(StatusCode, Json<AccountRecord>)> {
    let mut body: AccountBody = secret_inputs::parse_body(body)?;
    let org = state.org();
    let id = body
        .id
        .take()
        .ok_or_else(|| ApiError::invalid("`id` is required"))?;
    let (account, planned) = plan_account(&id, body)?;
    state
        .store
        .check_account(org, &account, true, Some(&ctx.principal.id))
        .await?;
    write_account(&state, &ctx, account, planned, true).await?;
    Ok((
        StatusCode::CREATED,
        Json(account_or_404(&state, &id).await?),
    ))
}

pub async fn update_account(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, id)): Path<(String, String)>,
    JsonBody(body): JsonBody<Value>,
) -> ApiResult<Json<AccountRecord>> {
    let mut body: AccountBody = secret_inputs::parse_body(body)?;
    let org = state.org();
    if body.id.take().is_some_and(|b| b != id) {
        return Err(ApiError::invalid("`id` cannot be changed"));
    }
    account_or_404(&state, &id).await?;
    let (account, planned) = plan_account(&id, body)?;
    state
        .store
        .check_account(org, &account, false, Some(&ctx.principal.id))
        .await?;
    write_account(&state, &ctx, account, planned, false).await?;
    Ok(Json(account_or_404(&state, &id).await?))
}

/// Obtains credentials for the account and asks AWS STS who they belong to. Errors
/// carry the STS failure message, never credentials.
pub async fn check_account(
    State(state): State<AppState>,
    Path((_, id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let account = account_or_404(&state, &id).await?.account;
    let primary = secret_inputs::primary(&state)?;
    let identity = state
        .cloud
        .check(&account, &primary)
        .await
        .map_err(|e| match e {
            CloudError::Invalid(m) | CloudError::Expiring(m) => ApiError::invalid(m),
            CloudError::Rejected(m) => {
                ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, code::CLOUD_REJECTED, m)
            }
            CloudError::Unavailable(m) => {
                ApiError::new(StatusCode::BAD_GATEWAY, code::CLOUD_UNAVAILABLE, m)
            }
        })?;
    Ok(Json(json!({
        "arn": identity.arn,
        "account": identity.account,
        "user_id": identity.user_id,
    })))
}

pub async fn delete_account(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, id)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let org = state.org();
    state
        .store
        .delete_account(org, &id, ctx.audit(org, "cloud_account.delete", &id))
        .await?;
    state.cloud.forget(&id);
    Ok(StatusCode::NO_CONTENT)
}

// Secret stores ------------------------------------------------------------------

/// Store types that can be registered. Their adapters arrive in milestone 2, so
/// they cannot be primary yet.
const EXTERNAL_STORE_TYPES: &[&str] = &["aws-secrets-manager", "aws-ssm"];

async fn store_or_404(state: &AppState, id: &str) -> ApiResult<SecretStoreRecord> {
    state
        .store
        .secret_store(state.org(), id)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("secret store `{id}` not found")))
}

pub async fn stores(State(state): State<AppState>) -> ApiResult<Json<Vec<SecretStoreRecord>>> {
    Ok(Json(state.store.secret_stores(state.org()).await?))
}

pub async fn get_store(
    State(state): State<AppState>,
    Path((_, id)): Path<(String, String)>,
) -> ApiResult<Json<SecretStoreRecord>> {
    Ok(Json(store_or_404(&state, &id).await?))
}

/// Checks an external store's configuration: a region and the cloud account to
/// read through, nothing else.
fn check_store_config(config: &Map<String, Value>) -> ApiResult<()> {
    for key in config.keys() {
        if key != "region" && key != "cloud_account" {
            return Err(ApiError::invalid(format!(
                "unknown `config.{key}`; expected `region` and `cloud_account`"
            )));
        }
    }
    for key in ["region", "cloud_account"] {
        match config.get(key).and_then(Value::as_str) {
            Some(v) if !v.trim().is_empty() => {}
            _ => {
                return Err(ApiError::invalid(format!(
                    "`config.{key}` is required and must be a non-empty string"
                )));
            }
        }
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoreBody {
    id: String,
    #[serde(rename = "type")]
    ty: String,
    #[serde(default)]
    config: Map<String, Value>,
}

pub async fn create_store(
    State(state): State<AppState>,
    ctx: Ctx,
    JsonBody(body): JsonBody<StoreBody>,
) -> ApiResult<(StatusCode, Json<SecretStoreRecord>)> {
    let org = state.org();
    check_id("secret store", &body.id)?;
    if body.ty == BUILTIN_STORE {
        return Err(ApiError::conflict(
            "the built-in `openporch` store always exists and cannot be registered again",
        ));
    }
    if !EXTERNAL_STORE_TYPES.contains(&body.ty.as_str()) {
        return Err(ApiError::invalid(format!(
            "unknown secret store type `{}`; expected `aws-secrets-manager` or `aws-ssm`",
            body.ty
        )));
    }
    check_store_config(&body.config)?;
    let record = SecretStoreRecord {
        id: body.id.clone(),
        ty: body.ty,
        config: body.config,
        primary: false,
        created_at: 0,
    };
    state
        .store
        .create_secret_store(
            org,
            &record,
            ctx.audit(org, "secret_store.create", &body.id),
        )
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(store_or_404(&state, &body.id).await?),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorePatch {
    config: Option<Map<String, Value>>,
    primary: Option<bool>,
}

/// Changes a store's configuration, or marks it primary. Only the built-in store
/// can be primary until the external adapters can write secrets.
pub async fn update_store(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, id)): Path<(String, String)>,
    JsonBody(body): JsonBody<StorePatch>,
) -> ApiResult<Json<SecretStoreRecord>> {
    let org = state.org();
    let store = store_or_404(&state, &id).await?;
    match body.primary {
        Some(true) if !store.primary => {
            return Err(ApiError::invalid(format!(
                "secret store `{id}` cannot become primary: this server cannot write \
                 secrets to `{}` stores yet",
                store.ty
            )));
        }
        Some(false) if store.primary => {
            return Err(ApiError::invalid(
                "mark another store primary instead of unmarking the primary store",
            ));
        }
        _ => {}
    }
    if let Some(config) = &body.config {
        check_store_config(config)?;
        state
            .store
            .update_secret_store(org, &id, config, ctx.audit(org, "secret_store.update", &id))
            .await?;
    }
    Ok(Json(store_or_404(&state, &id).await?))
}

pub async fn delete_store(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, id)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let org = state.org();
    state
        .store
        .delete_secret_store(org, &id, ctx.audit(org, "secret_store.delete", &id))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

// Grants -------------------------------------------------------------------------

pub async fn grants(
    State(state): State<AppState>,
    Path((_, store)): Path<(String, String)>,
) -> ApiResult<Json<Vec<GrantRecord>>> {
    store_or_404(&state, &store).await?;
    Ok(Json(state.store.grants(state.org(), &store).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantBody {
    #[serde(rename = "ref")]
    reference: String,
    app_id: Option<String>,
    env_id: Option<String>,
    #[serde(default)]
    org_wide: bool,
}

/// Grants use of one reference to an application, one of its environments, or,
/// explicitly, the whole organization.
pub async fn create_grant(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, store)): Path<(String, String)>,
    JsonBody(body): JsonBody<GrantBody>,
) -> ApiResult<(StatusCode, Json<GrantRecord>)> {
    let org = state.org();
    validate_reference(&SecretRef {
        store: store.clone(),
        reference: body.reference.clone(),
        version: None,
    })
    .map_err(|e| ApiError::invalid(e.to_string()))?;
    match (&body.app_id, &body.env_id, body.org_wide) {
        (None, None, true) | (Some(_), _, false) => {}
        (None, Some(_), _) => {
            return Err(ApiError::invalid("`env_id` needs `app_id`"));
        }
        (Some(_), _, true) => {
            return Err(ApiError::invalid(
                "a grant is either org-wide or for an application, not both",
            ));
        }
        (None, None, false) => {
            return Err(ApiError::invalid(
                "a grant needs `app_id` (and optionally `env_id`) or `org_wide: true`",
            ));
        }
    }
    let grant = NewGrant {
        store: store.clone(),
        reference: body.reference,
        app_id: body.app_id,
        env_id: body.env_id,
        org_wide: body.org_wide,
    };
    let record = state
        .store
        .create_grant(org, &grant, ctx.audit(org, "secret_grant.create", &store))
        .await?;
    Ok((StatusCode::CREATED, Json(record)))
}

pub async fn delete_grant(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, store, id)): Path<(String, String, String)>,
) -> ApiResult<StatusCode> {
    let org = state.org();
    state
        .store
        .delete_grant(
            org,
            &store,
            &id,
            ctx.audit(org, "secret_grant.delete", format!("{store}/{id}")),
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
