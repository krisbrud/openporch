//! First start logs only the bootstrap token's location, never the token.

use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Captured;
    fn make_writer(&'a self) -> Captured {
        self.clone()
    }
}

#[tokio::test]
async fn bootstrap_token_is_not_logged() {
    let logs = Captured::default();
    tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .with_max_level(tracing::Level::TRACE)
        .json()
        .init();
    let dir = tempfile::tempdir().unwrap();
    let _state = openporch_server::start(openporch_server::Config::new("acme", dir.path()), None)
        .await
        .unwrap();
    let path = dir.path().join("bootstrap-token");
    let token = std::fs::read_to_string(&path).unwrap();
    let token = token.trim();
    assert!(!token.is_empty());
    let logs = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(
        logs.contains("wrote the bootstrap administrator token"),
        "{logs}"
    );
    assert!(logs.contains(&path.display().to_string()), "{logs}");
    assert!(!logs.contains(token), "the token reached the logs");
    // Nor does any part of the master key.
    let key = std::fs::read_to_string(dir.path().join("master.key")).unwrap();
    assert!(
        !logs.contains(key.trim()),
        "the master key reached the logs"
    );
}
