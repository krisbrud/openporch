//! Route registration with a declared permission per route, and the authentication
//! and authorization middleware that enforces it.

use axum::Router;
use axum::extract::{FromRequest, FromRequestParts, Query, RawPathParams, Request, State};
use axum::handler::Handler;
use axum::http::request::Parts;
use axum::http::{HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodFilter, on};
use openporch_store::{AuditIntent, AuditOutcome, IdentityStore, Principal, Roles, Store};
use serde::de::DeserializeOwned;
use tracing::Instrument;

use crate::auth::{self, SESSION_PREFIX, TOKEN_PREFIX};
use crate::error::{ApiError, code};
use crate::rbac::{self, AppPerm, Decision, Permission, Scope};
use crate::state::AppState;

/// A registered route and the permission it requires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteSpec {
    /// HTTP method.
    pub method: Method,
    /// Path template, such as `/orgs/{org}/apps/{app}`.
    pub path: &'static str,
    /// The required permission.
    pub permission: Permission,
}

/// The only routes that may be served without credentials.
pub const PUBLIC_ROUTES: [(Method, &str); 9] = [
    (Method::GET, "/livez"),
    (Method::GET, "/readyz"),
    (Method::GET, "/healthz"),
    (Method::GET, "/version"),
    (Method::POST, "/auth/github/device"),
    (Method::POST, "/auth/github/token"),
    (Method::GET, "/auth/github/authorize"),
    (Method::GET, "/auth/github/callback"),
    (Method::POST, "/auth/github/session"),
];

/// Builds the router. Routes can only be added together with their permission, so
/// no route can exist without one, and a public route outside [`PUBLIC_ROUTES`]
/// stops the server from starting.
pub struct Api {
    state: AppState,
    router: Router<AppState>,
    specs: Vec<RouteSpec>,
}

impl Api {
    /// An empty API.
    pub fn new(state: AppState) -> Self {
        Self {
            state,
            router: Router::new(),
            specs: Vec::new(),
        }
    }

    /// Adds a route requiring `permission`.
    pub fn route<H, T>(
        mut self,
        method: Method,
        path: &'static str,
        permission: Permission,
        handler: H,
    ) -> Self
    where
        H: Handler<T, AppState>,
        T: 'static,
    {
        assert!(
            !self
                .specs
                .iter()
                .any(|s| s.method == method && s.path == path),
            "route {method} {path} registered twice"
        );
        assert!(
            permission != Permission::Public
                || PUBLIC_ROUTES
                    .iter()
                    .any(|(m, p)| *m == method && *p == path),
            "route {method} {path} is public but not a known public route"
        );
        let filter = MethodFilter::try_from(method.clone()).expect("standard method");
        let layer = middleware::from_fn_with_state(
            self.state.clone(),
            move |State(state): State<AppState>, req: Request, next: Next| {
                authorize(state, permission, req, next)
            },
        );
        self.router = self
            .router
            .route(path, on(filter, handler).route_layer(layer));
        self.specs.push(RouteSpec {
            method,
            path,
            permission,
        });
        self
    }

    /// The registered routes.
    pub fn specs(&self) -> &[RouteSpec] {
        &self.specs
    }

    /// The finished router with fallbacks, body limits and request IDs.
    pub fn into_router(self) -> Router {
        let limit = self.state.config.max_body_bytes;
        self.router
            .fallback(|| async { ApiError::not_found("no such route") })
            .method_not_allowed_fallback(|| async {
                ApiError::new(
                    StatusCode::METHOD_NOT_ALLOWED,
                    code::METHOD_NOT_ALLOWED,
                    "method not allowed on this path",
                )
            })
            .layer(axum::extract::DefaultBodyLimit::max(limit))
            .layer(middleware::from_fn_with_state(
                self.state.clone(),
                organization_guard,
            ))
            .layer(middleware::from_fn(request_id))
            .with_state(self.state)
    }
}

/// Hides every path under another organization, before routing, so that neither
/// method fallbacks nor unknown paths reveal which routes exist.
async fn organization_guard(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let mut segments = req.uri().path().split('/').skip(1);
    if segments.next() == Some("orgs")
        && let Some(org) = segments.next()
        && org != state.org()
    {
        return ApiError::not_found(format!("organization `{org}` not found")).into_response();
    }
    next.run(req).await
}

/// The request ID, from `x-request-id` or generated.
#[derive(Debug, Clone)]
pub struct RequestId(pub String);

async fn request_id(mut req: Request, next: Next) -> Response {
    let id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty() && v.len() <= 128 && v.chars().all(|c| c.is_ascii_graphic()))
        .map(str::to_owned)
        .unwrap_or_else(|| auth::random_id(16));
    req.extensions_mut().insert(RequestId(id.clone()));
    // Every event logged while serving the request carries its ID, at any
    // log level.
    let span = tracing::error_span!("request", request_id = %id);
    let mut res = next.run(req).instrument(span).await;
    if let Ok(v) = HeaderValue::from_str(&id) {
        res.headers_mut().insert("x-request-id", v);
    }
    // Clients refuse a server whose contract they do not support, whatever
    // they asked.
    res.headers_mut().insert(
        CONTRACT_HEADER,
        HeaderValue::from_static(crate::handlers::CONTRACT_VERSION),
    );
    res
}

/// The response header naming the API contract version on every response.
pub const CONTRACT_HEADER: &str = "openporch-contract";

/// The authenticated caller of a request.
#[derive(Debug, Clone)]
pub struct Ctx {
    /// The principal.
    pub principal: Principal,
    /// Their roles.
    pub roles: Roles,
    /// The session ID when the credential is a session.
    pub session: Option<String>,
    /// The request ID.
    pub request_id: String,
    /// The request method.
    pub method: Method,
    /// The request path.
    pub path: String,
}

impl Ctx {
    /// Whether the caller is an administrator.
    pub fn is_admin(&self) -> bool {
        self.roles.org == Some(openporch_store::OrgRole::Administrator)
    }

    /// An audit intent for an accepted mutation of this request.
    pub fn audit(&self, org: &str, action: &str, target: impl Into<String>) -> AuditIntent {
        AuditIntent {
            org: org.to_owned(),
            actor: Some(self.principal.id.clone()),
            request_id: Some(self.request_id.clone()),
            method: self.method.to_string(),
            path: self.path.clone(),
            action: action.to_owned(),
            target: Some(target.into()),
            outcome: AuditOutcome::Accepted,
            status_code: None,
            reason: None,
        }
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Ctx {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, ApiError> {
        parts
            .extensions
            .get::<Ctx>()
            .cloned()
            .ok_or_else(|| ApiError::internal("route has no authenticated context"))
    }
}

/// A JSON body whose rejections use the uniform error format.
pub struct JsonBody<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for JsonBody<T> {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        match axum::Json::<T>::from_request(req, state).await {
            Ok(axum::Json(v)) => Ok(Self(v)),
            Err(r) if r.status() == StatusCode::PAYLOAD_TOO_LARGE => Err(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                code::TOO_LARGE,
                "request body too large",
            )),
            Err(r) => Err(ApiError::invalid(r.body_text())),
        }
    }
}

/// Query parameters whose rejections use the uniform error format.
pub struct QueryParams<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequestParts<S> for QueryParams<T> {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        Query::<T>::from_request_parts(parts, state)
            .await
            .map(|Query(v)| Self(v))
            .map_err(|r| ApiError::invalid(r.body_text()))
    }
}

fn bearer(parts: &Parts) -> Option<&str> {
    let value = parts
        .headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
}

/// Resolves the credential to a current member, or `None`.
async fn authenticate(
    state: &AppState,
    token: &str,
) -> Result<Option<(Principal, Option<String>)>, ApiError> {
    let now = openporch_store::now_ms();
    if token.starts_with(TOKEN_PREFIX) {
        let p = state
            .store
            .authenticate_token(state.org(), &auth::hash_token(token), now)
            .await?;
        return Ok(p.map(|p| (p, None)));
    }
    if token.starts_with(SESSION_PREFIX)
        && let Some(signer) = &state.signer
        && let Some(claims) = signer.verify(token, now)
        && claims.org == state.org()
    {
        let p = state
            .store
            .authenticate_session(state.org(), &claims.jti, &claims.sub, now)
            .await?;
        return Ok(p.map(|p| (p, Some(claims.jti))));
    }
    Ok(None)
}

async fn authorize(state: AppState, permission: Permission, req: Request, next: Next) -> Response {
    // A change holds its admission until it has answered, so shutdown waits for
    // it, including any deployment it starts.
    let (req, _admitted) = match check(&state, permission, req).await {
        Ok(checked) => checked,
        Err(e) => return e.into_response(),
    };
    let ctx = req.extensions().get::<Ctx>().cloned();
    let res = next.run(req).await;
    // Handlers deny with 403 after their own object-level checks; those denials of
    // privileged actions are recorded like the route-level ones.
    if res.status() == StatusCode::FORBIDDEN
        && let Some(ctx) = ctx
        && !matches!(ctx.method, Method::GET | Method::HEAD)
    {
        let mut intent = ctx.audit(state.org(), "request.denied", ctx.path.clone());
        intent.target = None;
        intent.outcome = AuditOutcome::Denied;
        intent.status_code = Some(403);
        intent.reason = Some("forbidden by an object-level check".into());
        if let Err(e) = state.store.record_audit(intent).await {
            return ApiError::from(e).into_response();
        }
    }
    res
}

async fn check(
    state: &AppState,
    permission: Permission,
    req: Request,
) -> Result<(Request, Option<crate::state::Admission>), ApiError> {
    let (mut parts, body) = req.into_parts();
    let params = RawPathParams::from_request_parts(&mut parts, state)
        .await
        .map_err(|e| ApiError::invalid(e.body_text()))?;
    let param = |name: &str| {
        params
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.to_owned())
    };
    if let Some(org) = param("org")
        && org != state.org()
    {
        return Err(ApiError::not_found(format!(
            "organization `{org}` not found"
        )));
    }
    if permission == Permission::Public {
        // Login creates sessions and login state, so shutdown refuses and waits
        // for it like any change. Health and version stay available.
        let admitted = if parts.uri.path().starts_with("/auth/") {
            Some(state.admit_login().map_err(|reasons| {
                ApiError::not_ready(format!("the server is not ready: {}", reasons.join(", ")))
            })?)
        } else {
            None
        };
        return Ok((Request::from_parts(parts, body), admitted));
    }
    let token = bearer(&parts).ok_or_else(ApiError::unauthenticated)?;
    let (principal, session) = authenticate(state, token)
        .await?
        .ok_or_else(ApiError::unauthenticated)?;
    let mutating = !matches!(parts.method, Method::GET | Method::HEAD);
    let admitted = if mutating {
        Some(state.admit_change().map_err(|reasons| {
            ApiError::not_ready(format!("the server is not ready: {}", reasons.join(", ")))
        })?)
    } else {
        None
    };
    let roles = state.store.roles(state.org(), &principal.id).await?;
    let request_id = parts
        .extensions
        .get::<RequestId>()
        .map(|r| r.0.clone())
        .unwrap_or_default();
    let deny = |status: u16, reason: String| {
        let mut intent = AuditIntent::accepted(
            state.org(),
            &principal.id,
            parts.method.as_str(),
            parts.uri.path(),
            "request.denied",
        );
        intent.request_id = Some(request_id.clone());
        intent.outcome = AuditOutcome::Denied;
        intent.status_code = Some(status);
        intent.reason = Some(reason);
        intent
    };
    let app = param("app");
    let mut env_type = None;
    if let (Permission::App(perm), Some(a)) = (permission, &app) {
        // Applications the caller may not see, and missing ones, look the same.
        if !rbac::can_see_app(&roles, a) || state.store.app(state.org(), a).await?.is_none() {
            if mutating {
                state
                    .store
                    .record_audit(deny(404, format!("application `{a}` not visible")))
                    .await?;
            }
            return Err(ApiError::not_found(format!("application `{a}` not found")));
        }
        if perm == AppPerm::Deploy
            && let Some(e) = param("env")
        {
            env_type = Some(
                state
                    .store
                    .env(state.org(), a, &e)
                    .await?
                    .ok_or_else(|| ApiError::not_found(format!("environment `{a}/{e}` not found")))?
                    .env_type,
            );
        }
    }
    let scope = Scope {
        app: app.as_deref(),
        env_type: env_type.as_deref(),
    };
    match rbac::evaluate(permission, &roles, &scope) {
        Decision::Allow => {}
        Decision::Hidden => {
            if mutating {
                state
                    .store
                    .record_audit(deny(404, "hidden from the caller".into()))
                    .await?;
            }
            return Err(ApiError::not_found(format!(
                "application `{}` not found",
                app.unwrap_or_default()
            )));
        }
        Decision::Deny(reason) => {
            if mutating {
                state.store.record_audit(deny(403, reason.clone())).await?;
            }
            return Err(ApiError::forbidden(reason));
        }
    }
    let ctx = Ctx {
        principal,
        roles,
        session,
        request_id,
        method: parts.method.clone(),
        path: parts.uri.path().to_owned(),
    };
    parts.extensions.insert(ctx);
    Ok((Request::from_parts(parts, body), admitted))
}
