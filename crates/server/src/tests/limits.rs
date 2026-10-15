//! Rate limits on the login and token endpoints (`authentication`, `Transport and
//! credential handling`).

use std::net::SocketAddr;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use serde_json::json;
use tower::ServiceExt as _;

use super::{Server, assert_error, server_with};
use crate::Config;

async fn limited(login: u32, tokens: u32, proxy: bool) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config::new("acme", dir.path());
    config.rate_limits.login_per_minute = login;
    config.rate_limits.tokens_per_minute = tokens;
    config.tls_terminated_by_proxy = proxy;
    server_with(dir, config).await
}

/// Starts a device login from `peer`, optionally through a proxy that adds
/// `forwarded`. Returns the status and the `Retry-After` header.
async fn start_login(
    s: &Server,
    peer: [u8; 4],
    forwarded: Option<&str>,
) -> (StatusCode, Option<String>) {
    let mut req = Request::post("/auth/github/device");
    if let Some(f) = forwarded {
        req = req.header("x-forwarded-for", f);
    }
    let mut req = req.body(Body::empty()).unwrap();
    req.extensions_mut()
        .insert(ConnectInfo(SocketAddr::from((peer, 40000))));
    let res = s.router.clone().oneshot(req).await.unwrap();
    let retry = res
        .headers()
        .get(header::RETRY_AFTER)
        .map(|v| v.to_str().unwrap().to_owned());
    (res.status(), retry)
}

#[tokio::test]
async fn login_requests_are_limited_per_client_address() {
    let s = limited(2, 0, false).await;
    // GitHub is not configured, so allowed requests return 404.
    for _ in 0..2 {
        assert_eq!(
            start_login(&s, [10, 0, 0, 1], None).await.0,
            StatusCode::NOT_FOUND
        );
    }
    let (status, retry) = start_login(&s, [10, 0, 0, 1], None).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(retry.as_deref(), Some("30"));
    // Polling counts against the same allowance.
    let (status, b) = s
        .call(
            axum::http::Method::POST,
            "/auth/github/token",
            None,
            Some(json!({"device_code": "x"})),
        )
        .await;
    // Without connection info every caller shares one key, which is still unspent.
    assert_eq!(status, StatusCode::NOT_FOUND, "{b}");
    // Another address has its own allowance, and a forwarded header is ignored
    // without a trusted proxy.
    assert_eq!(
        start_login(&s, [10, 0, 0, 2], Some("10.9.9.9")).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        start_login(&s, [10, 0, 0, 1], Some("10.9.9.9")).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
async fn behind_a_proxy_the_forwarded_address_is_the_key() {
    let s = limited(1, 0, true).await;
    let proxy = [127, 0, 0, 1];
    assert_eq!(
        start_login(&s, proxy, Some("203.0.113.9, 198.51.100.1"))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    // A client cannot dodge the limit by prepending addresses of its own.
    assert_eq!(
        start_login(&s, proxy, Some("192.0.2.77, 198.51.100.1"))
            .await
            .0,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        start_login(&s, proxy, Some("198.51.100.2")).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn token_creation_is_limited_per_principal() {
    let s = limited(0, 2, false).await;
    for id in ["tok-a", "tok-b"] {
        let (status, b) = s
            .post(
                "/orgs/acme/users/bootstrap/tokens",
                &s.admin,
                json!({"id": id}),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{b}");
    }
    let (status, b) = s
        .post(
            "/orgs/acme/users/bootstrap/tokens",
            &s.admin,
            json!({"id": "tok-c"}),
        )
        .await;
    assert_error(status, &b, StatusCode::TOO_MANY_REQUESTS, "API-007");
    let (_, tokens) = s.get("/orgs/acme/users/bootstrap/tokens", &s.admin).await;
    assert!(
        !tokens.to_string().contains("tok-c"),
        "a limited request creates no token: {tokens}"
    );
}

#[tokio::test]
async fn oversized_bodies_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config::new("acme", dir.path());
    config.max_body_bytes = 1024;
    let s = server_with(dir, config).await;
    let big = json!({"id": "shop", "name": "x".repeat(2048)});
    let (status, b) = s.post("/orgs/acme/apps", &s.admin, big).await;
    assert_error(status, &b, StatusCode::PAYLOAD_TOO_LARGE, "API-006");
    let (status, b) = s
        .post("/orgs/acme/apps", &s.admin, json!({"id": "shop"}))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{b}");
}
