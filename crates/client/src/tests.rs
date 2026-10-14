//! Transport rules: verified TLS, plain HTTP only on loopback, and tokens that never
//! appear in diagnostics.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use tokio::io::AsyncReadExt as _;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::pem::PemObject as _;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};

use super::*;

const SECRET: &str = "opt_secret-token-value";
const CERT: &[u8] = include_bytes!("../../server/src/tests/fixtures/untrusted-test.crt");
const KEY: &[u8] = include_bytes!("../../server/src/tests/fixtures/untrusted-test.key");

fn token() -> Token {
    Token::new(SECRET).unwrap()
}

#[test]
fn server_urls_need_https_except_on_loopback() {
    for ok in [
        "https://porch.example.com",
        "https://porch.example.com/",
        "https://porch.example.com/api/",
        "http://127.0.0.1:8080",
        "http://localhost:8080/",
        "http://[::1]:8080",
    ] {
        let url = server_url(ok).unwrap_or_else(|e| panic!("{ok}: {e}"));
        assert!(!url_key(&url).ends_with('/'), "{url}");
    }
    for bad in [
        "http://porch.example.com",
        "http://10.0.0.1:8080",
        "http://127.0.0.1.example.com",
        "ftp://porch.example.com",
        "https://user:pw@porch.example.com",
        "https://porch.example.com/?x=1",
        "not a url",
    ] {
        assert!(server_url(bad).is_err(), "{bad} was accepted");
    }
    // The check runs before any connection is made.
    let err = Client::new("http://porch.example.com", "acme", Some(token())).unwrap_err();
    assert!(err.to_string().contains("https"), "{err}");
}

#[test]
fn tokens_are_redacted_in_debug_and_display() {
    let t = token();
    assert_eq!(t.expose(), SECRET);
    assert!(!format!("{t:?} {t}").contains(SECRET));
    let client = Client::new("http://127.0.0.1:1", "acme", Some(t)).unwrap();
    assert!(!format!("{client:?}").contains(SECRET));
    let device: DeviceLogin = serde_json::from_value(serde_json::json!({
        "device_code": "dev-secret", "user_code": "ABCD-1234",
        "verification_uri": "https://github.com/login/device", "expires_in": 900, "interval": 5,
    }))
    .unwrap();
    assert!(!format!("{device:?}").contains("dev-secret"));
    assert_eq!(Token::new("  \n"), None);
    assert_eq!(Token::new(" x \n").unwrap().expose(), "x");
}

#[tokio::test]
async fn errors_never_carry_the_token() {
    let seen = Arc::new(AtomicUsize::new(0));
    let counter = seen.clone();
    let app = Router::new().route(
        "/orgs/acme/apps",
        get(move |headers: HeaderMap| {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                // The token arrives as a bearer token.
                assert_eq!(
                    headers["authorization"].to_str().unwrap(),
                    format!("Bearer {SECRET}")
                );
                (
                    StatusCode::UNAUTHORIZED,
                    axum::Json(serde_json::json!({
                        "error": "AUTH-001", "message": "invalid or expired token"})),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });

    let client = Client::new(&format!("http://{addr}"), "acme", Some(token())).unwrap();
    let err = client.apps().await.unwrap_err();
    assert_eq!(err.status(), Some(401));
    assert!(matches!(&err, Error::Api { code, .. } if code == "AUTH-001"));
    assert_eq!(seen.load(Ordering::SeqCst), 1);
    for text in [err.to_string(), format!("{err:?}")] {
        assert!(!text.contains(SECRET), "{text}");
    }

    // Nor do transport errors.
    let client = Client::new("http://127.0.0.1:1", "acme", Some(token())).unwrap();
    let err = client.apps().await.unwrap_err();
    assert!(matches!(err, Error::Transport(_)), "{err}");
    for text in [err.to_string(), format!("{err:?}")] {
        assert!(!text.contains(SECRET), "{text}");
    }
}

/// `Invalid server certificate`, CLI half: a server with an untrusted certificate
/// is refused during the handshake, before the request and its token are sent.
#[tokio::test]
async fn untrusted_certificate_is_rejected_before_the_token_is_sent() {
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    let cert = CertificateDer::from_pem_slice(CERT).unwrap();
    let key = PrivateKeyDer::from_pem_slice(KEY).unwrap();
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        match acceptor.accept(tcp).await {
            Err(e) => Err(e.to_string()),
            Ok(mut tls) => {
                let mut buf = vec![0u8; 4096];
                Ok(tls.read(&mut buf).await.unwrap_or(0))
            }
        }
    });

    let client = Client::new(&format!("https://{addr}"), "acme", Some(token())).unwrap();
    let limit = std::time::Duration::from_secs(20);
    let err = tokio::time::timeout(limit, client.apps())
        .await
        .expect("the client neither connected nor failed")
        .unwrap_err();
    assert!(matches!(err, Error::Transport(_)), "{err}");
    assert!(!err.to_string().contains(SECRET), "{err}");
    let handshake = tokio::time::timeout(limit, server)
        .await
        .expect("the client never connected")
        .unwrap();
    match handshake {
        Err(e) => assert!(e.contains("alert"), "{e}"),
        Ok(n) => panic!("the handshake succeeded and the client sent {n} bytes"),
    }
}
