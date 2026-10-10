//! GitHub device login against a mocked GitHub: immutable ID mapping, renamed and
//! reused logins, verified emails, invitations by login and revocation.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use super::{Server, assert_error, server, server_with};
use crate::Config;
use crate::config::GithubConfig;

const CLIENT_ID: &str = "Iv1.test";

#[derive(Clone)]
struct Account {
    id: u64,
    login: String,
    /// `(email, primary, verified)`.
    emails: Vec<(String, bool, bool)>,
}

enum Device {
    Pending,
    Approved(u64),
    Denied,
    Used,
}

#[derive(Default)]
struct Mock {
    accounts: Vec<Account>,
    devices: HashMap<String, Device>,
    tokens: HashMap<String, u64>,
    next: u64,
}

type Shared = Arc<Mutex<Mock>>;

fn account_json(a: &Account) -> Value {
    let kind = if a.login.starts_with("org-") {
        "Organization"
    } else {
        "User"
    };
    json!({"id": a.id, "login": a.login, "name": null, "type": kind})
}

fn bearer_account(mock: &Mock, headers: &HeaderMap) -> Option<Account> {
    let token = headers
        .get("authorization")?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")?;
    let id = mock.tokens.get(token)?;
    mock.accounts.iter().find(|a| a.id == *id).cloned()
}

async fn device_code(
    State(m): State<Shared>,
    Form(form): Form<HashMap<String, String>>,
) -> Json<Value> {
    assert_eq!(form["client_id"], CLIENT_ID);
    assert_eq!(form["scope"], "read:user user:email");
    let mut m = m.lock().unwrap();
    m.next += 1;
    let code = format!("dev-{}", m.next);
    m.devices.insert(code.clone(), Device::Pending);
    Json(json!({
        "device_code": code,
        "user_code": format!("ABCD-{:04}", m.next),
        "verification_uri": "https://github.com/login/device",
        "expires_in": 900,
        "interval": 5,
    }))
}

async fn access_token(
    State(m): State<Shared>,
    Form(form): Form<HashMap<String, String>>,
) -> Json<Value> {
    assert_eq!(form["client_id"], CLIENT_ID);
    assert_eq!(
        form["grant_type"],
        "urn:ietf:params:oauth:grant-type:device_code"
    );
    let mut m = m.lock().unwrap();
    m.next += 1;
    let token = format!("gho_secret{}", m.next);
    let reply = match m.devices.get_mut(&form["device_code"]) {
        None => json!({"error": "incorrect_device_code"}),
        Some(Device::Pending) => json!({"error": "authorization_pending", "interval": 5}),
        Some(Device::Denied) => json!({"error": "access_denied"}),
        Some(Device::Used) => json!({"error": "bad_verification_code"}),
        Some(d @ Device::Approved(_)) => {
            let Device::Approved(id) = std::mem::replace(d, Device::Used) else {
                unreachable!()
            };
            m.tokens.insert(token.clone(), id);
            json!({"access_token": token, "token_type": "bearer", "scope": "read:user,user:email"})
        }
    };
    Json(reply)
}

async fn user(State(m): State<Shared>, headers: HeaderMap) -> Result<Json<Value>, StatusCode> {
    let m = m.lock().unwrap();
    let a = bearer_account(&m, &headers).ok_or(StatusCode::UNAUTHORIZED)?;
    Ok(Json(account_json(&a)))
}

async fn emails(
    State(m): State<Shared>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, StatusCode> {
    let m = m.lock().unwrap();
    let a = bearer_account(&m, &headers).ok_or(StatusCode::UNAUTHORIZED)?;
    let per_page: usize = q.get("per_page").map_or(30, |v| v.parse().unwrap());
    let page: usize = q.get("page").map_or(1, |v| v.parse().unwrap());
    Ok(Json(Value::Array(
        a.emails
            .iter()
            .skip((page - 1) * per_page)
            .take(per_page)
            .map(|(e, p, v)| json!({"email": e, "primary": p, "verified": v, "visibility": null}))
            .collect(),
    )))
}

async fn user_by_login(
    State(m): State<Shared>,
    Path(login): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    let m = m.lock().unwrap();
    m.accounts
        .iter()
        .find(|a| a.login.eq_ignore_ascii_case(&login))
        .map(|a| Json(account_json(a)))
        .ok_or(StatusCode::NOT_FOUND)
}

/// A mocked GitHub serving both the OAuth and the REST endpoints.
struct Github {
    base: String,
    mock: Shared,
}

impl Github {
    async fn start() -> Self {
        let mock = Shared::default();
        let app = Router::new()
            .route("/login/device/code", post(device_code))
            .route("/login/oauth/access_token", post(access_token))
            .route("/user", get(user))
            .route("/user/emails", get(emails))
            .route("/users/{login}", get(user_by_login))
            .with_state(mock.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        Self {
            base: format!("http://{addr}/"),
            mock,
        }
    }

    fn account(&self, id: u64, login: &str, emails: &[(&str, bool, bool)]) {
        let mut m = self.mock.lock().unwrap();
        m.accounts.retain(|a| a.id != id);
        m.accounts.push(Account {
            id,
            login: login.into(),
            emails: emails
                .iter()
                .map(|(e, p, v)| ((*e).to_owned(), *p, *v))
                .collect(),
        });
    }

    fn rename(&self, id: u64, login: &str) {
        let mut m = self.mock.lock().unwrap();
        if let Some(a) = m.accounts.iter_mut().find(|a| a.id == id) {
            a.login = login.into();
        }
    }

    fn decide(&self, device_code: &str, decision: Device) {
        self.mock
            .lock()
            .unwrap()
            .devices
            .insert(device_code.into(), decision);
    }

    async fn server(&self) -> Server {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::new("acme", dir.path());
        config.github = Some(GithubConfig {
            client_id: CLIENT_ID.into(),
            api_url: self.base.clone(),
            web_url: self.base.clone(),
        });
        server_with(dir, config).await
    }
}

impl Server {
    async fn public_post(&self, path: &str, body: Value) -> (StatusCode, Value) {
        self.call(axum::http::Method::POST, path, None, Some(body))
            .await
    }

    /// Starts a device login and returns its device code.
    async fn start_device(&self) -> String {
        let (status, b) = self.public_post("/auth/github/device", json!({})).await;
        assert_eq!(status, StatusCode::OK, "{b}");
        assert_eq!(b["verification_uri"], "https://github.com/login/device");
        assert!(b["user_code"].as_str().unwrap().starts_with("ABCD-"));
        b["device_code"].as_str().unwrap().to_owned()
    }

    async fn poll(&self, device_code: &str, invitation: Option<&str>) -> (StatusCode, Value) {
        let mut body = json!({"device_code": device_code});
        if let Some(code) = invitation {
            body["invitation_code"] = json!(code);
        }
        let (status, b) = self.public_post("/auth/github/token", body).await;
        // The GitHub access token never leaves the server.
        assert!(!b.to_string().contains("gho_"), "{b}");
        (status, b)
    }

    /// A complete device login by GitHub account `id`.
    async fn github_login(
        &self,
        gh: &Github,
        id: u64,
        invitation: Option<&str>,
    ) -> (StatusCode, Value) {
        let device = self.start_device().await;
        gh.decide(&device, Device::Approved(id));
        self.poll(&device, invitation).await
    }

    async fn audit_actions(&self) -> Vec<Value> {
        let (_, page) = self.get("/orgs/acme/audit-logs", &self.admin).await;
        page["entries"].as_array().unwrap().clone()
    }
}

#[tokio::test]
async fn device_login_maps_the_immutable_github_id() {
    let gh = Github::start().await;
    gh.account(583231, "octocat", &[("octo@example.com", true, true)]);
    let s = gh.server().await;

    // An invitation by login stores the immutable ID GitHub reports now.
    let (status, inv) = s
        .post(
            "/orgs/acme/invitations",
            &s.admin,
            json!({"provider": "github", "login": "octocat", "role": "member"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{inv}");
    assert_eq!(inv["subject"], "583231");
    let code = inv["code"].as_str().unwrap();

    let device = s.start_device().await;
    let (status, pending) = s.poll(&device, Some(code)).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(pending, json!({"status": "pending", "interval": 5}));

    gh.decide(&device, Device::Approved(583231));
    let (status, session) = s.poll(&device, Some(code)).await;
    assert_eq!(status, StatusCode::OK, "{session}");
    let token = session["token"].as_str().unwrap();
    let now = openporch_store::now_ms();
    let expires = session["expires_at"].as_i64().unwrap();
    assert!((now + crate::auth::SESSION_TTL_MS - expires).abs() < 60_000);
    let (_, me) = s.get("/orgs/acme/current-user", token).await;
    assert_eq!(me["identity"], "github:583231");
    assert_eq!(me["role"], "member");
    assert_eq!(me["email"], "octo@example.com");
    assert_eq!(me["id"], session["user"]);

    // A device code yields one session.
    let (status, b) = s.poll(&device, Some(code)).await;
    assert_error(status, &b, StatusCode::UNAUTHORIZED, "AUTH-001");

    // Later logins need no invitation.
    let (status, again) = s.github_login(&gh, 583231, None).await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["user"], session["user"]);

    let entries = s.audit_actions().await;
    let sessions = entries
        .iter()
        .filter(|e| e["action"] == "session.create" && e["user_id"] == session["user"])
        .count();
    assert_eq!(sessions, 2);
    assert!(entries.iter().any(|e| e["action"] == "invitation.accept"));
    assert!(!Value::Array(entries).to_string().contains("gho_"));
}

#[tokio::test]
async fn renamed_and_reused_github_logins() {
    let gh = Github::start().await;
    gh.account(100, "octocat", &[]);
    let s = gh.server().await;
    let (_, inv) = s
        .post(
            "/orgs/acme/invitations",
            &s.admin,
            json!({"provider": "github", "login": "octocat", "role": "member"}),
        )
        .await;
    let code = inv["code"].as_str().unwrap();

    // The invited account renames itself and another account takes the old name.
    gh.rename(100, "octo-renamed");
    gh.account(200, "octocat", &[]);
    let (status, b) = s.github_login(&gh, 200, Some(code)).await;
    assert_error(status, &b, StatusCode::UNAUTHORIZED, "AUTH-001");
    let failed = s.audit_actions().await;
    let failed = failed
        .iter()
        .find(|e| e["action"] == "login.failed")
        .unwrap();
    assert_eq!(failed["target"], "github:200");
    assert_eq!(failed["outcome"], "denied");

    // The renamed account keeps the invitation and, later, its membership.
    let (status, first) = s.github_login(&gh, 100, Some(code)).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    gh.rename(100, "octo-again");
    let (status, later) = s.github_login(&gh, 100, None).await;
    assert_eq!(status, StatusCode::OK, "{later}");
    assert_eq!(later["user"], first["user"]);
    // The account now named `octocat` is still nobody.
    let (status, b) = s.github_login(&gh, 200, None).await;
    assert_error(status, &b, StatusCode::UNAUTHORIZED, "AUTH-001");
    assert!(
        b["message"]
            .as_str()
            .unwrap()
            .contains("an invitation is required"),
        "{b}"
    );
}

#[tokio::test]
async fn email_invitations_need_a_verified_github_email() {
    let gh = Github::start().await;
    gh.account(8, "bob", &[("bob@example.com", true, false)]);
    let s = gh.server().await;
    let (_, code) = s
        .invite(json!({"email": "bob@example.com", "role": "member"}))
        .await;
    let (status, b) = s.github_login(&gh, 8, Some(&code)).await;
    assert_error(status, &b, StatusCode::UNAUTHORIZED, "AUTH-001");
    // Once GitHub has verified it, the same code works, also as a secondary address
    // on a later page, and the member gets the invited address.
    let others: Vec<String> = (0..150).map(|i| format!("old{i}@example.org")).collect();
    let mut emails = vec![("bob@personal.example", true, true)];
    emails.extend(others.iter().map(|e| (e.as_str(), false, true)));
    emails.push(("bob@unverified.example", false, false));
    emails.push(("bob@example.com", false, true));
    gh.account(8, "bob", &emails);
    let (status, b) = s.github_login(&gh, 8, Some(&code)).await;
    assert_eq!(status, StatusCode::OK, "{b}");
    let (_, me) = s
        .get("/orgs/acme/current-user", b["token"].as_str().unwrap())
        .await;
    assert_eq!(me["email"], "bob@example.com");
}

#[tokio::test]
async fn denied_expired_and_revoked_logins() {
    let gh = Github::start().await;
    gh.account(42, "carol", &[]);
    let s = gh.server().await;

    let device = s.start_device().await;
    gh.decide(&device, Device::Denied);
    let (status, b) = s.poll(&device, None).await;
    assert_error(status, &b, StatusCode::UNAUTHORIZED, "AUTH-001");
    let (status, b) = s.poll("dev-unknown", None).await;
    assert_error(status, &b, StatusCode::UNAUTHORIZED, "AUTH-001");
    let (status, b) = s.poll("../../user", None).await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");

    let (_, inv) = s
        .post(
            "/orgs/acme/invitations",
            &s.admin,
            json!({"provider": "github", "login": "carol", "role": "member"}),
        )
        .await;
    let (_, first) = s
        .github_login(&gh, 42, Some(inv["code"].as_str().unwrap()))
        .await;
    let user = first["user"].as_str().unwrap().to_owned();
    let first = first["token"].as_str().unwrap().to_owned();
    let (_, second) = s.github_login(&gh, 42, None).await;
    let second = second["token"].as_str().unwrap().to_owned();

    // Logging out revokes only that session.
    assert_eq!(
        s.delete("/orgs/acme/sessions/current", &first).await.0,
        StatusCode::NO_CONTENT
    );
    let (status, b) = s.get("/orgs/acme/current-user", &first).await;
    assert_error(status, &b, StatusCode::UNAUTHORIZED, "AUTH-001");
    assert_eq!(
        s.get("/orgs/acme/current-user", &second).await.0,
        StatusCode::OK
    );

    // Removing the member rejects their still-valid signed session at once.
    assert_eq!(
        s.delete(&format!("/orgs/acme/users/{user}"), &s.admin)
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    let (status, b) = s.get("/orgs/acme/current-user", &second).await;
    assert_error(status, &b, StatusCode::UNAUTHORIZED, "AUTH-001");
    // And the GitHub account cannot log back in without a new invitation.
    let (status, b) = s.github_login(&gh, 42, None).await;
    assert_error(status, &b, StatusCode::UNAUTHORIZED, "AUTH-001");
}

#[tokio::test]
async fn invitations_by_login_resolve_through_github() {
    let gh = Github::start().await;
    gh.account(7, "org-acme", &[]);
    let s = gh.server().await;
    let invite = |body: Value| s.post("/orgs/acme/invitations", &s.admin, body);
    let (status, b) =
        invite(json!({"provider": "github", "login": "ghost", "role": "member"})).await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
    // Organizations cannot log in, so they cannot be invited.
    let (status, b) =
        invite(json!({"provider": "github", "login": "org-acme", "role": "member"})).await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
    let (status, b) =
        invite(json!({"provider": "github", "login": "../user", "role": "member"})).await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
    let (status, b) =
        invite(json!({"provider": "oidc", "login": "someone", "role": "member"})).await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
    let (status, b) =
        invite(json!({"email": "a@example.com", "login": "x", "role": "member"})).await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");

    // A manager may not invite an administrator, and GitHub is not consulted.
    let manager = s.member("mgr", "manager").await;
    let (status, b) = s
        .post(
            "/orgs/acme/invitations",
            &manager,
            json!({"provider": "github", "login": "ghost", "role": "administrator"}),
        )
        .await;
    assert_error(status, &b, StatusCode::FORBIDDEN, "AUTH-002");
}

#[tokio::test]
async fn github_login_is_optional_and_failures_are_reported() {
    // Without a provider the login routes are absent and logins cannot be resolved.
    let s = server().await;
    let (status, b) = s.public_post("/auth/github/device", json!({})).await;
    assert_error(status, &b, StatusCode::NOT_FOUND, "API-001");
    let (status, b) = s
        .post(
            "/orgs/acme/invitations",
            &s.admin,
            json!({"provider": "github", "login": "octocat", "role": "member"}),
        )
        .await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");

    // An unreachable GitHub is a 502 that names no internals.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/", listener.local_addr().unwrap());
    drop(listener);
    let gh = Github {
        base,
        mock: Shared::default(),
    };
    let s = gh.server().await;
    let (status, b) = s.public_post("/auth/github/device", json!({})).await;
    assert_error(status, &b, StatusCode::BAD_GATEWAY, "SRV-003");
    assert_eq!(
        b["message"],
        "the identity provider is unavailable; try again"
    );
}
