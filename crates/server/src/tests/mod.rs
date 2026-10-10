//! HTTP tests for the scenarios of `api-access`, `authentication`,
//! `organization-membership`, `rbac` and `audit-log`.

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt as _;
use openporch_store::{
    AuditIntent, ExternalIdentity, IdentityStore, NewInvitation, OrgRole, Store,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt as _;

use crate::auth::{self, Claims};
use crate::rbac::{self, AppPerm, Decision, Permission, Scope};
use crate::{AppState, Config, RouteSpec, login, router, start};

struct Server {
    dir: TempDir,
    state: AppState,
    router: Router,
    specs: Vec<RouteSpec>,
    admin: String,
}

mod catalog;
mod envs;
mod github;

async fn server_in(dir: TempDir) -> Server {
    let config = Config::new("acme", dir.path());
    server_with(dir, config).await
}

async fn server_with(dir: TempDir, config: Config) -> Server {
    let state = start(config, None).await.unwrap();
    let admin = std::fs::read_to_string(dir.path().join("bootstrap-token"))
        .map(|t| t.trim().to_owned())
        .unwrap_or_default();
    let (router, specs) = router(state.clone());
    Server {
        dir,
        state,
        router,
        specs,
        admin,
    }
}

async fn server() -> Server {
    server_in(tempfile::tempdir().unwrap()).await
}

impl Server {
    async fn call(
        &self,
        method: Method,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(path);
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        let req = match body {
            Some(b) => req
                .header("content-type", "application/json")
                .body(Body::from(b.to_string())),
            None => req.body(Body::empty()),
        }
        .unwrap();
        let res = self.router.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| panic!("non-JSON body: {}", String::from_utf8_lossy(&bytes)))
        };
        (status, value)
    }

    async fn get(&self, path: &str, token: &str) -> (StatusCode, Value) {
        self.call(Method::GET, path, Some(token), None).await
    }

    async fn post(&self, path: &str, token: &str, body: Value) -> (StatusCode, Value) {
        self.call(Method::POST, path, Some(token), Some(body)).await
    }

    async fn delete(&self, path: &str, token: &str) -> (StatusCode, Value) {
        self.call(Method::DELETE, path, Some(token), None).await
    }

    /// Creates service user `id` with `role` and returns a token for it.
    async fn member(&self, id: &str, role: &str) -> String {
        let (s, b) = self
            .post(
                "/orgs/acme/users",
                &self.admin,
                json!({"id": id, "type": "service", "role": role}),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
        self.token_for(id).await
    }

    async fn token_for(&self, id: &str) -> String {
        let (s, b) = self
            .post(
                &format!("/orgs/acme/users/{id}/tokens"),
                &self.admin,
                json!({"id": format!("t-{}", auth::random_id(4))}),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
        b["token"].as_str().unwrap().to_owned()
    }

    async fn grant_app(&self, app: &str, user: &str, role: &str) {
        let (s, b) = self
            .post(
                &format!("/orgs/acme/apps/{app}/users"),
                &self.admin,
                json!({"id": user, "role": role}),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
    }

    async fn grant_deployer(&self, env_type: &str, user: &str) {
        let (s, b) = self
            .post(
                &format!("/orgs/acme/env-types/{env_type}/users"),
                &self.admin,
                json!({"id": user, "role": "deployer"}),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
    }

    async fn app(&self, id: &str) {
        let (s, b) = self
            .post("/orgs/acme/apps", &self.admin, json!({"id": id}))
            .await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
    }

    async fn env_type(&self, id: &str) {
        let (s, b) = self
            .post("/orgs/acme/env-types", &self.admin, json!({"id": id}))
            .await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
    }

    async fn invite(&self, body: Value) -> (String, String) {
        let (s, b) = self.post("/orgs/acme/invitations", &self.admin, body).await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
        (
            b["id"].as_str().unwrap().to_owned(),
            b["code"].as_str().unwrap().to_owned(),
        )
    }
}

fn github(subject: &str, login: &str) -> ExternalIdentity {
    ExternalIdentity {
        provider: "github".into(),
        subject: subject.into(),
        login: Some(login.into()),
        verified_emails: Vec::new(),
        name: login.into(),
    }
}

fn assert_error(status: StatusCode, body: &Value, expected: StatusCode, code: &str) {
    assert_eq!(status, expected, "{body}");
    assert_eq!(body["error"], code, "{body}");
    assert!(body["message"].is_string(), "{body}");
}

// --- api-access -------------------------------------------------------------------

#[tokio::test]
async fn single_organization() {
    let s = server().await;
    s.app("shop").await;
    let (status, body) = s.get("/orgs/acme/apps", &s.admin).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body[0]["id"], "shop");
    let (status, body) = s.get("/orgs/other/apps", &s.admin).await;
    assert_error(status, &body, StatusCode::NOT_FOUND, "API-001");
}

#[tokio::test]
async fn authentication_and_authorization_on_every_route() {
    let s = server().await;
    let (status, body) = s.call(Method::GET, "/orgs/acme/apps", None, None).await;
    assert_error(status, &body, StatusCode::UNAUTHORIZED, "AUTH-001");
    let (status, body) = s
        .call(
            Method::GET,
            "/orgs/acme/apps",
            Some("opt_not-a-token"),
            None,
        )
        .await;
    assert_error(status, &body, StatusCode::UNAUTHORIZED, "AUTH-001");

    // A viewer deleting an application gets 403, and nothing is deleted.
    s.app("shop").await;
    let viewer = s.member("viewer-1", "member").await;
    s.grant_app("shop", "viewer-1", "viewer").await;
    let (status, body) = s.delete("/orgs/acme/apps/shop", &viewer).await;
    assert_error(status, &body, StatusCode::FORBIDDEN, "AUTH-002");
    assert_eq!(
        s.get("/orgs/acme/apps/shop", &s.admin).await.0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn router_walk_every_route_declares_a_permission_and_requires_credentials() {
    let s = server().await;
    let public = [
        "/livez",
        "/readyz",
        "/healthz",
        "/version",
        "/auth/github/device",
        "/auth/github/token",
    ];
    assert!(!s.specs.is_empty());
    for spec in &s.specs {
        let path = spec
            .path
            .replace("{org}", "acme")
            .split('/')
            .map(|seg| if seg.starts_with('{') { "x" } else { seg })
            .collect::<Vec<_>>()
            .join("/");
        let body = (spec.method != Method::GET && spec.method != Method::DELETE).then(|| json!({}));
        let (status, b) = s.call(spec.method.clone(), &path, None, body).await;
        if spec.permission == Permission::Public {
            assert!(
                public.contains(&spec.path),
                "unexpected public route {}",
                spec.path
            );
            assert_ne!(
                status,
                StatusCode::UNAUTHORIZED,
                "{} {}",
                spec.method,
                spec.path
            );
        } else {
            assert_error(status, &b, StatusCode::UNAUTHORIZED, "AUTH-001");
        }
    }
    // Every public path is registered.
    for p in public {
        assert!(s.specs.iter().any(|r| r.path == p), "{p}");
    }
}

#[tokio::test]
async fn uniform_errors_and_identifiers() {
    let s = server().await;
    let (status, body) = s
        .post("/orgs/acme/apps", &s.admin, json!({"id": "My App"}))
        .await;
    assert_error(status, &body, StatusCode::BAD_REQUEST, "API-002");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("lowercase letters"),
        "{body}"
    );
    s.app("shop").await;
    let (status, body) = s
        .post("/orgs/acme/apps", &s.admin, json!({"id": "shop"}))
        .await;
    assert_error(status, &body, StatusCode::CONFLICT, "API-004");
    // Malformed bodies, unknown routes and wrong methods use the same format.
    let (status, body) = s
        .post("/orgs/acme/apps", &s.admin, json!({"name": 3}))
        .await;
    assert_error(status, &body, StatusCode::BAD_REQUEST, "API-003");
    let (status, body) = s.get("/nowhere", &s.admin).await;
    assert_error(status, &body, StatusCode::NOT_FOUND, "API-001");
    let (status, body) = s
        .call(Method::PUT, "/orgs/acme/apps", Some(&s.admin), None)
        .await;
    assert_error(status, &body, StatusCode::METHOD_NOT_ALLOWED, "API-005");
}

#[tokio::test]
async fn health_and_version() {
    let s = server().await;
    for p in ["/livez", "/readyz", "/healthz"] {
        let (status, _) = s.call(Method::GET, p, None, None).await;
        assert_eq!(status, StatusCode::OK, "{p}");
    }
    let (status, body) = s.call(Method::GET, "/version", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["version"], crate::VERSION);
    assert_eq!(body["contract"], "1");
    assert!(body["capabilities"].is_object());
}

#[tokio::test]
async fn missing_master_key_blocks_readiness_and_mutations() {
    let s = server().await;
    let admin = s.admin.clone();
    let key_id = s.state.keyring.as_ref().unwrap().active_id().to_owned();
    let Server {
        dir, state, router, ..
    } = s;
    drop((state, router));
    std::fs::remove_file(dir.path().join("master.key")).unwrap();

    let s = server_in(dir).await;
    let (status, _) = s.call(Method::GET, "/livez", None, None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = s.call(Method::GET, "/readyz", None, None).await;
    assert_error(status, &body, StatusCode::SERVICE_UNAVAILABLE, "SRV-002");
    assert_eq!(
        body["reasons"],
        json!([format!("master_key_missing:{key_id}")])
    );
    // Reads work; mutations are refused.
    assert_eq!(s.get("/orgs/acme/users", &admin).await.0, StatusCode::OK);
    let (status, body) = s
        .post("/orgs/acme/apps", &admin, json!({"id": "shop"}))
        .await;
    assert_error(status, &body, StatusCode::SERVICE_UNAVAILABLE, "SRV-002");
}

// --- authentication ---------------------------------------------------------------

#[tokio::test]
async fn first_start_bootstraps_an_administrator() {
    // The log side of this scenario is in `tests/bootstrap_logs.rs`.
    let s = server().await;

    let path = s.dir.path().join("bootstrap-token");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    assert!(s.admin.starts_with(auth::TOKEN_PREFIX));

    let (status, body) = s.get("/orgs/acme/users", &s.admin).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body[0]["id"], "bootstrap");
    assert_eq!(body[0]["type"], "service");
    assert_eq!(body[0]["role"], "administrator");
    // The token expires after 24 hours by default.
    let (_, tokens) = s.get("/orgs/acme/users/bootstrap/tokens", &s.admin).await;
    let expires = tokens[0]["expires_at"].as_i64().unwrap();
    let created = tokens[0]["created_at"].as_i64().unwrap();
    assert!((expires - created - 24 * 3_600_000).abs() < 60_000);

    // A restart does not bootstrap again.
    let Server {
        dir, state, router, ..
    } = s;
    drop((state, router));
    std::fs::remove_file(&path).unwrap();
    let s = server_in(dir).await;
    assert!(!s.dir.path().join("bootstrap-token").exists());
}

#[tokio::test]
async fn tokens_are_shown_once_and_revocable() {
    let s = server().await;
    s.member("ci", "member").await;
    let (status, body) = s
        .post(
            "/orgs/acme/users/ci/tokens",
            &s.admin,
            json!({"id": "deploy", "description": "CI"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let token = body["token"].as_str().unwrap().to_owned();
    assert_eq!(body["id"], "deploy");

    assert_eq!(s.get("/orgs/acme/current-user", &token).await.1["id"], "ci");
    let (_, info) = s.get("/orgs/acme/users/ci/tokens/deploy", &s.admin).await;
    assert!(info.get("token").is_none(), "{info}");
    assert!(info["last_used_at"].is_i64(), "{info}");
    let (_, list) = s.get("/orgs/acme/users/ci/tokens", &s.admin).await;
    assert!(list.to_string().find(&token).is_none());

    let (status, _) = s
        .delete("/orgs/acme/users/ci/tokens/deploy", &s.admin)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let before = s
        .state
        .store
        .audit_entries("acme", 1000)
        .await
        .unwrap()
        .len();
    let (status, body) = s.get("/orgs/acme/current-user", &token).await;
    assert_error(status, &body, StatusCode::UNAUTHORIZED, "AUTH-001");
    let (status, _) = s.delete("/orgs/acme/apps/x", &token).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        s.state
            .store
            .audit_entries("acme", 1000)
            .await
            .unwrap()
            .len(),
        before,
        "a revoked token recorded something"
    );
}

#[tokio::test]
async fn expired_tokens_are_rejected() {
    let s = server().await;
    s.member("ci", "member").await;
    let soon = openporch_store::now_ms() + 50;
    let (status, body) = s
        .post(
            "/orgs/acme/users/ci/tokens",
            &s.admin,
            json!({"id": "short", "expires_at": soon}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let token = body["token"].as_str().unwrap().to_owned();
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    assert_eq!(
        s.get("/orgs/acme/current-user", &token).await.0,
        StatusCode::UNAUTHORIZED
    );
    // Expiry in the past is refused at creation.
    let (status, _) = s
        .post(
            "/orgs/acme/users/ci/tokens",
            &s.admin,
            json!({"id": "old", "expires_at": 1}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn who_may_create_tokens() {
    let s = server().await;
    let member = s.member("dana", "member").await;
    s.member("erin", "member").await;
    // A member cannot create tokens for someone else, but can for themselves.
    let (status, body) = s
        .post("/orgs/acme/users/erin/tokens", &member, json!({"id": "xx"}))
        .await;
    assert_error(status, &body, StatusCode::FORBIDDEN, "AUTH-002");
    let (status, _) = s
        .post(
            "/orgs/acme/users/dana/tokens",
            &member,
            json!({"id": "mine"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    // Nobody lists another member's tokens without authority.
    assert_eq!(
        s.get("/orgs/acme/users/erin/tokens", &member).await.0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn missing_bearer_header() {
    let s = server().await;
    let req_without_scheme = s.call(Method::GET, "/orgs/acme/users", None, None).await;
    assert_eq!(req_without_scheme.0, StatusCode::UNAUTHORIZED);
}

async fn logged_in(s: &Server, subject: &str, login_name: &str, role: &str) -> String {
    let (_, code) = s
        .invite(
            json!({"provider": "github", "subject": subject, "login": login_name, "role": role}),
        )
        .await;
    login::login(&s.state, &github(subject, login_name), Some(&code))
        .await
        .unwrap()
}

#[tokio::test]
async fn sessions_check_membership_and_revocation_on_every_request() {
    let s = server().await;
    let session = logged_in(&s, "583231", "octocat", "member").await;
    assert!(session.starts_with(auth::SESSION_PREFIX));
    let (status, me) = s.get("/orgs/acme/current-user", &session).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["identity"], "github:583231");
    // Later logins map the same immutable ID to the same user without a code.
    let again = login::login(&s.state, &github("583231", "octocat"), None)
        .await
        .unwrap();
    assert_eq!(
        s.get("/orgs/acme/current-user", &again).await.1["id"],
        me["id"]
    );

    // Logout revokes this session only.
    assert_eq!(
        s.delete("/orgs/acme/sessions/current", &session).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        s.get("/orgs/acme/current-user", &session).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        s.get("/orgs/acme/current-user", &again).await.0,
        StatusCode::OK
    );

    // A removed user's valid signature no longer authenticates.
    let id = me["id"].as_str().unwrap();
    assert_eq!(
        s.delete(&format!("/orgs/acme/users/{id}"), &s.admin)
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        s.get("/orgs/acme/current-user", &again).await.0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn sessions_expire_after_24_hours() {
    let s = server().await;
    let session = logged_in(&s, "1", "alice", "member").await;
    let signer = s.state.signer.as_ref().unwrap();
    let claims = signer.verify(&session, openporch_store::now_ms()).unwrap();
    assert_eq!(claims.exp - claims.iat, auth::SESSION_TTL_MS);
    // The same session presented 25 hours later.
    assert!(
        signer
            .verify(&session, claims.iat + 25 * 3_600_000)
            .is_none()
    );
    let expired = signer.sign(&Claims {
        iat: claims.iat - 25 * 3_600_000,
        exp: claims.iat - 3_600_000,
        ..claims.clone()
    });
    assert_eq!(
        s.get("/orgs/acme/current-user", &expired).await.0,
        StatusCode::UNAUTHORIZED
    );
    // A session for another organization or with a forged signature fails.
    let other = signer.sign(&Claims {
        org: "other".into(),
        ..claims
    });
    assert_eq!(
        s.get("/orgs/acme/current-user", &other).await.0,
        StatusCode::UNAUTHORIZED
    );
    let forged = format!("{}x", &session[..session.len() - 1]);
    assert_eq!(
        s.get("/orgs/acme/current-user", &forged).await.0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn unknown_identities_need_an_invitation() {
    let s = server().await;
    let e = login::login(&s.state, &github("99", "stranger"), None)
        .await
        .unwrap_err();
    assert_eq!(e.status, StatusCode::UNAUTHORIZED);
    assert!(
        e.message.contains("invitation is required"),
        "{}",
        e.message
    );
}

#[tokio::test]
async fn identity_binding_is_locked_after_first_login() {
    let s = server().await;
    let (_, code) = s
        .invite(json!({"email": "alice@example.com", "role": "member"}))
        .await;
    let alice = ExternalIdentity {
        verified_emails: vec!["alice@example.com".into()],
        ..github("7", "alice")
    };
    login::login(&s.state, &alice, Some(&code)).await.unwrap();
    // The same email through another provider gets no access, even with the code.
    let oidc = ExternalIdentity {
        provider: "https://id.example.com".into(),
        subject: "alice-sub".into(),
        login: None,
        verified_emails: vec!["alice@example.com".into()],
        name: "Alice".into(),
    };
    assert!(login::login(&s.state, &oidc, None).await.is_err());
    assert!(login::login(&s.state, &oidc, Some(&code)).await.is_err());
    // Not even with a fresh invitation for the same email.
    let (_, fresh) = s
        .invite(json!({"email": "alice@example.com", "role": "member"}))
        .await;
    let e = login::login(&s.state, &oidc, Some(&fresh))
        .await
        .unwrap_err();
    assert!(
        e.message.contains("already belongs to a member"),
        "{}",
        e.message
    );
}

#[tokio::test]
async fn reused_github_login_gains_nothing() {
    let s = server().await;
    let (_, code) = s
        .invite(
            json!({"provider": "github", "subject": "100", "login": "octocat", "role": "member"}),
        )
        .await;
    // Another account that now holds the login name `octocat`.
    let e = login::login(&s.state, &github("200", "octocat"), Some(&code))
        .await
        .unwrap_err();
    assert_eq!(e.status, StatusCode::UNAUTHORIZED);
    // The invitation is still usable by the original account.
    login::login(&s.state, &github("100", "octocat"), Some(&code))
        .await
        .unwrap();
}

#[tokio::test]
async fn unverified_invitation_email_is_denied() {
    let s = server().await;
    let (_, code) = s
        .invite(json!({"email": "bob@example.com", "role": "member"}))
        .await;
    let unverified = github("8", "bob");
    assert!(
        login::login(&s.state, &unverified, Some(&code))
            .await
            .is_err()
    );
}

// --- organization-membership ---------------------------------------------------------

#[tokio::test]
async fn last_administrator_is_kept() {
    let s = server().await;
    let (status, body) = s
        .call(
            Method::PATCH,
            "/orgs/acme/users/bootstrap",
            Some(&s.admin),
            Some(json!({"role": "member"})),
        )
        .await;
    assert_error(status, &body, StatusCode::CONFLICT, "API-004");
    let (status, _) = s.delete("/orgs/acme/users/bootstrap", &s.admin).await;
    assert_eq!(status, StatusCode::CONFLICT);
    // With a second administrator, the change succeeds.
    s.member("admin-2", "administrator").await;
    let (status, body) = s
        .call(
            Method::PATCH,
            "/orgs/acme/users/bootstrap",
            Some(&s.admin),
            Some(json!({"role": "member"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["role"], "member");
}

#[tokio::test]
async fn members_are_listed_with_type_role_and_identity() {
    let s = server().await;
    logged_in(&s, "583231", "octocat", "orgViewer").await;
    let (_, users) = s.get("/orgs/acme/users", &s.admin).await;
    let octocat = users
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["identity"] == "github:583231")
        .unwrap();
    assert_eq!(octocat["type"], "user");
    assert_eq!(octocat["role"], "orgViewer");
    assert!(octocat["created_at"].is_i64());
}

#[tokio::test]
async fn accept_invitation_by_github_identity() {
    let s = server().await;
    let (id, code) = s
        .invite(json!({"provider": "github", "subject": "583231", "login": "octocat", "role": "member"}))
        .await;
    let session = login::login(&s.state, &github("583231", "octocat"), Some(&code))
        .await
        .unwrap();
    let (_, me) = s.get("/orgs/acme/current-user", &session).await;
    assert_eq!(me["role"], "member");
    assert_eq!(me["type"], "user");
    let (_, invitations) = s.get("/orgs/acme/invitations", &s.admin).await;
    let inv = invitations
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == id.as_str())
        .unwrap();
    assert_eq!(inv["status"], "accepted");
    assert!(inv.get("code").is_none());
    // The code is single-use.
    assert!(
        login::login(&s.state, &github("583232", "octocat2"), Some(&code))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn expired_invitation() {
    let s = server().await;
    let code = auth::new_invitation_code();
    let eight_days_ago = openporch_store::now_ms() - 8 * 24 * 3_600_000;
    s.state
        .store
        .create_invitation(
            "acme",
            NewInvitation {
                id: "old".into(),
                role: OrgRole::Member,
                email: None,
                provider: Some("github".into()),
                subject: Some("5".into()),
                login: Some("late".into()),
                code_hash: auth::hash_token(&code),
                expires_at: eight_days_ago + crate::handlers::INVITATION_TTL_MS,
            },
            AuditIntent::accepted(
                "acme",
                "bootstrap",
                "POST",
                "/orgs/acme/invitations",
                "invitation.create",
            ),
        )
        .await
        .unwrap();
    let e = login::login(&s.state, &github("5", "late"), Some(&code))
        .await
        .unwrap_err();
    assert!(e.message.contains("expired"), "{}", e.message);
    let (_, invitations) = s.get("/orgs/acme/invitations", &s.admin).await;
    assert_eq!(invitations[0]["status"], "expired");
}

#[tokio::test]
async fn manager_delegation_ceiling() {
    let s = server().await;
    let manager = s.member("carol", "manager").await;
    s.member("admin-2", "administrator").await;
    s.member("dev", "member").await;

    // Invite or create an administrator.
    let (status, body) = s
        .post(
            "/orgs/acme/invitations",
            &manager,
            json!({"email": "x@example.com", "role": "administrator"}),
        )
        .await;
    assert_error(status, &body, StatusCode::FORBIDDEN, "AUTH-002");
    let (status, _) = s
        .post(
            "/orgs/acme/users",
            &manager,
            json!({"id": "root-2", "type": "service", "role": "administrator"}),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        s.get("/orgs/acme/users/root-2", &s.admin).await.0,
        StatusCode::NOT_FOUND
    );

    // Promote a member to administrator.
    let (status, _) = s
        .call(
            Method::PATCH,
            "/orgs/acme/users/dev",
            Some(&manager),
            Some(json!({"role": "administrator"})),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        s.get("/orgs/acme/users/dev", &s.admin).await.1["role"],
        "member"
    );

    // Remove an administrator.
    let (status, _) = s.delete("/orgs/acme/users/admin-2", &manager).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Token for bootstrap.
    let (status, _) = s
        .post(
            "/orgs/acme/users/bootstrap/tokens",
            &manager,
            json!({"id": "steal"}),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (_, tokens) = s.get("/orgs/acme/users/bootstrap/tokens", &s.admin).await;
    assert!(
        tokens
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t["id"] != "steal")
    );

    // Within the ceiling, managers invite, create and remove non-administrators.
    let (status, _) = s
        .post(
            "/orgs/acme/invitations",
            &manager,
            json!({"email": "y@example.com", "role": "member"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = s
        .post(
            "/orgs/acme/users",
            &manager,
            json!({"id": "bot", "type": "service", "role": "member"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = s
        .post("/orgs/acme/users/bot/tokens", &manager, json!({"id": "t1"}))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        s.delete("/orgs/acme/users/bot", &manager).await.0,
        StatusCode::NO_CONTENT
    );
    // Members cannot manage membership at all.
    let member = s.token_for("dev").await;
    let (status, _) = s
        .post(
            "/orgs/acme/invitations",
            &member,
            json!({"email": "z@example.com", "role": "member"}),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn service_user_beyond_the_issuers_authority() {
    let s = server().await;
    s.app("shop").await;
    s.env_type("production").await;
    let manager = s.member("carol", "manager").await;
    s.grant_app("shop", "carol", "developer").await;
    s.grant_deployer("development", "carol").await;
    s.member("ci-prod", "member").await;
    s.grant_app("shop", "ci-prod", "developer").await;
    s.grant_deployer("production", "ci-prod").await;
    let (status, body) = s
        .post(
            "/orgs/acme/users/ci-prod/tokens",
            &manager,
            json!({"id": "t1"}),
        )
        .await;
    assert_error(status, &body, StatusCode::FORBIDDEN, "AUTH-002");
    // A development CI user within carol's authority is fine.
    s.member("ci-dev", "member").await;
    s.grant_app("shop", "ci-dev", "developer").await;
    s.grant_deployer("development", "ci-dev").await;
    let (status, _) = s
        .post(
            "/orgs/acme/users/ci-dev/tokens",
            &manager,
            json!({"id": "t1"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
async fn ci_service_user() {
    let s = server().await;
    s.app("shop").await;
    s.app("billing").await;
    let ci = s.member("ci", "member").await;
    s.grant_app("shop", "ci", "developer").await;
    s.grant_deployer("development", "ci").await;
    assert_eq!(s.get("/orgs/acme/apps/shop", &ci).await.0, StatusCode::OK);
    assert_eq!(
        s.get("/orgs/acme/apps/billing", &ci).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        s.post("/orgs/acme/apps", &ci, json!({"id": "new"})).await.0,
        StatusCode::FORBIDDEN
    );
    // Its token carries exactly these roles: deploy to shop/development and nothing else.
    let roles = s.state.store.roles("acme", "ci").await.unwrap();
    let scope = |app, env_type| Scope {
        app: Some(app),
        env_type: Some(env_type),
    };
    let deploy = Permission::App(AppPerm::Deploy);
    assert_eq!(
        rbac::evaluate(deploy, &roles, &scope("shop", "development")),
        Decision::Allow
    );
    assert!(matches!(
        rbac::evaluate(deploy, &roles, &scope("shop", "production")),
        Decision::Deny(_)
    ));
    assert_eq!(
        rbac::evaluate(deploy, &roles, &scope("billing", "development")),
        Decision::Hidden
    );
}

#[tokio::test]
async fn removal_revokes_access() {
    let s = server().await;
    let alice = s.member("alice", "member").await;
    s.app("shop").await;
    s.grant_app("shop", "alice", "owner").await;
    s.grant_deployer("development", "alice").await;
    assert_eq!(
        s.get("/orgs/acme/current-user", &alice).await.0,
        StatusCode::OK
    );
    assert_eq!(
        s.delete("/orgs/acme/users/alice", &s.admin).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        s.get("/orgs/acme/current-user", &alice).await.0,
        StatusCode::UNAUTHORIZED
    );
    let (_, shop_users) = s.get("/orgs/acme/apps/shop/users", &s.admin).await;
    assert!(
        shop_users
            .as_array()
            .unwrap()
            .iter()
            .all(|u| u["user"] != "alice")
    );
    let (_, deployers) = s
        .get("/orgs/acme/env-types/development/users", &s.admin)
        .await;
    assert_eq!(deployers, json!([]));
}

// --- rbac ---------------------------------------------------------------------------

#[tokio::test]
async fn union_of_roles_and_hidden_applications() {
    let s = server().await;
    for app in ["shop", "billing", "secret-project"] {
        s.app(app).await;
    }
    let bob = s.member("bob", "member").await;
    s.grant_app("shop", "bob", "viewer").await;
    s.grant_app("billing", "bob", "developer").await;
    let (_, apps) = s.get("/orgs/acme/apps", &bob).await;
    let ids: Vec<_> = apps
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].clone())
        .collect();
    assert_eq!(ids, vec![json!("billing"), json!("shop")]);
    assert_eq!(s.get("/orgs/acme/apps/shop", &bob).await.0, StatusCode::OK);
    let (status, body) = s.get("/orgs/acme/apps/secret-project", &bob).await;
    assert_error(status, &body, StatusCode::NOT_FOUND, "API-001");
    // Hidden and missing applications look the same.
    let (_, missing) = s.get("/orgs/acme/apps/nope", &bob).await;
    assert_eq!(missing["error"], body["error"]);
    // Org viewers read every application but change nothing.
    let viewer = s.member("auditor", "orgViewer").await;
    assert_eq!(
        s.get("/orgs/acme/apps/secret-project", &viewer).await.0,
        StatusCode::OK
    );
    assert_eq!(
        s.delete("/orgs/acme/apps/secret-project", &viewer).await.0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn administration_is_reserved_to_administrators() {
    let s = server().await;
    s.app("shop").await;
    let manager = s.member("carol", "manager").await;
    let developer = s.member("dev", "member").await;
    s.grant_app("shop", "dev", "developer").await;
    for token in [&manager, &developer] {
        let (status, body) = s
            .post("/orgs/acme/env-types", token, json!({"id": "staging"}))
            .await;
        assert_error(status, &body, StatusCode::FORBIDDEN, "AUTH-002");
        let (status, _) = s
            .post(
                "/orgs/acme/env-types/development/users",
                token,
                json!({"id": "dev", "role": "deployer"}),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
    // Developers cannot manage application users; owners can.
    let (status, _) = s
        .post(
            "/orgs/acme/apps/shop/users",
            &developer,
            json!({"id": "carol", "role": "viewer"}),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn creator_becomes_owner() {
    let s = server().await;
    let carol = s.member("carol", "manager").await;
    let (status, _) = s
        .post("/orgs/acme/apps", &carol, json!({"id": "shop"}))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, users) = s.get("/orgs/acme/apps/shop/users", &carol).await;
    assert_eq!(users, json!([{"user": "carol", "role": "owner"}]));
    // As owner, carol manages the application's users.
    s.member("dev", "member").await;
    let (status, _) = s
        .post(
            "/orgs/acme/apps/shop/users",
            &carol,
            json!({"id": "dev", "role": "developer"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = s
        .call(
            Method::PATCH,
            "/orgs/acme/apps/shop/users/dev",
            Some(&carol),
            Some(json!({"role": "viewer"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        s.delete("/orgs/acme/apps/shop/users/dev", &carol).await.0,
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn deleting_an_application_needs_owner_and_deployer() {
    let s = server().await;
    let carol = s.member("carol", "manager").await;
    s.post("/orgs/acme/apps", &carol, json!({"id": "shop"}))
        .await;
    let (status, body) = s.delete("/orgs/acme/apps/shop", &carol).await;
    assert_error(status, &body, StatusCode::FORBIDDEN, "AUTH-002");
    assert!(
        body["message"].as_str().unwrap().contains("`deployer`"),
        "{body}"
    );
    s.grant_deployer("development", "carol").await;
    assert_eq!(
        s.delete("/orgs/acme/apps/shop", &carol).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        s.get("/orgs/acme/apps/shop", &s.admin).await.0,
        StatusCode::NOT_FOUND
    );
}

// --- audit-log ----------------------------------------------------------------------

#[tokio::test]
async fn mutations_and_denials_are_audited() {
    let s = server().await;
    let ci = s.member("ci", "manager").await;
    let (status, _) = s.post("/orgs/acme/apps", &ci, json!({"id": "shop"})).await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, page) = s.get("/orgs/acme/audit-logs?user_id=ci", &s.admin).await;
    let entry = &page["entries"][0];
    assert_eq!(entry["user_id"], "ci");
    assert_eq!(entry["method"], "POST");
    assert_eq!(entry["path"], "/orgs/acme/apps");
    assert_eq!(entry["action"], "app.create");
    assert_eq!(entry["target"], "shop");
    assert!(entry["request_id"].is_string());

    // A denied privileged action is recorded; reads are not.
    let (status, _) = s
        .post("/orgs/acme/env-types", &ci, json!({"id": "prod"}))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (_, page) = s.get("/orgs/acme/audit-logs?user_id=ci", &s.admin).await;
    let denied = &page["entries"][0];
    assert_eq!(denied["outcome"], "denied");
    assert_eq!(denied["status_code"], 403);
    assert_eq!(page["entries"].as_array().unwrap().len(), 2);

    // No token value reaches the log.
    let log = s.get("/orgs/acme/audit-logs", &s.admin).await.1.to_string();
    assert!(!log.contains(&ci) && !log.contains(&s.admin));
}

#[tokio::test]
async fn audit_query_filters_and_pages() {
    let s = server().await;
    for app in ["shop", "billing"] {
        s.app(app).await;
    }
    s.member("dev", "member").await;
    s.grant_app("shop", "dev", "viewer").await;
    s.delete("/orgs/acme/apps/shop/users/dev", &s.admin).await;
    s.delete("/orgs/acme/apps/billing", &s.admin).await;
    let (status, page) = s
        .get(
            "/orgs/acme/audit-logs?path_prefix=/orgs/acme/apps/shop&method=DELETE",
            &s.admin,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let entries = page["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1, "{page}");
    assert_eq!(entries[0]["path"], "/orgs/acme/apps/shop/users/dev");

    // Pagination: newest first, `next` continues.
    let (_, first) = s.get("/orgs/acme/audit-logs?limit=2", &s.admin).await;
    let next = first["next"].as_i64().unwrap();
    let (_, second) = s
        .get(
            &format!("/orgs/acme/audit-logs?limit=2&before={next}"),
            &s.admin,
        )
        .await;
    let a = first["entries"][1]["seq"].as_i64().unwrap();
    let b = second["entries"][0]["seq"].as_i64().unwrap();
    assert!(b < a);
    assert_eq!(
        s.get("/orgs/acme/audit-logs?limit=0", &s.admin).await.0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn audit_access_and_deletion() {
    let s = server().await;
    let viewer = s.member("auditor", "orgViewer").await;
    let member = s.member("dev", "member").await;
    assert_eq!(
        s.get("/orgs/acme/audit-logs", &viewer).await.0,
        StatusCode::OK
    );
    assert_eq!(
        s.get("/orgs/acme/audit-logs", &member).await.0,
        StatusCode::FORBIDDEN
    );
    for token in [&s.admin, &viewer, &member] {
        let (status, body) = s.delete("/orgs/acme/audit-logs", token).await;
        assert_error(status, &body, StatusCode::METHOD_NOT_ALLOWED, "API-005");
    }
}

#[tokio::test]
async fn audit_failure_rejects_the_mutation() {
    let s = server().await;
    s.member("dev", "member").await;
    sqlx::query(
        "CREATE TRIGGER audit_full BEFORE INSERT ON audit_log \
         BEGIN SELECT RAISE(ABORT, 'storage full'); END",
    )
    .execute(s.state.store.pool())
    .await
    .unwrap();
    let (status, body) = s
        .call(
            Method::PATCH,
            "/orgs/acme/users/dev",
            Some(&s.admin),
            Some(json!({"role": "manager"})),
        )
        .await;
    assert_error(status, &body, StatusCode::INTERNAL_SERVER_ERROR, "SRV-001");
    assert!(!body["message"].as_str().unwrap().contains("storage full"));
    assert_eq!(
        s.state
            .store
            .principal("acme", "dev")
            .await
            .unwrap()
            .unwrap()
            .role,
        OrgRole::Member
    );
}

#[tokio::test]
async fn hidden_and_missing_applications_are_404_and_mutations_are_audited() {
    let s = server().await;
    s.app("billing").await;
    let member = s.member("dev", "member").await;
    let viewer = s.member("auditor", "orgViewer").await;
    let (status, body) = s.delete("/orgs/acme/apps/billing", &member).await;
    assert_error(status, &body, StatusCode::NOT_FOUND, "API-001");
    let (status, _) = s.delete("/orgs/acme/apps/nope", &viewer).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, page) = s.get("/orgs/acme/audit-logs?user_id=dev", &s.admin).await;
    let entry = &page["entries"][0];
    assert_eq!(entry["outcome"], "denied");
    assert_eq!(entry["status_code"], 404);
    assert_eq!(entry["path"], "/orgs/acme/apps/billing");
}

#[tokio::test]
async fn delegation_is_rechecked_with_the_change() {
    // The store re-checks the ceiling inside the transaction, so a check that passed
    // in the handler cannot be raced: here the store is called directly.
    let s = server().await;
    s.member("carol", "manager").await;
    s.member("admin-2", "administrator").await;
    s.member("ci", "member").await;
    let as_carol = |path: &str| AuditIntent::accepted("acme", "carol", "PATCH", path, "test");
    let store = &s.state.store;
    let e = store
        .set_org_role("acme", "admin-2", OrgRole::Member, as_carol("/u/admin-2"))
        .await
        .unwrap_err();
    assert!(
        matches!(e, openporch_store::StoreError::Forbidden(_)),
        "{e}"
    );
    let e = store
        .remove_principal("acme", "admin-2", as_carol("/u/admin-2"))
        .await
        .unwrap_err();
    assert!(
        matches!(e, openporch_store::StoreError::Forbidden(_)),
        "{e}"
    );
    // `ci` gained production deployer after the handler would have checked.
    s.env_type("production").await;
    s.grant_deployer("production", "ci").await;
    let e = store
        .create_token(
            "acme",
            "ci",
            "t1",
            None,
            None,
            "hash",
            as_carol("/u/ci/tokens"),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(e, openporch_store::StoreError::Forbidden(_)),
        "{e}"
    );
    assert_eq!(
        store
            .principal("acme", "admin-2")
            .await
            .unwrap()
            .unwrap()
            .role,
        OrgRole::Administrator
    );
}

#[tokio::test]
async fn interrupted_bootstrap_completes_on_the_next_start() {
    let s = server().await;
    let token = s.admin.clone();
    let Server {
        dir, state, router, ..
    } = s;
    drop((state, router));
    // Simulate a crash after the commit, before the file was moved into place.
    let path = dir.path().join("bootstrap-token");
    std::fs::rename(&path, dir.path().join("bootstrap-token.pending")).unwrap();
    let s = server_in(dir).await;
    assert_eq!(s.admin, token);
    assert_eq!(
        s.get("/orgs/acme/current-user", &token).await.0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn managers_remove_any_non_administrator() {
    let s = server().await;
    s.app("shop").await;
    let manager = s.member("carol", "manager").await;
    s.member("dev", "member").await;
    s.grant_app("shop", "dev", "owner").await;
    s.member("auditor", "orgViewer").await;
    assert_eq!(
        s.delete("/orgs/acme/users/dev", &manager).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        s.delete("/orgs/acme/users/auditor", &manager).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        s.delete("/orgs/acme/users/bootstrap", &manager).await.0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn handler_denials_are_audited() {
    let s = server().await;
    let manager = s.member("carol", "manager").await;
    let (status, _) = s
        .post(
            "/orgs/acme/users/bootstrap/tokens",
            &manager,
            json!({"id": "steal"}),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (_, page) = s.get("/orgs/acme/audit-logs?user_id=carol", &s.admin).await;
    let entry = &page["entries"][0];
    assert_eq!(entry["outcome"], "denied");
    assert_eq!(entry["status_code"], 403);
    assert_eq!(entry["path"], "/orgs/acme/users/bootstrap/tokens");
}

#[tokio::test]
async fn invitations_need_the_inviters_current_authority() {
    let s = server().await;
    let manager = s.member("carol", "manager").await;
    let (status, inv) = s
        .post(
            "/orgs/acme/invitations",
            &manager,
            json!({"provider": "github", "subject": "42", "login": "x", "role": "manager"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let code = inv["code"].as_str().unwrap().to_owned();
    let (status, _) = s
        .call(
            Method::PATCH,
            "/orgs/acme/users/carol",
            Some(&s.admin),
            Some(json!({"role": "member"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let e = login::login(&s.state, &github("42", "x"), Some(&code))
        .await
        .unwrap_err();
    assert_eq!(e.status, StatusCode::UNAUTHORIZED);
    assert!(e.message.contains("no longer valid"), "{}", e.message);
}

#[tokio::test]
async fn role_grants_are_rechecked_with_the_change() {
    let s = server().await;
    s.app("shop").await;
    s.member("alice", "member").await;
    s.member("bob", "member").await;
    s.grant_app("shop", "alice", "owner").await;
    // alice lost ownership after her request passed the route check.
    s.delete("/orgs/acme/apps/shop/users/alice", &s.admin).await;
    let as_alice = AuditIntent::accepted(
        "acme",
        "alice",
        "POST",
        "/orgs/acme/apps/shop/users",
        "test",
    );
    let e = s
        .state
        .store
        .set_app_role(
            "acme",
            "shop",
            "bob",
            openporch_store::AppRole::Owner,
            as_alice.clone(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(e, openporch_store::StoreError::Forbidden(_)),
        "{e}"
    );
    let e = s
        .state
        .store
        .set_env_type_role("acme", "development", "bob", as_alice)
        .await
        .unwrap_err();
    assert!(
        matches!(e, openporch_store::StoreError::Forbidden(_)),
        "{e}"
    );
}

#[tokio::test]
async fn application_deletion_is_rechecked_with_the_change() {
    let s = server().await;
    s.app("shop").await;
    s.member("alice", "member").await;
    s.grant_app("shop", "alice", "owner").await;
    s.grant_deployer("development", "alice").await;
    // alice lost `deployer` after her request passed the handler check.
    s.delete("/orgs/acme/env-types/development/users/alice", &s.admin)
        .await;
    let as_alice = AuditIntent::accepted("acme", "alice", "DELETE", "/orgs/acme/apps/shop", "test");
    let e = s
        .state
        .store
        .delete_app("acme", "shop", as_alice)
        .await
        .unwrap_err();
    assert!(
        matches!(e, openporch_store::StoreError::Forbidden(_)),
        "{e}"
    );
    assert_eq!(
        s.get("/orgs/acme/apps/shop", &s.admin).await.0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn creation_and_revocation_are_rechecked_with_the_change() {
    let s = server().await;
    s.member("carol", "member").await;
    s.member("ci", "member").await;
    s.token_for("ci").await;
    let as_carol =
        |method: &str, path: &str| AuditIntent::accepted("acme", "carol", method, path, "test");
    let store = &s.state.store;
    let forbidden = |e: openporch_store::StoreError| {
        assert!(
            matches!(e, openporch_store::StoreError::Forbidden(_)),
            "{e}"
        );
    };
    // carol was demoted from manager after her requests passed the route checks.
    forbidden(
        store
            .create_app(
                "acme",
                "shop",
                "Shop",
                "carol",
                as_carol("POST", "/orgs/acme/apps"),
            )
            .await
            .unwrap_err(),
    );
    forbidden(
        store
            .create_env_type(
                "acme",
                "staging",
                "",
                as_carol("POST", "/orgs/acme/env-types"),
            )
            .await
            .unwrap_err(),
    );
    let (_, tokens) = s.get("/orgs/acme/users/ci/tokens", &s.admin).await;
    let token = tokens[0]["id"].as_str().unwrap().to_owned();
    forbidden(
        store
            .revoke_token(
                "acme",
                "ci",
                &token,
                as_carol("DELETE", "/orgs/acme/users/ci/tokens"),
            )
            .await
            .unwrap_err(),
    );
    assert_eq!(
        s.get("/orgs/acme/apps/shop", &s.admin).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn other_organizations_are_hidden_before_routing() {
    let s = server().await;
    for (method, path) in [
        (Method::DELETE, "/orgs/other/audit-logs"),
        (Method::PUT, "/orgs/other/apps"),
        (Method::GET, "/orgs/other/nowhere"),
    ] {
        let (status, body) = s.call(method, path, None, None).await;
        assert_error(status, &body, StatusCode::NOT_FOUND, "API-001");
    }
    let (status, _) = s
        .call(Method::DELETE, "/orgs/acme/audit-logs", None, None)
        .await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn stale_bootstrap_file_stops_a_new_installation_without_changes() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("bootstrap-token"), "opt_stale\n").unwrap();
    let e = start(Config::new("acme", dir.path()), None)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("remove the stale file"), "{e}");
    // After removing it, the next start bootstraps normally.
    std::fs::remove_file(dir.path().join("bootstrap-token")).unwrap();
    let s = server_in(dir).await;
    assert_eq!(
        s.get("/orgs/acme/current-user", &s.admin).await.0,
        StatusCode::OK
    );
}
