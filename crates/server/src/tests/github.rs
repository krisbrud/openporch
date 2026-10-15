//! GitHub device login against a mocked GitHub: immutable ID mapping, renamed and
//! reused logins, verified emails, invitations by login and revocation.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use super::{Server, assert_error, server, server_with};
use crate::Config;
use crate::config::GithubConfig;

const CLIENT_ID: &str = "Iv1.test";
const CLIENT_SECRET: &str = "oauth-client-secret";
const CALLBACK_URL: &str = "http://127.0.0.1:8080/auth/github/callback";
const PORTAL: &str = "http://localhost:3000/login/done";

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
    /// Authorization codes of browser logins, by the account that signed in.
    codes: HashMap<String, u64>,
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
    let mut m = m.lock().unwrap();
    m.next += 1;
    let token = format!("gho_secret{}", m.next);
    if let Some(code) = form.get("code") {
        // The authorization-code flow: the secret and the registered callback.
        assert_eq!(form["client_secret"], CLIENT_SECRET);
        assert_eq!(form["redirect_uri"], CALLBACK_URL);
        return Json(match m.codes.remove(code) {
            Some(id) => {
                m.tokens.insert(token.clone(), id);
                json!({"access_token": token, "token_type": "bearer"})
            }
            None => json!({"error": "bad_verification_code"}),
        });
    }
    assert_eq!(
        form["grant_type"],
        "urn:ietf:params:oauth:grant-type:device_code"
    );
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
            browser: None,
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

impl Github {
    /// GitHub approves a browser login by account `id`: an authorization code.
    fn authorization_code(&self, id: u64) -> String {
        let mut m = self.mock.lock().unwrap();
        m.next += 1;
        let code = format!("code{}", m.next);
        m.codes.insert(code.clone(), id);
        code
    }

    /// A server with browser login, its client secret in an owner-only file.
    async fn browser_server(&self) -> Server {
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("github-client-secret");
        std::fs::write(&secret, format!("{CLIENT_SECRET}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let mut config = Config::new("acme", dir.path());
        config.github = Some(GithubConfig {
            client_id: CLIENT_ID.into(),
            api_url: self.base.clone(),
            web_url: self.base.clone(),
            browser: Some(crate::config::GithubBrowserConfig {
                client_secret_file: secret,
                callback_url: CALLBACK_URL.into(),
                return_urls: vec![PORTAL.into(), "https://portal.example.com/done".into()],
            }),
        });
        server_with(dir, config).await
    }
}

/// A browser request: the status, the `Location` and the `Set-Cookie` headers.
async fn navigate(s: &Server, path: &str, cookie: Option<&str>) -> (StatusCode, String, String) {
    use tower::ServiceExt as _;
    let mut req = axum::http::Request::builder().uri(path);
    if let Some(c) = cookie {
        req = req.header("cookie", c);
    }
    let res = s
        .router
        .clone()
        .oneshot(req.body(axum::body::Body::empty()).unwrap())
        .await
        .unwrap();
    let header = |name| {
        res.headers()
            .get(name)
            .map(|v: &axum::http::HeaderValue| v.to_str().unwrap().to_owned())
            .unwrap_or_default()
    };
    (res.status(), header("location"), header("set-cookie"))
}

fn query(url: &str) -> HashMap<String, String> {
    reqwest::Url::parse(url)
        .unwrap()
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

/// Starts a browser login: the `state` GitHub is sent and the cookie that binds it.
async fn start_browser(s: &Server, return_to: &str) -> (String, String) {
    let path = format!("/auth/github/authorize?return_to={}", url_escape(return_to));
    let (status, location, cookie) = navigate(s, &path, None).await;
    assert_eq!(status, StatusCode::FOUND);
    assert!(location.contains("login/oauth/authorize?"), "{location}");
    let q = query(&location);
    assert_eq!(q["client_id"], CLIENT_ID);
    assert_eq!(q["redirect_uri"], CALLBACK_URL);
    assert_eq!(q["scope"], "read:user user:email");
    assert!(!location.contains(CLIENT_SECRET));
    for attr in [
        "HttpOnly",
        "SameSite=Lax",
        "Path=/auth/github/callback",
        "Max-Age=600",
    ] {
        assert!(cookie.contains(attr), "{cookie}");
    }
    let binding = cookie.split(';').next().unwrap().to_owned();
    (q["state"].clone(), binding)
}

fn url_escape(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[tokio::test]
async fn browser_login_returns_a_one_time_code_for_a_session() {
    let gh = Github::start().await;
    gh.account(583231, "octocat", &[("octo@example.com", true, true)]);
    let s = gh.browser_server().await;
    let (_, invitation) = s
        .post(
            "/orgs/acme/invitations",
            &s.admin,
            json!({"role": "member", "provider": "github", "login": "octocat"}),
        )
        .await;
    let invitation = invitation["code"].as_str().unwrap().to_owned();

    // GitHub sends the browser back with the state and a code.
    let (state, cookie) = start_browser(&s, PORTAL).await;
    let code = gh.authorization_code(583231);
    let (status, location, cleared) = navigate(
        &s,
        &format!("/auth/github/callback?code={code}&state={state}"),
        Some(&cookie),
    )
    .await;
    assert_eq!(status, StatusCode::FOUND);
    assert!(location.starts_with(PORTAL), "{location}");
    assert!(cleared.contains("Max-Age=0"), "{cleared}");
    let login_code = query(&location)["login_code"].clone();
    assert!(!location.contains("gho_"));

    // The portal exchanges the code once, with the invitation on a first login.
    let (status, session) = s
        .public_post(
            "/auth/github/session",
            json!({"login_code": login_code, "invitation_code": invitation}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{session}");
    assert_eq!(session["user"], "octocat");
    assert!(!session.to_string().contains("gho_"));
    let token = session["token"].as_str().unwrap();
    let (status, me) = s.get("/orgs/acme/current-user", token).await;
    assert_eq!(status, StatusCode::OK, "{me}");
    assert_eq!(me["id"], "octocat");
    let (status, b) = s
        .public_post("/auth/github/session", json!({"login_code": login_code}))
        .await;
    assert_error(status, &b, StatusCode::UNAUTHORIZED, "AUTH-001");

    // The state was spent with the callback.
    let (status, _, _) = navigate(
        &s,
        &format!(
            "/auth/github/callback?code={}&state={state}",
            gh.authorization_code(583231)
        ),
        Some(&cookie),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Later logins of the same account need no invitation.
    let (state, cookie) = start_browser(&s, "https://portal.example.com/done").await;
    let (_, location, _) = navigate(
        &s,
        &format!(
            "/auth/github/callback?code={}&state={state}",
            gh.authorization_code(583231)
        ),
        Some(&cookie),
    )
    .await;
    assert!(location.starts_with("https://portal.example.com/done?login_code="));
    let (status, session) = s
        .public_post(
            "/auth/github/session",
            json!({"login_code": query(&location)["login_code"]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{session}");
    assert!(
        s.audit_actions()
            .await
            .iter()
            .any(|e| e["action"] == "session.create" && e["path"] == "/auth/github/session")
    );
}

#[tokio::test]
async fn browser_login_refuses_forged_callbacks_and_unknown_return_urls() {
    let gh = Github::start().await;
    gh.account(583231, "octocat", &[]);
    gh.account(1, "mallory", &[]);
    let s = gh.browser_server().await;

    // Only the configured return pages, named exactly.
    for bad in [
        "https://evil.example.com/",
        "http://localhost:3000/login/done/x",
        "http://localhost:3000/login/done?next=https://evil.example.com",
    ] {
        let path = format!("/auth/github/authorize?return_to={}", url_escape(bad));
        let (status, location, _) = navigate(&s, &path, None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
        assert!(location.is_empty());
    }
    // With two configured, one must be named.
    let (status, _, _) = navigate(&s, "/auth/github/authorize", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // A malformed query gets the usual JSON error.
    for path in [
        "/auth/github/authorize?return_to=a&return_to=b",
        "/auth/github/callback?state=a&state=b",
    ] {
        let (status, body) = s.call(Method::GET, path, None, None).await;
        assert_error(
            status,
            &body,
            StatusCode::BAD_REQUEST,
            crate::error::code::INVALID_REQUEST,
        );
    }

    // Login CSRF: Mallory starts a login and sends the victim the callback with
    // her own code. The victim's browser lacks her cookie, so it is refused.
    let (state, _mallory_cookie) = start_browser(&s, PORTAL).await;
    let (victim_state, victim_cookie) = start_browser(&s, PORTAL).await;
    let forged = format!(
        "/auth/github/callback?code={}&state={state}",
        gh.authorization_code(1)
    );
    for cookie in [None, Some(victim_cookie.as_str())] {
        let (status, location, _) = navigate(&s, &forged, cookie).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(location.is_empty());
    }
    // An unknown state is refused too.
    let (status, _, _) = navigate(
        &s,
        &format!("/auth/github/callback?code=x&state={}", "0".repeat(64)),
        Some(&victim_cookie),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // A declined login, and a code GitHub does not confirm, return with an error.
    let (status, location, _) = navigate(
        &s,
        &format!("/auth/github/callback?error=access_denied&state={victim_state}"),
        Some(&victim_cookie),
    )
    .await;
    assert_eq!(status, StatusCode::FOUND);
    assert_eq!(query(&location)["error"], "access_denied");
    let (state, cookie) = start_browser(&s, PORTAL).await;
    let (_, location, _) = navigate(
        &s,
        &format!("/auth/github/callback?code=unknown&state={state}"),
        Some(&cookie),
    )
    .await;
    assert_eq!(query(&location)["error"], "login_failed");

    // An unknown GitHub account still needs an invitation.
    let (state, cookie) = start_browser(&s, PORTAL).await;
    let (_, location, _) = navigate(
        &s,
        &format!(
            "/auth/github/callback?code={}&state={state}",
            gh.authorization_code(583231)
        ),
        Some(&cookie),
    )
    .await;
    let (status, b) = s
        .public_post(
            "/auth/github/session",
            json!({"login_code": query(&location)["login_code"]}),
        )
        .await;
    assert_error(status, &b, StatusCode::UNAUTHORIZED, "AUTH-001");
    assert!(b["message"].as_str().unwrap().contains("invitation"), "{b}");
}

#[tokio::test]
async fn browser_login_needs_its_configuration() {
    let gh = Github::start().await;
    // Device login only: the browser routes do not exist.
    let s = gh.server().await;
    let (status, _, _) = navigate(&s, "/auth/github/authorize", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, b) = s
        .public_post("/auth/github/session", json!({"login_code": "x"}))
        .await;
    assert_error(status, &b, StatusCode::NOT_FOUND, "API-001");
    let (_, version) = s
        .call(axum::http::Method::GET, "/version", None, None)
        .await;
    assert_eq!(
        version["capabilities"]["identity_providers"],
        json!([{"id": "github", "flows": ["device"]}])
    );
    let s = gh.browser_server().await;
    let (_, version) = s
        .call(axum::http::Method::GET, "/version", None, None)
        .await;
    assert_eq!(
        version["capabilities"]["identity_providers"],
        json!([{"id": "github", "flows": ["device", "browser"]}])
    );

    // A client secret others may read stops the server from starting.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("secret");
        std::fs::write(&secret, CLIENT_SECRET).unwrap();
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o644)).unwrap();
        let mut config = Config::new("acme", dir.path());
        config.github = Some(GithubConfig {
            client_id: CLIENT_ID.into(),
            api_url: gh.base.clone(),
            web_url: gh.base.clone(),
            browser: Some(crate::config::GithubBrowserConfig {
                client_secret_file: secret,
                callback_url: CALLBACK_URL.into(),
                return_urls: vec![PORTAL.into()],
            }),
        });
        let err = crate::start(config, None).await.unwrap_err();
        assert!(err.to_string().contains("chmod 600"), "{err}");
        assert!(!err.to_string().contains(CLIENT_SECRET));
    }
}
