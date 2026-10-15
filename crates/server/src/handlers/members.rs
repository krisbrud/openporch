//! Members, service users, API tokens, invitations and sessions.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use openporch_store::{
    IdentityStore, Invitation, NewInvitation, NewPrincipal, OrgRole, Principal, Roles, TokenInfo,
};
use serde::{Deserialize, Serialize};

use super::apps::display_name;
use super::check_id;
use crate::api::{Ctx, JsonBody};
use crate::auth;
use crate::error::{ApiError, ApiResult};
use crate::rbac;
use crate::state::AppState;

/// Invitations expire after seven days.
pub const INVITATION_TTL_MS: i64 = 7 * 24 * 60 * 60 * 1000;

async fn principal_or_404(state: &AppState, id: &str) -> ApiResult<Principal> {
    state
        .store
        .principal(state.org(), id)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("user `{id}` not found")))
}

/// Whether the caller may act on a member with `target` roles: administrators always;
/// managers only on non-administrators whose every permission they hold themselves.
fn may_manage(ctx: &Ctx, target: &Roles) -> bool {
    ctx.is_admin()
        || (ctx.roles.org == Some(OrgRole::Manager)
            && target.org != Some(OrgRole::Administrator)
            && rbac::covers(&ctx.roles, target))
}

fn org_only(role: OrgRole) -> Roles {
    Roles {
        org: Some(role),
        ..Roles::default()
    }
}

pub async fn list(State(state): State<AppState>) -> ApiResult<Json<Vec<Principal>>> {
    Ok(Json(state.store.principals(state.org()).await?))
}

pub async fn get(
    State(state): State<AppState>,
    Path((_, user)): Path<(String, String)>,
) -> ApiResult<Json<Principal>> {
    Ok(Json(principal_or_404(&state, &user).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateUser {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    name: Option<String>,
    role: OrgRole,
}

/// Creates a service user. Human users join through invitations.
pub async fn create(
    State(state): State<AppState>,
    ctx: Ctx,
    JsonBody(body): JsonBody<CreateUser>,
) -> ApiResult<(StatusCode, Json<Principal>)> {
    check_id("user", &body.id)?;
    if body.kind != "service" {
        return Err(ApiError::invalid(
            "only service users can be created; users join through invitations",
        ));
    }
    if !may_manage(&ctx, &org_only(body.role)) {
        return Err(ApiError::forbidden(format!(
            "creating a member with role `{}` requires more authority than the caller has",
            body.role.as_str()
        )));
    }
    let org = state.org();
    let p = state
        .store
        .create_principal(
            org,
            NewPrincipal {
                name: match body.name.as_deref() {
                    Some(name) => display_name("user", name)?.to_owned(),
                    None => body.id.clone(),
                },
                id: body.id.clone(),
                kind: "service".into(),
                email: None,
                role: body.role,
            },
            ctx.audit(org, "user.create", &body.id),
        )
        .await?;
    Ok((StatusCode::CREATED, Json(p)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateUser {
    role: Option<OrgRole>,
    name: Option<String>,
}

/// Changes a member's organization role, or a service user's display name. The
/// caller must cover the current roles and, for a role change, the resulting ones;
/// the store keeps at least one administrator.
pub async fn update(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, user)): Path<(String, String)>,
    JsonBody(body): JsonBody<UpdateUser>,
) -> ApiResult<Json<Principal>> {
    let org = state.org();
    let p = principal_or_404(&state, &user).await?;
    let current = state.store.roles(org, &user).await?;
    let resulting = Roles {
        org: body.role.or(current.org),
        ..current.clone()
    };
    if !may_manage(&ctx, &current) || !may_manage(&ctx, &resulting) {
        return Err(ApiError::forbidden(
            "changing this member requires the `administrator` role",
        ));
    }
    let name = body
        .name
        .as_deref()
        .map(|n| display_name("user", n))
        .transpose()?;
    if name.is_some() && p.kind != "service" {
        return Err(ApiError::invalid(
            "only service users can be renamed; a person's name comes from their identity",
        ));
    }
    if body.role.is_none() && name.is_none() {
        return Err(ApiError::invalid("give `role`, `name` or both"));
    }
    let p = state
        .store
        .update_principal(
            org,
            &user,
            body.role,
            name,
            ctx.audit(org, "user.update", &user),
        )
        .await?;
    Ok(Json(p))
}

/// Removes a member with their tokens, sessions and roles.
pub async fn remove(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, user)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let org = state.org();
    principal_or_404(&state, &user).await?;
    // Removal grants nothing: managers may remove any non-administrator.
    let current = state.store.roles(org, &user).await?;
    let allowed = ctx.is_admin()
        || (ctx.roles.org == Some(OrgRole::Manager) && current.org != Some(OrgRole::Administrator));
    if !allowed {
        return Err(ApiError::forbidden(
            "removing this member requires the `administrator` role",
        ));
    }
    state
        .store
        .remove_principal(org, &user, ctx.audit(org, "user.delete", &user))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
pub struct CurrentUser {
    #[serde(flatten)]
    principal: Principal,
    roles: Roles,
}

pub async fn current(ctx: Ctx) -> Json<CurrentUser> {
    Json(CurrentUser {
        principal: ctx.principal,
        roles: ctx.roles,
    })
}

/// Revokes the session presenting the request.
pub async fn logout(State(state): State<AppState>, ctx: Ctx) -> ApiResult<StatusCode> {
    let Some(jti) = &ctx.session else {
        return Err(ApiError::invalid(
            "the credential is an API token; revoke it under /users/{user}/tokens",
        ));
    };
    let org = state.org();
    let audit = ctx.audit(org, "session.revoke", &ctx.principal.id);
    state.store.revoke_session(org, jti, audit).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Who may hold, read or revoke tokens of `target`. Everyone may for themselves.
/// Only administrators and, within the delegation ceiling, managers may issue tokens
/// for service users; nobody issues tokens for another human user.
async fn check_token_access(
    state: &AppState,
    ctx: &Ctx,
    target: &str,
    issue: bool,
) -> ApiResult<()> {
    let p = principal_or_404(state, target).await?;
    if p.id == ctx.principal.id {
        return Ok(());
    }
    if p.kind != "service" {
        if !issue && ctx.is_admin() {
            return Ok(());
        }
        return Err(ApiError::forbidden(
            "tokens of a user can only be created by that user",
        ));
    }
    let roles = state.store.roles(state.org(), target).await?;
    if may_manage(ctx, &roles) {
        Ok(())
    } else {
        Err(ApiError::forbidden(format!(
            "issuing or managing tokens of `{target}` requires every permission it holds"
        )))
    }
}

pub async fn tokens(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, user)): Path<(String, String)>,
) -> ApiResult<Json<Vec<TokenInfo>>> {
    check_token_access(&state, &ctx, &user, false).await?;
    Ok(Json(state.store.tokens(state.org(), &user).await?))
}

pub async fn token(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, user, token)): Path<(String, String, String)>,
) -> ApiResult<Json<TokenInfo>> {
    check_token_access(&state, &ctx, &user, false).await?;
    state
        .store
        .tokens(state.org(), &user)
        .await?
        .into_iter()
        .find(|t| t.id == token)
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("token `{token}` of `{user}` not found")))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateToken {
    id: String,
    description: Option<String>,
    expires_at: Option<i64>,
}

/// A created token: the only response that ever contains its value.
#[derive(Serialize)]
pub struct CreatedToken {
    #[serde(flatten)]
    info: TokenInfo,
    token: String,
}

pub async fn create_token(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, user)): Path<(String, String)>,
    JsonBody(body): JsonBody<CreateToken>,
) -> ApiResult<(StatusCode, Json<CreatedToken>)> {
    state.token_limit.check(&ctx.principal.id)?;
    check_token_access(&state, &ctx, &user, true).await?;
    check_id("token", &body.id)?;
    if body
        .expires_at
        .is_some_and(|t| t <= openporch_store::now_ms())
    {
        return Err(ApiError::invalid("`expires_at` must be in the future"));
    }
    let org = state.org();
    let value = auth::new_token();
    let info = state
        .store
        .create_token(
            org,
            &user,
            &body.id,
            body.description.as_deref(),
            body.expires_at,
            &auth::hash_token(&value),
            ctx.audit(org, "token.create", format!("{user}/{}", body.id)),
        )
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(CreatedToken {
            info,
            token: value.to_string(),
        }),
    ))
}

pub async fn revoke_token(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, user, token)): Path<(String, String, String)>,
) -> ApiResult<StatusCode> {
    check_token_access(&state, &ctx, &user, false).await?;
    let org = state.org();
    state
        .store
        .revoke_token(
            org,
            &user,
            &token,
            ctx.audit(org, "token.revoke", format!("{user}/{token}")),
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateInvitation {
    role: OrgRole,
    email: Option<String>,
    provider: Option<String>,
    subject: Option<String>,
    login: Option<String>,
}

/// An invitation with its computed status.
#[derive(Serialize)]
pub struct InvitationView {
    #[serde(flatten)]
    invitation: Invitation,
    /// `pending`, `accepted` or `expired`.
    status: &'static str,
    /// The one-time code, only in the creation response.
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<String>,
}

fn view(invitation: Invitation, now: i64, code: Option<String>) -> InvitationView {
    let status = if invitation.consumed_at.is_some() {
        "accepted"
    } else if invitation.expires_at <= now {
        "expired"
    } else {
        "pending"
    };
    InvitationView {
        invitation,
        status,
        code,
    }
}

pub async fn invitations(State(state): State<AppState>) -> ApiResult<Json<Vec<InvitationView>>> {
    let now = openporch_store::now_ms();
    Ok(Json(
        state
            .store
            .invitations(state.org())
            .await?
            .into_iter()
            .map(|i| view(i, now, None))
            .collect(),
    ))
}

/// Invites by email or by external identity. A GitHub invitation may name only the
/// login; it is resolved to the account's immutable ID now, so whoever holds the login
/// later gains nothing. The one-time code is returned once.
pub async fn invite(
    State(state): State<AppState>,
    ctx: Ctx,
    JsonBody(mut body): JsonBody<CreateInvitation>,
) -> ApiResult<(StatusCode, Json<InvitationView>)> {
    let by_identity = body.provider.is_some() || body.subject.is_some();
    let valid = match (&body.email, by_identity) {
        (Some(_), false) => body.login.is_none(),
        (None, true) => body.provider.is_some() && (body.subject.is_some() || body.login.is_some()),
        _ => false,
    };
    if !valid {
        return Err(ApiError::invalid(
            "an invitation names either `email`, or `provider` with `subject` or `login`",
        ));
    }
    if body.email.as_deref().is_some_and(|e| !e.contains('@')) {
        return Err(ApiError::invalid("`email` is not an email address"));
    }
    if !may_manage(&ctx, &org_only(body.role)) {
        return Err(ApiError::forbidden(format!(
            "inviting a member with role `{}` requires more authority than the caller has",
            body.role.as_str()
        )));
    }
    if body.subject.is_none()
        && let Some(login) = body.login.clone()
    {
        if body.provider.as_deref() != Some("github") {
            return Err(ApiError::invalid(
                "only `github` invitations can name a `login` without a `subject`",
            ));
        }
        let provider = state.github.clone().ok_or_else(|| {
            ApiError::invalid(
                "GitHub login is not configured, so the login cannot be resolved; give the \
                 numeric GitHub user ID as `subject`",
            )
        })?;
        let identity = provider
            .resolve_login(&login)
            .await?
            .ok_or_else(|| ApiError::invalid(format!("GitHub has no user `{login}`")))?;
        body.subject = Some(identity.subject);
        body.login = identity.login;
    }
    let org = state.org();
    let id = auth::random_id(8);
    let code = auth::new_invitation_code();
    let now = openporch_store::now_ms();
    let invitation = state
        .store
        .create_invitation(
            org,
            NewInvitation {
                id: id.clone(),
                role: body.role,
                email: body.email,
                provider: body.provider,
                subject: body.subject,
                login: body.login,
                code_hash: auth::hash_token(&code),
                expires_at: now + INVITATION_TTL_MS,
            },
            ctx.audit(org, "invitation.create", &id),
        )
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(view(invitation, now, Some(code.to_string()))),
    ))
}
