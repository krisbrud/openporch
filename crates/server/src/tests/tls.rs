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
            // The handshake failed: no request bytes were ever readable.
            Err(_) => 0,
            Ok(mut tls) => {
                let mut buf = vec![0u8; 4096];
                tls.read(&mut buf).await.unwrap_or(0)
            }
        }
    });

    let base = format!("https://{addr}/");
    let github = Github::new(&GithubConfig {
        client_id: "Iv1.test-client".into(),
        api_url: base.clone(),
        web_url: base,
    })
    .unwrap();
    let err = github.start_device_login().await.unwrap_err();
    assert!(!err.to_string().contains("Iv1.test-client"), "{err}");
    assert_eq!(server.await.unwrap(), 0, "the client sent a request");
}
