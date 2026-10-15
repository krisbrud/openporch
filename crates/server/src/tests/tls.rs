//! `Invalid server certificate` in `authentication`, for the server's own outbound
//! connections: a GitHub endpoint with an untrusted certificate is refused during
//! the handshake, before any request (and so any credential) is sent. The CLI half
//! is verified with the client in task 6.1.

use std::sync::Arc;

use tokio::io::AsyncReadExt as _;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::pem::PemObject as _;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};

use crate::config::GithubConfig;
use crate::identity::{Github, IdentityProvider as _};

const CERT: &[u8] = include_bytes!("fixtures/untrusted-test.crt");
const KEY: &[u8] = include_bytes!("fixtures/untrusted-test.key");

#[tokio::test]
async fn untrusted_certificate_is_rejected_before_any_request() {
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
            // The client aborted the handshake, so no request bytes were readable.
            Err(e) => Err(e.to_string()),
            Ok(mut tls) => {
                let mut buf = vec![0u8; 4096];
                Ok(tls.read(&mut buf).await.unwrap_or(0))
            }
        }
    });

    let base = format!("https://{addr}/");
    let github = Github::new(&GithubConfig {
        client_id: "Iv1.test-client".into(),
        api_url: base.clone(),
        web_url: base,
        browser: None,
    })
    .unwrap();
    let limit = std::time::Duration::from_secs(20);
    let err = tokio::time::timeout(limit, github.start_device_login())
        .await
        .expect("the client neither connected nor failed")
        .unwrap_err();
    assert!(!err.to_string().contains("Iv1.test-client"), "{err}");
    let handshake = tokio::time::timeout(limit, server)
        .await
        .expect("the client never connected")
        .unwrap();
    // The client connected and then rejected the certificate with a TLS alert.
    match handshake {
        Err(e) => assert!(e.contains("alert"), "{e}"),
        Ok(n) => panic!("the handshake succeeded and the client sent {n} bytes"),
    }
}
