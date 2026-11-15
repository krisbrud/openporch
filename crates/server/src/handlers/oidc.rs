//! The OIDC issuer's discovery document and JWKS, served without
//! credentials, signing key rotation for administrators, and how to set up
//! AWS to trust the issuer for an `aws-identity` account.

use axum::Json;
use axum::extract::{Path, State};
use openporch_core::ids::AccountId;
use openporch_core::model::CloudAccountType;
use openporch_engine::cloud::{DEFAULT_AUDIENCE, identity_subject};
use openporch_engine::oidc::KeyInfo;
use openporch_store::{AccountStore, AuditOutcome, IdentityStore, now_ms};
use serde::Serialize;
use serde_json::{Value, json};

use crate::api::Ctx;
use crate::error::{ApiError, ApiResult};
use crate::state::{AppState, OIDC_KEY_PURPOSE, new_oidc_key, open_oidc_keys};

/// The audit action of a key rotation.
pub const ROTATE_ACTION: &str = "oidc.key.rotate";

fn issuer(state: &AppState) -> ApiResult<&openporch_engine::oidc::Issuer> {
    match (&state.issuer, &state.config.public_url) {
        (Some(issuer), _) => Ok(issuer),
        (None, None) => Err(ApiError::not_found(
            "the OIDC issuer is off: set `public_url` to the HTTPS URL cloud providers reach \
             this server at",
        )),
        (None, Some(_)) => Err(ApiError::not_ready(
            "the OIDC issuer is unavailable until the master key is loaded",
        )),
    }
}

/// `GET /oidc/.well-known/openid-configuration`.
pub async fn discovery(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(issuer(&state)?.discovery()))
}

/// `GET /oidc/jwks`: the public signing keys, never a private one.
pub async fn jwks(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(issuer(&state)?.jwks(now_ms())))
}

/// The result of a rotation.
#[derive(Serialize)]
pub struct Rotated {
    /// The key new tokens are signed with.
    signing_kid: String,
    /// Every published key, newest first, with when each rotated-out one is
    /// unpublished.
    keys: Vec<KeyInfo>,
}

/// `POST /orgs/{org}/oidc/keys/rotate`: a new key signs from now on, and the
/// old one stays in the JWKS for `oidc.key_grace_minutes`.
pub async fn rotate(State(state): State<AppState>, ctx: Ctx) -> ApiResult<Json<Rotated>> {
    let issuer = issuer(&state)?;
    let keyring = state
        .keyring
        .as_ref()
        .ok_or_else(|| ApiError::not_ready("the master key is not loaded"))?;
    let generated = new_oidc_key(keyring).await.map_err(ApiError::internal)?;
    let grace_ms = i64::try_from(state.config.oidc.key_grace_minutes)
        .unwrap_or(i64::MAX)
        .saturating_mul(60_000);
    let _rotating = state.oidc_rotation.lock().await;
    let mut audit = ctx.audit(state.org(), ROTATE_ACTION, "oidc");
    audit.outcome = AuditOutcome::Succeeded;
    let stored = state
        .store
        .rotate_signing_key(OIDC_KEY_PURPOSE, "RS256", generated, grace_ms, audit)
        .await?;
    let opened = open_oidc_keys(keyring, &stored).map_err(ApiError::internal)?;
    issuer.replace(&opened).map_err(ApiError::internal)?;
    let signing_kid = issuer
        .signing_kid()
        .ok_or_else(|| ApiError::internal("no active key after rotation"))?;
    Ok(Json(Rotated {
        signing_kid,
        keys: issuer.published(now_ms()),
    }))
}

/// How to set up AWS to trust this issuer for an `aws-identity` account:
/// the IAM OIDC identity provider to create and the role trust policy, scoped
/// to the account's subject. `account`, when it exists, supplies the role
/// ARN's partition and AWS account, and the audience.
pub fn aws_identity_setup(
    state: &AppState,
    id: &str,
    account: Option<&openporch_core::model::CloudAccount>,
) -> ApiResult<Value> {
    let issuer = state.config.oidc_issuer().ok_or_else(|| {
        ApiError::invalid(
            "the OIDC issuer is off: set `public_url` to the HTTPS URL AWS reaches this \
             server at",
        )
    })?;
    let config = account.map(|a| &a.config);
    let audience = config
        .and_then(|c| c.get("audience"))
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_AUDIENCE);
    let role_arn = config
        .and_then(|c| c.get("role_arn"))
        .and_then(Value::as_str);
    // arn:<partition>:iam::<account>:role/<name>
    let arn: Vec<&str> = role_arn.map(|a| a.split(':').collect()).unwrap_or_default();
    let partition = arn
        .get(1)
        .copied()
        .filter(|p| !p.is_empty())
        .unwrap_or("aws");
    let aws_account = arn
        .get(4)
        .copied()
        .filter(|a| !a.is_empty())
        .unwrap_or("<AWS_ACCOUNT_ID>");
    let host_path = issuer.trim_start_matches("https://");
    let provider_arn = format!("arn:{partition}:iam::{aws_account}:oidc-provider/{host_path}");
    let subject = identity_subject(state.org(), id);
    Ok(json!({
        "issuer": issuer,
        "provider_url": issuer,
        "provider_arn": provider_arn,
        "audience": audience,
        "subject": subject,
        "thumbprint": "AWS checks the issuer's TLS certificate against its own trusted \
                       certificate authorities, so `public_url` must be served with a \
                       certificate from a public CA. The identity provider's thumbprint \
                       list is then not used for the check; tools that still require one \
                       take the SHA-1 fingerprint of the top intermediate certificate.",
        "create_provider": format!(
            "aws iam create-open-id-connect-provider --url {issuer} --client-id-list {audience}"
        ),
        "trust_policy": {
            "Version": "2012-10-17",
            "Statement": [{
                "Effect": "Allow",
                "Principal": {"Federated": provider_arn},
                "Action": "sts:AssumeRoleWithWebIdentity",
                "Condition": {"StringEquals": {
                    format!("{host_path}:sub"): subject,
                    format!("{host_path}:aud"): audience,
                }},
            }],
        },
    }))
}

/// `GET /orgs/{org}/oidc/aws-identity/{account}`: the setup for an
/// `aws-identity` account, which need not exist yet.
pub async fn aws_identity(
    State(state): State<AppState>,
    Path((_, id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let id = AccountId::new(&id).map_err(|e| ApiError::invalid_id(e.to_string()))?;
    let account = state
        .store
        .account(state.org(), id.as_str())
        .await?
        .map(|r| r.account)
        .filter(|a| a.ty == CloudAccountType::AwsIdentity);
    Ok(Json(aws_identity_setup(
        &state,
        id.as_str(),
        account.as_ref(),
    )?))
}
