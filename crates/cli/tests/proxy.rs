//! Plain HTTP is only for this machine, so `porch` never sends it through a proxy
//! from `HTTP_PROXY`, where the token would travel in clear text.

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const SECRET: &str = "opt_proxy-leak-check";

/// Accepts connections and records whether any arrived; answers `401`.
fn listener(seen: Arc<AtomicBool>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            seen.store(true, Ordering::SeqCst);
            let mut stream = stream;
            stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let body = r#"{"error":"AUTH-001","message":"invalid or expired token"}"#;
            let _ = write!(
                stream,
                "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    format!("http://{addr}")
}

#[test]
fn loopback_http_bypasses_proxies() {
    let (proxied, served) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    let proxy = listener(proxied.clone());
    let server = listener(served.clone());
    let home = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_porch"))
        .args(["--url", &server, "--org", "acme", "get", "apps"])
        .env_clear()
        .env("HOME", home.path())
        .env("OPENPORCH_TOKEN", SECRET)
        .env("HTTP_PROXY", &proxy)
        .env("http_proxy", &proxy)
        .env("ALL_PROXY", &proxy)
        .env("NO_PROXY", "")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(stderr.contains("AUTH-001"), "{stderr}");
    assert!(!stderr.contains(SECRET), "{stderr}");
    assert!(served.load(Ordering::SeqCst));
    assert!(
        !proxied.load(Ordering::SeqCst),
        "the request went through the proxy"
    );
}
