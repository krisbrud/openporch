//! Master-key rotation (`secret-stores-and-references`, `Key recovery and
//! rotation`): a new active key put first in the key file seals new secrets while
//! retired keys keep older versions readable, and a missing retired key blocks
//! readiness instead of being regenerated.

use axum::http::{Method, StatusCode};
use openporch_store::secrets::{MasterKey, SecretStore as _};
use serde_json::json;

use super::{Server, assert_error, server, server_in};
use crate::Config;

pub(super) fn write_keys(path: &std::path::Path, keys: &[&str]) {
    use std::io::Write as _;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    opts.mode(0o600);
    let mut f = opts.open(path).unwrap();
    for k in keys {
        writeln!(f, "{k}").unwrap();
    }
}

async fn secret(s: &Server, key: &str, value: &str) -> openporch_core::model::SecretRef {
    let (status, b) = s
        .post(
            "/orgs/acme/apps/shop/values",
            &s.admin,
            json!({"key": key, "secret_ref": {"value": value}}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{b}");
    serde_json::from_value(b["secret_ref"].clone()).unwrap()
}

async fn read(s: &Server, r: &openporch_core::model::SecretRef) -> Option<String> {
    let primary = crate::secret_inputs::primary(&s.state).ok()?;
    primary.read(r).await.ok().map(|v| v.expose().to_owned())
}

#[tokio::test]
async fn rotate_the_master_key() {
    let s = server().await;
    s.app("shop").await;
    let old_ref = secret(&s, "OLD", "before-rotation").await;
    let path = s.dir.path().join("master.key");
    let old_key = std::fs::read_to_string(&path).unwrap().trim().to_owned();
    let old_id = s.state.keyring.as_ref().unwrap().active_id().to_owned();
    let Server {
        dir,
        state,
        router,
        admin,
        ..
    } = s;
    drop((state, router));

    // Rotate: the new key goes first, the old one stays as retired.
    let new_key = MasterKey::generate();
    write_keys(&path, &[new_key.to_base64().as_str(), &old_key]);
    let mut s = server_in(dir).await;
    s.admin = admin;
    assert!(s.state.ready(), "{:?}", s.state.not_ready);
    assert_eq!(s.state.keyring.as_ref().unwrap().active_id(), new_key.id());
    let new_ref = secret(&s, "NEW", "after-rotation").await;
    assert_eq!(read(&s, &old_ref).await.as_deref(), Some("before-rotation"));
    assert_eq!(read(&s, &new_ref).await.as_deref(), Some("after-rotation"));
    // Sessions and tokens keep working across the rotation.
    assert_eq!(s.get("/orgs/acme/users", &s.admin).await.0, StatusCode::OK);
    let Server {
        dir,
        state,
        router,
        admin,
        ..
    } = s;
    drop((state, router));

    // Dropping the retired key too early blocks readiness and names the key.
    write_keys(&path, &[new_key.to_base64().as_str()]);
    let mut s = server_in(dir).await;
    s.admin = admin;
    let (status, body) = s.call(Method::GET, "/readyz", None, None).await;
    assert_error(status, &body, StatusCode::SERVICE_UNAVAILABLE, "SRV-002");
    assert_eq!(
        body["reasons"],
        json!([format!("master_key_missing:{old_id}")])
    );
    // The key file was not replaced with a generated key.
    assert_eq!(
        std::fs::read_to_string(&path).unwrap().trim(),
        new_key.to_base64().as_str()
    );
}

#[tokio::test]
async fn a_second_server_on_the_same_installation_exits_before_any_work() {
    let s = server().await;
    let token = std::fs::read_to_string(s.dir.path().join("bootstrap-token")).unwrap();
    let e = crate::start(Config::new("acme", s.dir.path()), None)
        .await
        .unwrap_err();
    assert!(
        e.to_string().contains("owned by another openporch process"),
        "{e}"
    );
    // The first server keeps serving, and nothing of its installation changed.
    assert_eq!(
        std::fs::read_to_string(s.dir.path().join("bootstrap-token")).unwrap(),
        token
    );
    let (status, apps) = s.get("/orgs/acme/apps", &s.admin).await;
    assert_eq!(status, StatusCode::OK, "{apps}");
}

#[tokio::test]
async fn an_older_binary_refuses_a_newer_schema_and_names_the_way_out() {
    let s = server().await;
    // A newer release has migrated the database past what this binary knows.
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, installed_on, success, checksum, \
         execution_time) VALUES (99990101000000, 'future', CURRENT_TIMESTAMP, 1, x'00', 0)",
    )
    .execute(s.state.store.pool())
    .await
    .unwrap();
    let Server {
        dir, state, router, ..
    } = s;
    state.store.pool().close().await;
    drop((state, router));

    let e = crate::start(Config::new("acme", dir.path()), None)
        .await
        .unwrap_err();
    let message = e.to_string();
    assert!(
        message.contains("schema version 99990101000000"),
        "{message}"
    );
    assert!(
        message.contains("run a compatible openporch version or restore a compatible backup"),
        "{message}"
    );
}
