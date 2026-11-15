//! Generic OpenID Connect login against a local test issuer: PKCE, state and
//! nonce, ID token checks (algorithm, signature, issuer, audience, expiry),
//! verified-email invitations, replays and key rotation.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Form, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD as B64};
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde_json::{Value, json};

use super::{Server, assert_error, server_with};
use crate::Config;
use crate::config::OidcLoginConfig;
use crate::oidc_login::code_challenge;

const CLIENT_ID: &str = "openporch-test";
const CLIENT_SECRET: &str = "oidc client secret";
const CALLBACK_URL: &str = "http://127.0.0.1:8080/auth/oidc/corp/callback";
const PORTAL: &str = "http://localhost:3000/login/done";

/// A signing key of the test issuer.
struct Key {
    kid: String,
    pair: RsaKeyPair,
}

impl Key {
    fn new(kid: &str) -> Self {
        let der = openporch_engine::oidc::generate_key().unwrap();
        Self {
            kid: kid.into(),
            pair: RsaKeyPair::from_pkcs8(&der).unwrap(),
        }
    }

    fn jwk(&self) -> Value {
        let public = ring::rsa::PublicKeyComponents::<Vec<u8>>::from(self.pair.public());
        json!({"kty": "RSA", "use": "sig", "alg": "RS256", "kid": self.kid,
               "n": B64.encode(&public.n), "e": B64.encode(&public.e)})
    }

    fn sign(&self, header: &Value, claims: &Value) -> String {
        let input = format!(
            "{}.{}",
            B64.encode(header.to_string()),
            B64.encode(claims.to_string())
        );
        let mut sig = vec![0u8; self.pair.public().modulus_len()];
        self.pair
            .sign(
                &RSA_PKCS1_SHA256,
                &ring::rand::SystemRandom::new(),
                input.as_bytes(),
                &mut sig,
            )
            .unwrap();
        format!("{input}.{}", B64.encode(sig))
    }
}

/// What the person approved: the code's PKCE challenge and redirect, and the ID
/// token the token endpoint returns for it.
struct Grant {
    challenge: String,
    redirect_uri: String,
    id_token: String,
}

#[derive(Default)]
struct Idp {
    issuer: String,
    keys: Vec<Arc<Key>>,
    grants: HashMap<String, Grant>,
    /// Client credentials each code exchange presented.
    clients: Vec<String>,
}

type Shared = Arc<Mutex<Idp>>;

async fn discovery(State(idp): State<Shared>) -> Json<Value> {
    let issuer = idp.lock().unwrap().issuer.clone();
    Json(json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}/authorize"),
        "token_endpoint": format!("{issuer}/token"),
        "jwks_uri": format!("{issuer}/jwks"),
        "response_types_supported": ["code"],
        "code_challenge_methods_supported": ["S256"],
    }))
}

async fn jwks(State(idp): State<Shared>) -> Json<Value> {
    let keys: Vec<Value> = idp.lock().unwrap().keys.iter().map(|k| k.jwk()).collect();
    Json(json!({ "keys": keys }))
}

/// The token endpoint: a code is used once, with the verifier of its challenge
/// and its redirect, by the client that authenticates with the secret.
async fn token(
    State(idp): State<Shared>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> (StatusCode, Json<Value>) {
    let refuse = |error: &str| (StatusCode::BAD_REQUEST, Json(json!({ "error": error })));
    let mut idp = idp.lock().unwrap();
    let basic = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
        .and_then(|v| STANDARD.decode(v).ok())
        .and_then(|v| String::from_utf8(v).ok())
        .unwrap_or_default();
    idp.clients.push(basic.clone());
    if basic != "openporch-test:oidc+client+secret" {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid_client"})),
        );
    }
    if form.get("grant_type").map(String::as_str) != Some("authorization_code") {
        return refuse("unsupported_grant_type");
    }
    let Some(grant) = form.get("code").and_then(|c| idp.grants.remove(c)) else {
        return refuse("invalid_grant");
    };
    let verifier = form.get("code_verifier").cloned().unwrap_or_default();
    if code_challenge(&verifier) != grant.challenge
        || form.get("redirect_uri") != Some(&grant.redirect_uri)
    {
        return refuse("invalid_grant");
    }
    (
        StatusCode::OK,
        Json(
            json!({"access_token": "idp-access-token", "token_type": "Bearer",
                    "id_token": grant.id_token}),
        ),
    )
}

/// A local OpenID Connect issuer.
#[derive(Clone)]
struct Issuer {
    url: String,
    idp: Shared,
}

/// The parameters an authorization request carried.
struct Request {
    state: String,
    nonce: String,
    challenge: String,
    redirect_uri: String,
}

impl Issuer {
    async fn start() -> Self {
        let idp = Shared::default();
        let app = Router::new()
            .route("/.well-known/openid-configuration", get(discovery))
            .route("/jwks", get(jwks))
            .route("/token", post(token))
            .with_state(idp.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        {
            let mut i = idp.lock().unwrap();
            i.issuer = url.clone();
            i.keys.push(Arc::new(Key::new("k1")));
        }
        Self { url, idp }
    }

    fn config(&self, secret: &std::path::Path) -> OidcLoginConfig {
        OidcLoginConfig {
            id: "corp".into(),
            issuer: self.url.clone(),
            client_id: CLIENT_ID.into(),
            client_secret_file: Some(secret.to_owned()),
            callback_url: CALLBACK_URL.into(),
            return_urls: vec![PORTAL.into(), "https://portal.example.com/done".into()],
            scopes: vec!["openid".into(), "email".into(), "profile".into()],
            algorithms: vec!["RS256".into()],
        }
    }

    async fn server(&self) -> Server {
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("oidc-secret");
        std::fs::write(&secret, format!("{CLIENT_SECRET}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let mut config = Config::new("acme", dir.path());
        config.oidc_login = vec![self.config(&secret)];
        server_with(dir, config).await
    }

    /// The claims of an ID token for `sub` answering `req`.
    fn claims(&self, req: &Request, sub: &str) -> Value {
        let now = openporch_store::now_ms() / 1000;
        json!({
            "iss": self.url, "sub": sub, "aud": CLIENT_ID, "iat": now, "exp": now + 300,
            "nonce": req.nonce, "email": format!("{sub}@example.com"), "email_verified": true,
            "name": format!("User {sub}"), "preferred_username": sub,
        })
    }

    fn key(&self) -> Arc<Key> {
        self.idp.lock().unwrap().keys[0].clone()
    }

    /// Approves `req` with an ID token signed by `key` and returns its code.
    fn approve_token(&self, req: &Request, id_token: String) -> String {
        let code = format!("code/{}", crate::auth::random_id(8));
        self.idp.lock().unwrap().grants.insert(
            code.clone(),
            Grant {
                challenge: req.challenge.clone(),
                redirect_uri: req.redirect_uri.clone(),
                id_token,
            },
        );
        code
    }

    fn approve_claims(&self, req: &Request, claims: &Value) -> String {
        let key = self.key();
        let header = json!({"alg": "RS256", "typ": "JWT", "kid": key.kid});
        self.approve_token(req, key.sign(&header, claims))
    }

    fn approve(&self, req: &Request, sub: &str) -> String {
        self.approve_claims(req, &self.claims(req, sub))
    }

    /// Replaces the signing keys with a new one.
    fn rotate(&self, kid: &str) {
        self.idp.lock().unwrap().keys = vec![Arc::new(Key::new(kid))];
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

fn escape(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Starts a login: what the provider was sent, and the cookie that binds it.
async fn start(s: &Server, idp: &Issuer, return_to: &str) -> (Request, String) {
    let path = format!("/auth/oidc/corp/authorize?return_to={}", escape(return_to));
    let (status, location, cookie) = navigate(s, &path, None).await;
    assert_eq!(status, StatusCode::FOUND, "{location}");
    assert!(
        location.starts_with(&format!("{}/authorize?", idp.url)),
        "{location}"
    );
    let q = query(&location);
    assert_eq!(q["response_type"], "code");
    assert_eq!(q["client_id"], CLIENT_ID);
    assert_eq!(q["redirect_uri"], CALLBACK_URL);
    assert_eq!(q["scope"], "openid email profile");
    assert_eq!(q["code_challenge_method"], "S256");
    assert_eq!(q["code_challenge"].len(), 43);
    assert!(q["nonce"].len() >= 32 && q["state"].len() >= 32);
    assert!(!location.contains("client+secret") && !location.contains("client%20secret"));
    let name = format!(
        "openporch_oidc_corp_{}=",
        &crate::auth::hash_token(&q["state"])[..16]
    );
    assert!(cookie.starts_with(&name), "{cookie}");
    for attr in [
        "HttpOnly",
        "SameSite=Lax",
        "Path=/auth/oidc/corp/callback",
        "Max-Age=600",
    ] {
        assert!(cookie.contains(attr), "{cookie}");
    }
    let binding = cookie.split(';').next().unwrap().to_owned();
    (
        Request {
            state: q["state"].clone(),
            nonce: q["nonce"].clone(),
            challenge: q["code_challenge"].clone(),
            redirect_uri: q["redirect_uri"].clone(),
        },
        binding,
    )
}

/// The provider's redirect back with `code`: where the browser is sent.
async fn callback(s: &Server, req: &Request, code: &str, cookie: &str) -> (StatusCode, String) {
    let path = format!(
        "/auth/oidc/corp/callback?code={}&state={}",
        escape(code),
        req.state
    );
    let (status, location, _) = navigate(s, &path, Some(cookie)).await;
    (status, location)
}

async fn exchange(s: &Server, login_code: &str, invitation: Option<&str>) -> (StatusCode, Value) {
    let mut body = json!({ "login_code": login_code });
    if let Some(code) = invitation {
        body["invitation_code"] = json!(code);
    }
    s.call(Method::POST, "/auth/oidc/corp/session", None, Some(body))
        .await
}

/// A whole login of `sub`: the session response.
async fn login(
    s: &Server,
    idp: &Issuer,
    sub: &str,
    invitation: Option<&str>,
) -> (StatusCode, Value) {
    let (req, cookie) = start(s, idp, PORTAL).await;
    let code = idp.approve(&req, sub);
    let (status, location) = callback(s, &req, &code, &cookie).await;
    assert_eq!(status, StatusCode::FOUND);
    let q = query(&location);
    let login_code = q.get("login_code").unwrap_or_else(|| panic!("{location}"));
    exchange(s, login_code, invitation).await
}

#[tokio::test]
async fn oidc_login_maps_issuer_and_subject_and_accepts_verified_email_invitations() {
    let idp = Issuer::start().await;
    let s = idp.server().await;
    let (_, version) = s.call(Method::GET, "/version", None, None).await;
    assert_eq!(
        version["capabilities"]["identity_providers"],
        json!([{"id": "corp", "type": "oidc", "issuer": idp.url, "flows": ["browser"]}])
    );
    let (_, invitation) = s
        .invite(json!({"email": "alice@example.com", "role": "member"}))
        .await;

    // An unknown identity needs an invitation.
    let (status, b) = login(&s, &idp, "alice", None).await;
    assert_error(status, &b, StatusCode::UNAUTHORIZED, "AUTH-001");
    assert!(b["message"].as_str().unwrap().contains("invitation"), "{b}");

    let (req, cookie) = start(&s, &idp, PORTAL).await;
    let code = idp.approve(&req, "alice");
    let (status, location) = callback(&s, &req, &code, &cookie).await;
    assert_eq!(status, StatusCode::FOUND);
    assert!(location.starts_with(PORTAL), "{location}");
    assert!(!location.contains("idp-access-token") && !location.contains("code%2F"));
    let login_code = query(&location)["login_code"].clone();
    let (status, session) = exchange(&s, &login_code, Some(&invitation)).await;
    assert_eq!(status, StatusCode::OK, "{session}");
    assert_eq!(session["user"], "alice");
    assert!(!session.to_string().contains("idp-access-token"));
    let token = session["token"].as_str().unwrap();
    let (status, me) = s.get("/orgs/acme/current-user", token).await;
    assert_eq!(status, StatusCode::OK, "{me}");
    assert_eq!(me["email"], "alice@example.com");
    assert_eq!(me["identity"], format!("{}:alice", idp.url));
    // The client authenticated with its secret, form-encoded for Basic auth.
    assert!(
        idp.idp
            .lock()
            .unwrap()
            .clients
            .iter()
            .all(|c| c == "openporch-test:oidc+client+secret")
    );

    // A login code and a state are used once.
    let (status, b) = exchange(&s, &login_code, None).await;
    assert_error(status, &b, StatusCode::UNAUTHORIZED, "AUTH-001");
    let again = idp.approve(&req, "alice");
    let (status, _) = callback(&s, &req, &again, &cookie).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Later logins need no invitation.
    let (status, session) = login(&s, &idp, "alice", None).await;
    assert_eq!(status, StatusCode::OK, "{session}");
    assert_eq!(session["user"], "alice");
    let (_, page) = s.get("/orgs/acme/audit-logs", &s.admin).await;
    assert!(
        page["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["action"] == "session.create" && e["path"] == "/auth/oidc/corp/session")
    );
}

#[tokio::test]
async fn unverified_emails_do_not_accept_email_invitations() {
    let idp = Issuer::start().await;
    let s = idp.server().await;
    let (_, invitation) = s
        .invite(json!({"email": "bob@example.com", "role": "member"}))
        .await;
    for verified in [json!(false), Value::Null] {
        let (req, cookie) = start(&s, &idp, PORTAL).await;
        let mut claims = idp.claims(&req, "bob");
        claims["email_verified"] = verified;
        let code = idp.approve_claims(&req, &claims);
        let (_, location) = callback(&s, &req, &code, &cookie).await;
        let (status, b) = exchange(&s, &query(&location)["login_code"], Some(&invitation)).await;
        assert_error(status, &b, StatusCode::UNAUTHORIZED, "AUTH-001");
    }
    // The invitation is still open for the verified address.
    let (status, session) = login(&s, &idp, "bob", Some(&invitation)).await;
    assert_eq!(status, StatusCode::OK, "{session}");
}

#[tokio::test]
async fn id_tokens_failing_a_check_create_no_session() {
    let idp = Issuer::start().await;
    let s = idp.server().await;
    let (_, invitation) = s
        .invite(json!({"email": "carol@example.com", "role": "member"}))
        .await;
    let other = Key::new("unpublished");
    let key = idp.key();
    let now = openporch_store::now_ms() / 1000;
    type Case = Box<dyn Fn(&Request, &mut Value) -> Option<String>>;
    fn edit(f: impl Fn(&mut Value) + 'static) -> Case {
        Box::new(move |_, c| {
            f(c);
            None
        })
    }
    let cases: Vec<(&str, Case)> = vec![
        (
            "another audience",
            edit(|c| c["aud"] = json!("someone-else")),
        ),
        (
            "another audience next to ours, without azp",
            edit(|c| c["aud"] = json!([CLIENT_ID, "someone-else"])),
        ),
        (
            "issued to another client",
            edit(|c| c["azp"] = json!("someone-else")),
        ),
        (
            "another issuer",
            edit(|c| c["iss"] = json!("https://evil.example")),
        ),
        ("expired", edit(move |c| c["exp"] = json!(now - 120))),
        (
            "issued in the future",
            edit(move |c| c["iat"] = json!(now + 600)),
        ),
        ("not valid yet", edit(move |c| c["nbf"] = json!(now + 600))),
        (
            "without exp",
            edit(|c| drop(c.as_object_mut().unwrap().remove("exp"))),
        ),
        (
            "another login's nonce",
            edit(|c| c["nonce"] = json!("replayed-nonce")),
        ),
        (
            "without a nonce",
            edit(|c| drop(c.as_object_mut().unwrap().remove("nonce"))),
        ),
        ("without a subject", edit(|c| c["sub"] = json!(""))),
        (
            "unsigned",
            Box::new(|_, c| {
                Some(format!(
                    "{}.{}.",
                    B64.encode(json!({"alg": "none"}).to_string()),
                    B64.encode(c.to_string())
                ))
            }),
        ),
        (
            "an algorithm that is not allowed",
            Box::new(move |_, c| Some(key.sign(&json!({"alg": "HS256", "kid": key.kid}), c))),
        ),
        (
            "an unpublished key",
            Box::new(move |_, c| Some(other.sign(&json!({"alg": "RS256", "kid": "k1"}), c))),
        ),
        (
            "tampered claims",
            Box::new({
                let key = idp.key();
                move |_, c| {
                    let token = key.sign(&json!({"alg": "RS256", "kid": key.kid}), c);
                    let mut parts: Vec<String> = token.split('.').map(str::to_owned).collect();
                    c["sub"] = json!("mallory");
                    parts[1] = B64.encode(c.to_string());
                    Some(parts.join("."))
                }
            }),
        ),
    ];
    for (name, case) in cases {
        let (req, cookie) = start(&s, &idp, PORTAL).await;
        let mut claims = idp.claims(&req, "carol");
        let code = match case(&req, &mut claims) {
            Some(token) => idp.approve_token(&req, token),
            None => idp.approve_claims(&req, &claims),
        };
        let (status, location) = callback(&s, &req, &code, &cookie).await;
        assert_eq!(status, StatusCode::FOUND, "{name}");
        let q = query(&location);
        assert_eq!(
            q.get("error").map(String::as_str),
            Some("login_failed"),
            "{name}: {location}"
        );
        assert!(!q.contains_key("login_code"), "{name}");
    }

    // A code approved for another login's PKCE challenge is refused by the
    // provider, since the server sends this login's verifier.
    let (req, cookie) = start(&s, &idp, PORTAL).await;
    let (other_req, _) = start(&s, &idp, PORTAL).await;
    let mut mixed = Request {
        challenge: other_req.challenge.clone(),
        ..req
    };
    let code = idp.approve(&mixed, "carol");
    let (_, location) = callback(&s, &mixed, &code, &cookie).await;
    assert_eq!(query(&location)["error"], "login_failed");

    // A response naming another issuer is refused before the code is used.
    let (req, cookie) = start(&s, &idp, PORTAL).await;
    mixed = req;
    let code = idp.approve(&mixed, "carol");
    let path = format!(
        "/auth/oidc/corp/callback?code={}&state={}&iss={}",
        escape(&code),
        mixed.state,
        escape("https://evil.example")
    );
    let (_, location, _) = navigate(&s, &path, Some(&cookie)).await;
    assert_eq!(query(&location)["error"], "login_failed");
    assert!(idp.idp.lock().unwrap().grants.contains_key(&code));

    // Nobody was created along the way, and carol's invitation still works.
    let (status, session) = login(&s, &idp, "carol", Some(&invitation)).await;
    assert_eq!(status, StatusCode::OK, "{session}");
    let (_, users) = s.get("/orgs/acme/users", &s.admin).await;
    assert!(!users.to_string().contains("mallory"), "{users}");
}

#[tokio::test]
async fn forged_callbacks_unknown_providers_and_return_urls_are_refused() {
    let idp = Issuer::start().await;
    let s = idp.server().await;
    let (req, cookie) = start(&s, &idp, PORTAL).await;
    let code = idp.approve(&req, "dave");
    // Without the cookie, or with another login's, the state is refused and spent.
    let (other, other_cookie) = start(&s, &idp, PORTAL).await;
    let (status, _) = callback(&s, &req, &code, &other_cookie).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = callback(&s, &req, &code, &cookie).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _, _) = navigate(
        &s,
        &format!("/auth/oidc/corp/callback?code=x&state={}", other.state),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // The person declining comes back as `access_denied`.
    let (req, cookie) = start(&s, &idp, PORTAL).await;
    let (_, location, cleared) = navigate(
        &s,
        &format!(
            "/auth/oidc/corp/callback?error=access_denied&state={}",
            req.state
        ),
        Some(&cookie),
    )
    .await;
    assert_eq!(query(&location)["error"], "access_denied");
    assert!(cleared.contains("Max-Age=0"), "{cleared}");

    let (status, b) = s
        .call(
            Method::GET,
            "/auth/oidc/corp/authorize?return_to=https%3A%2F%2Fevil.example%2F",
            None,
            None,
        )
        .await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
    let (status, b) = s
        .call(Method::GET, "/auth/oidc/corp/authorize", None, None)
        .await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
    for (method, path) in [
        (Method::GET, "/auth/oidc/other/authorize"),
        (Method::GET, "/auth/oidc/other/callback?state=x"),
        (Method::POST, "/auth/oidc/other/session"),
    ] {
        let body = (method == Method::POST).then(|| json!({"login_code": "x"}));
        let (status, b) = s.call(method, path, None, body).await;
        assert_error(status, &b, StatusCode::NOT_FOUND, "API-001");
    }
}

#[tokio::test]
async fn logins_a_browser_runs_at_the_same_time_keep_their_own_cookies() {
    let idp = Issuer::start().await;
    let s = idp.server().await;
    let (_, invitation) = s
        .invite(json!({"email": "heidi@example.com", "role": "member"}))
        .await;
    let (first, first_cookie) = start(&s, &idp, PORTAL).await;
    let (second, second_cookie) = start(&s, &idp, PORTAL).await;
    assert_ne!(
        first_cookie.split('=').next(),
        second_cookie.split('=').next()
    );
    // The browser sends both cookies to the callback.
    let both = format!("{first_cookie}; {second_cookie}");
    for req in [&first, &second] {
        let code = idp.approve(req, "heidi");
        let (_, location) = callback(&s, req, &code, &both).await;
        let login_code = query(&location)
            .get("login_code")
            .cloned()
            .unwrap_or_else(|| panic!("{location}"));
        let (status, b) = exchange(&s, &login_code, Some(&invitation)).await;
        assert_eq!(status, StatusCode::OK, "{b}");
    }
}

#[test]
fn a_login_whose_redirect_failed_holds_no_slot() {
    let logins = crate::browser_login::BrowserLogins::default();
    let started = logins.start("corp", PORTAL, None, 0).unwrap();
    logins.abandon(&started.state);
    assert!(
        logins
            .callback("corp", &started.state, Some(&started.binding), 1)
            .is_err()
    );
}

#[tokio::test]
async fn rotated_provider_keys_are_fetched_again() {
    let idp = Issuer::start().await;
    let s = idp.server().await;
    let (_, invitation) = s
        .invite(json!({"email": "erin@example.com", "role": "member"}))
        .await;
    let (status, b) = login(&s, &idp, "erin", Some(&invitation)).await;
    assert_eq!(status, StatusCode::OK, "{b}");

    // A token from a key the cached set lacks is refused until the keys may be
    // fetched again, so forged tokens cannot make the server hammer the provider.
    idp.rotate("k2");
    let (req, cookie) = start(&s, &idp, PORTAL).await;
    let code = idp.approve(&req, "erin");
    let (_, location) = callback(&s, &req, &code, &cookie).await;
    assert_eq!(query(&location)["error"], "login_failed");
    s.state.oidc_logins["corp"].refresh_keys_at_once();
    let (status, b) = login(&s, &idp, "erin", None).await;
    assert_eq!(status, StatusCode::OK, "{b}");
}

#[tokio::test]
async fn an_unreachable_provider_is_reported_as_unavailable() {
    let idp = Issuer::start().await;
    let s = idp.server().await;
    let (req, cookie) = start(&s, &idp, PORTAL).await;
    // The provider refuses a code it no longer holds.
    let code = idp.approve(&req, "frank");
    idp.idp.lock().unwrap().grants.clear();
    let (_, location) = callback(&s, &req, &code, &cookie).await;
    assert_eq!(query(&location)["error"], "login_failed");

    // A provider that cannot be reached at all.
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config::new("acme", dir.path());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    config.oidc_login = vec![OidcLoginConfig {
        issuer: dead,
        client_secret_file: None,
        ..idp.config(std::path::Path::new("unused"))
    }];
    let s = server_with(dir, config).await;
    let (status, b) = s
        .call(Method::GET, "/auth/oidc/corp/authorize", None, None)
        .await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
    let (status, b) = s
        .call(
            Method::GET,
            &format!("/auth/oidc/corp/authorize?return_to={}", escape(PORTAL)),
            None,
            None,
        )
        .await;
    assert_error(status, &b, StatusCode::BAD_GATEWAY, "SRV-003");
}

#[test]
fn oidc_login_configuration_is_checked() {
    let dir = tempfile::tempdir().unwrap();
    let base = OidcLoginConfig {
        id: "corp".into(),
        issuer: "https://id.example.com".into(),
        client_id: CLIENT_ID.into(),
        client_secret_file: None,
        callback_url: "https://porch.example.com/auth/oidc/corp/callback".into(),
        return_urls: vec!["https://portal.example.com/done".into()],
        scopes: vec!["openid".into()],
        algorithms: vec!["RS256".into(), "ES256".into()],
    };
    let check = |providers: Vec<OidcLoginConfig>| {
        let mut config = Config::new("acme", dir.path());
        config.oidc_login = providers;
        config.validate().err().map(|e| e.to_string())
    };
    assert_eq!(check(vec![base.clone()]), None);
    let cases: Vec<(OidcLoginConfig, &str)> = vec![
        (
            OidcLoginConfig {
                id: "github".into(),
                ..base.clone()
            },
            "other than `github`",
        ),
        (
            OidcLoginConfig {
                issuer: "http://id.example.com".into(),
                ..base.clone()
            },
            "must use https",
        ),
        (
            OidcLoginConfig {
                callback_url: "https://porch.example.com/auth/oidc/other/callback".into(),
                ..base.clone()
            },
            "/auth/oidc/corp/callback",
        ),
        (
            OidcLoginConfig {
                return_urls: vec![],
                ..base.clone()
            },
            "at least one page",
        ),
        (
            OidcLoginConfig {
                scopes: vec!["email".into()],
                ..base.clone()
            },
            "must include `openid`",
        ),
        (
            OidcLoginConfig {
                algorithms: vec!["HS256".into()],
                ..base.clone()
            },
            "supported are",
        ),
        (
            OidcLoginConfig {
                algorithms: vec!["none".into()],
                ..base.clone()
            },
            "supported are",
        ),
    ];
    for (config, expected) in cases {
        let message = check(vec![config]).expect(expected);
        assert!(message.contains(expected), "{message}");
    }
    let message = check(vec![base.clone(), base]).unwrap();
    assert!(message.contains("used twice"), "{message}");
}

#[tokio::test]
async fn a_member_who_signed_in_with_github_cannot_sign_in_through_oidc_by_email() {
    let idp = Issuer::start().await;
    let s = idp.server().await;
    let (_, invitation) = s
        .invite(json!({"email": "grace@example.com", "role": "member"}))
        .await;
    let github = openporch_store::ExternalIdentity {
        provider: "github".into(),
        subject: "42".into(),
        login: Some("grace".into()),
        verified_emails: vec!["grace@example.com".into()],
        name: "Grace".into(),
    };
    crate::login::login(&s.state, &github, Some(&invitation))
        .await
        .unwrap();
    // Neither the spent invitation nor a new one for the same email links the
    // OIDC identity, whose email the provider has verified.
    let (status, b) = login(&s, &idp, "grace", Some(&invitation)).await;
    assert_error(status, &b, StatusCode::UNAUTHORIZED, "AUTH-001");
    let (_, fresh) = s
        .invite(json!({"email": "grace@example.com", "role": "member"}))
        .await;
    let (status, b) = login(&s, &idp, "grace", Some(&fresh)).await;
    assert_error(status, &b, StatusCode::UNAUTHORIZED, "AUTH-001");
    assert!(
        b["message"]
            .as_str()
            .unwrap()
            .contains("already belongs to a member"),
        "{b}"
    );
}
