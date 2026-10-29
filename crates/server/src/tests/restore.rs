//! The backup and restore drill: an installation is backed up, restored into
//! a new data directory and started. It accepts no changes until every
//! restored resource has been observed, keeps every resource's identity, and
//! blocks a resource that no longer exists instead of creating it again.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::StatusCode;
use openporch_drivers::{
    DeleteOutcome, Driver, DriverFuture, DriverInfo, DriverRegistry, DriverRequest, Echo,
    Observation, Plan, PutOutcome,
};
use serde_json::{Value, json};

use super::{Server, server_from};
use crate::backup::{self, RESTORE_MARKER};
use crate::{Config, state};

const DRIVER: &str = "drill/gated";
const ENV: &str = "/orgs/acme/apps/shop/envs/development";

/// Whether observations may answer.
static OPEN: AtomicBool = AtomicBool::new(true);
/// Resources that no longer exist.
static GONE: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// `drill/gated`: `openporch/echo` whose observations wait for [`OPEN`] and
/// find nothing for the resources in [`GONE`].
#[derive(Debug)]
struct Gated(Echo, DriverInfo);

impl Gated {
    fn new() -> Self {
        let echo = Echo::new();
        let mut info = echo.info().clone();
        info.id = DRIVER.to_owned();
        Self(echo, info)
    }
}

impl Driver for Gated {
    fn info(&self) -> &DriverInfo {
        &self.1
    }

    fn plan<'a>(&'a self, req: &'a DriverRequest) -> DriverFuture<'a, Plan> {
        self.0.plan(req)
    }

    fn plan_delete<'a>(&'a self, req: &'a DriverRequest) -> DriverFuture<'a, Plan> {
        self.0.plan_delete(req)
    }

    fn put<'a>(&'a self, req: &'a DriverRequest) -> DriverFuture<'a, PutOutcome> {
        self.0.put(req)
    }

    fn get<'a>(&'a self, req: &'a DriverRequest) -> DriverFuture<'a, Observation> {
        Box::pin(async move {
            while !OPEN.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let gone = GONE
                .lock()
                .unwrap()
                .contains(&req.guresid.as_str().to_owned());
            if gone {
                Ok(Observation::Absent { cookie: None })
            } else {
                self.0.get(req).await
            }
        })
    }

    fn delete<'a>(&'a self, req: &'a DriverRequest) -> DriverFuture<'a, DeleteOutcome> {
        self.0.delete(req)
    }
}

fn drivers() -> Arc<DriverRegistry> {
    let mut registry = DriverRegistry::with_builtins();
    registry.register(Arc::new(Gated::new()) as Arc<_>).unwrap();
    Arc::new(registry)
}

fn config(dir: &std::path::Path) -> Config {
    let mut config = Config::new("acme", dir);
    config.images.pin_other_registries = false;
    config
}

async fn start(dir: tempfile::TempDir) -> Server {
    let state = state::start_with_drivers(
        config(dir.path()),
        None,
        Arc::new(openporch_engine::cloud::AwsSts::new()),
        Some(drivers()),
    )
    .await
    .unwrap();
    server_from(dir, state)
}

async fn stop(s: Server) -> tempfile::TempDir {
    let Server {
        dir, state, router, ..
    } = s;
    state.store.pool().close().await;
    drop((state, router));
    dir
}

/// Deploys the development environment from `source`, which must succeed.
async fn deploy(s: &Server, source: Value) -> Value {
    let (_, e) = s.get(ENV, &s.admin).await;
    let mut body = source;
    body["expected_generation"] = e["generation"].clone();
    let (st, d) = s.post(&format!("{ENV}/deploys"), &s.admin, body).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{d}");
    let path = format!("{ENV}/deploys/{}", d["id"].as_str().unwrap());
    for _ in 0..500 {
        let (_, done) = s.get(&path, &s.admin).await;
        if done["status"] != "in_progress" {
            return done;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("deployment {path} did not finish");
}

/// Each resource's GUResID and status, by descriptor.
async fn resources(s: &Server) -> BTreeMap<String, (String, String)> {
    let (st, list) = s.get(&format!("{ENV}/resources"), &s.admin).await;
    assert_eq!(st, StatusCode::OK, "{list}");
    list["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["descriptor"].as_str().unwrap().to_owned(),
                (
                    r["guresid"].as_str().unwrap().to_owned(),
                    r["status"].as_str().unwrap().to_owned(),
                ),
            )
        })
        .collect()
}

#[tokio::test]
async fn backup_and_restore_drill() {
    let s = start(tempfile::tempdir().unwrap()).await;
    // A record, so definitions may name the driver; it runs in-process.
    sqlx::query("INSERT INTO drivers (org_id, id, body, created_at) VALUES ('acme', ?, '{}', 0)")
        .bind(DRIVER)
        .execute(s.state.store.pool())
        .await
        .unwrap();
    s.app("shop").await;
    for (id, ty, driver) in [
        ("env", "base-env", "openporch/none"),
        ("workloads", "workload", "openporch/none"),
        ("pg", "postgres", DRIVER),
        ("cache", "redis", DRIVER),
    ] {
        let (st, b) = s
            .post(
                "/orgs/acme/resources/defs",
                &s.admin,
                json!({"id": id, "type": ty, "driver_type": driver,
                       "driver_inputs": {"values": {
                           "host": format!("{id}.internal"), "name": id, "port": 5432},
                           "secret_refs": {"username": {"value": "shop"}, "password": {"value": "change-me"}}},
                       "criteria": [{}]}),
            )
            .await;
        assert_eq!(st, StatusCode::CREATED, "{b}");
    }
    let (st, delta) = s
        .post(
            "/orgs/acme/apps/shop/deltas",
            &s.admin,
            json!({"modules": {"add": {"web": {
                "externals": {"db": {"type": "postgres"}, "cache": {"type": "redis"}},
                "spec": {"containers": {"web": {"image": "web:1"}}}}}}}),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{delta}");
    let first = deploy(&s, json!({"delta_id": delta["id"]})).await;
    assert_eq!(first["status"], "succeeded", "{first}");
    let before = resources(&s).await;
    assert_eq!(before.len(), 4, "{before:?}");
    let db = "postgres.default#modules.web.externals.db";
    let cache = "redis.default#modules.web.externals.cache";

    // A backup needs the server stopped.
    let backups = tempfile::tempdir().unwrap();
    let dest = backups.path().join("drill");
    let err = backup::backup(&config(s.dir.path()), &dest)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("stop the server"), "{err}");
    let admin = s.admin.clone();
    let old = stop(s).await;
    // OpenTofu state is backed up; its runs and plugin cache are not.
    let tofu = old.path().join("opentofu");
    for (file, body) in [
        ("state/0123/terraform.tfstate", "{}"),
        ("runs/0123/credentials.json", "{}"),
        ("plugin-cache/provider", ""),
    ] {
        std::fs::create_dir_all(tofu.join(file).parent().unwrap()).unwrap();
        std::fs::write(tofu.join(file), body).unwrap();
    }
    // Not inside the installation, and the master key is left out however
    // the configuration spells its path.
    let inside = old.path().join("backups");
    let err = backup::backup(&config(old.path()), &inside)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("inside the installation"), "{err}");
    assert!(!inside.exists());
    #[cfg(unix)]
    {
        let link = backups.path().join("link");
        std::os::unix::fs::symlink(old.path(), &link).unwrap();
        let mut spelled = config(&link);
        spelled.master_key_file = Some(old.path().join("master.key"));
        let other = backups.path().join("spelled");
        backup::backup(&spelled, &other).await.unwrap();
        assert!(other.join("data/openporch.db").exists());
        assert!(!other.join("data/master.key").exists());
        assert!(!other.join("data/opentofu").exists());
    }
    let manifest = backup::backup(&config(old.path()), &dest).await.unwrap();
    assert_eq!(manifest.state_backend, "local");
    assert!(dest.join("opentofu-state/0123/terraform.tfstate").exists());
    assert!(!dest.join("data/opentofu").exists());
    assert_eq!(manifest.org, "acme");
    assert_eq!(manifest.active_resources, 4);
    assert_eq!(manifest.schema_version, openporch_store::SCHEMA_VERSION);
    assert!(!manifest.key_ids.is_empty());
    assert!(!dest.join("data/master.key").exists());
    assert!(!dest.join("data/bootstrap-token").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dest), 0o700);
        assert_eq!(mode(&dest.join("data/openporch.db")), 0o600);
        assert_eq!(mode(&dest.join("backup.json")), 0o600);
    }

    // Restored into a new data directory, with the master key restored on
    // its own; a second restore into it is refused.
    let new = tempfile::tempdir().unwrap();
    let restored = backup::restore(&config(new.path()), &dest).await.unwrap();
    assert_eq!(restored, manifest);
    let err = backup::restore(&config(new.path()), &dest)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not empty"), "{err}");
    std::fs::copy(old.path().join("master.key"), new.path().join("master.key")).unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(
        new.path().join("master.key"),
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .unwrap();
    assert!(new.path().join(RESTORE_MARKER).exists());
    assert!(
        new.path()
            .join("opentofu/state/0123/terraform.tfstate")
            .exists()
    );
    assert!(!new.path().join("opentofu/runs").exists());

    // The cache was deleted after the backup; observations wait.
    GONE.lock().unwrap().push(before[cache].0.clone());
    OPEN.store(false, Ordering::SeqCst);
    let mut s = start(new).await;
    s.admin = admin;

    // Until every resource is observed, reads work and changes are refused.
    let (st, ready) = s
        .call(axum::http::Method::GET, "/healthz", None, None)
        .await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE, "{ready}");
    assert!(
        ready["reasons"]
            .as_array()
            .unwrap()
            .contains(&json!("restore_reconciling")),
        "{ready}"
    );
    let during = resources(&s).await;
    assert_eq!(during[db].1, "unknown", "{during:?}");
    let (st, b) = s
        .post("/orgs/acme/apps", &s.admin, json!({"id": "other"}))
        .await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE, "{b}");
    assert_eq!(b["error"], "SRV-002", "{b}");

    OPEN.store(true, Ordering::SeqCst);
    for _ in 0..500 {
        if s.state.not_ready_reasons().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        s.state.not_ready_reasons().is_empty(),
        "{:?}",
        s.state.not_ready_reasons()
    );
    assert!(!s.dir.path().join(RESTORE_MARKER).exists());

    // Every resource keeps its identity. Those that exist are as the backup
    // recorded them; the one that does not is blocked for an operator rather
    // than created again.
    let after = resources(&s).await;
    for (descriptor, (guresid, status)) in &before {
        assert_eq!(&after[descriptor].0, guresid, "{descriptor}");
        if descriptor == cache {
            assert_eq!(after[descriptor].1, "recovery_required", "{after:?}");
        } else {
            assert_eq!(&after[descriptor].1, status, "{descriptor}: {after:?}");
        }
    }
    let (_, audit) = s.get("/orgs/acme/audit-logs", &s.admin).await;
    assert!(audit.to_string().contains("\"restore\""), "{audit}");
    let (_, e) = s.get(ENV, &s.admin).await;
    let (st, b) = s
        .post(
            &format!("{ENV}/deploys"),
            &s.admin,
            json!({"deploy_id": first["id"], "replay": "current",
                   "expected_generation": e["generation"]}),
        )
        .await;
    assert_eq!(st, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"], "DPL-004", "{b}");

    // Once an administrator has checked, the next deployment provisions it
    // again with the same identity: nothing is duplicated.
    let (st, b) = s
        .post(
            &format!("{ENV}/resources/redis/modules.web.externals.cache/resolve"),
            &s.admin,
            json!({"reason": "deleted after the backup; checked that nothing is left"}),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    let third = deploy(&s, json!({"deploy_id": first["id"], "replay": "current"})).await;
    assert_eq!(third["status"], "succeeded", "{third}");
    let last = resources(&s).await;
    for (descriptor, (guresid, _)) in &before {
        assert_eq!(&last[descriptor].0, guresid, "{descriptor}");
        assert_eq!(last[descriptor].1, "provisioned", "{descriptor}: {last:?}");
    }
    drop(backups);
    drop(old);
}

/// Starts the installation in `dir` with the default drivers.
async fn start_plain(dir: tempfile::TempDir, admin: &str) -> Server {
    let mut s = super::server_in(dir).await;
    s.admin = admin.to_owned();
    s
}

/// The plaintext of `r`, read through the server's built-in store.
async fn read_secret(s: &Server, r: &openporch_core::model::SecretRef) -> Option<String> {
    use openporch_store::secrets::SecretStore as _;
    let primary = crate::secret_inputs::primary(&s.state).ok()?;
    primary.read(r).await.ok().map(|v| v.expose().to_owned())
}

/// Restores `backup` into a new data directory with a key file holding
/// `keys` (none: no key file), and starts it.
async fn restore_with(backup: &std::path::Path, keys: &[&str], admin: &str) -> Server {
    let new = tempfile::tempdir().unwrap();
    backup::restore(&config(new.path()), backup).await.unwrap();
    if !keys.is_empty() {
        super::keys::write_keys(&new.path().join("master.key"), keys);
    }
    start_plain(new, admin).await
}

/// `secret-stores-and-references`: `Restore after key rotation` and `Master
/// key unavailable`. A backup holds secret versions sealed with a retired key
/// and with the key that replaced it. Restored with both keys, each version
/// decrypts with the key it records. Restored without the retired key, or
/// without any key, readiness fails naming the missing key, no deployment is
/// accepted, nothing decrypts with the wrong key and no key is generated in
/// its place.
#[tokio::test]
async fn a_restore_after_key_rotation_needs_every_recorded_key() {
    let s = super::server().await;
    let admin = s.admin.clone();
    s.app("shop").await;
    for (id, ty) in [("env", "base-env"), ("workloads", "workload")] {
        let (st, b) = s
            .post(
                "/orgs/acme/resources/defs",
                &s.admin,
                json!({"id": id, "type": ty, "driver_type": "openporch/none", "criteria": [{}]}),
            )
            .await;
        assert_eq!(st, StatusCode::CREATED, "{b}");
    }
    let secret = |key: &'static str, value: &'static str| {
        let s = &s;
        async move {
            let (st, b) = s
                .post(
                    "/orgs/acme/apps/shop/values",
                    &s.admin,
                    json!({"key": key, "secret_ref": {"value": value}}),
                )
                .await;
            assert_eq!(st, StatusCode::CREATED, "{b}");
            serde_json::from_value::<openporch_core::model::SecretRef>(b["secret_ref"].clone())
                .unwrap()
        }
    };
    let old_ref = secret("OLD", "sealed-with-the-old-key").await;
    let old_key = std::fs::read_to_string(s.dir.path().join("master.key"))
        .unwrap()
        .trim()
        .to_owned();
    let old_id = s.state.keyring.as_ref().unwrap().active_id().to_owned();
    let dir = stop(s).await;

    // Rotate: a new active key first, the old one retired after it.
    let new_key = openporch_store::secrets::MasterKey::generate();
    let new_id = new_key.id().to_owned();
    let both = [new_key.to_base64().to_string(), old_key.clone()];
    let both: Vec<&str> = both.iter().map(String::as_str).collect();
    super::keys::write_keys(&dir.path().join("master.key"), &both);
    let s = start_plain(dir, &admin).await;
    assert!(s.state.ready(), "{:?}", s.state.not_ready);
    let new_ref = {
        let (st, b) = s
            .post(
                "/orgs/acme/apps/shop/values",
                &s.admin,
                json!({"key": "NEW", "secret_ref": {"value": "sealed-with-the-new-key"}}),
            )
            .await;
        assert_eq!(st, StatusCode::CREATED, "{b}");
        serde_json::from_value::<openporch_core::model::SecretRef>(b["secret_ref"].clone()).unwrap()
    };
    // A delta to deploy once restored.
    let (st, delta) = s
        .post(
            "/orgs/acme/apps/shop/deltas",
            &s.admin,
            json!({"modules": {"add": {"web": {"spec": {"containers": {
                "web": {"image": "web:1"}}}}}}}),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{delta}");
    let dir = stop(s).await;

    // The backup records ciphertext under both keys.
    let backups = tempfile::tempdir().unwrap();
    let dest = backups.path().join("rotated");
    let manifest = backup::backup(&config(dir.path()), &dest).await.unwrap();
    let mut recorded = manifest.key_ids.clone();
    recorded.sort();
    let mut expected = vec![old_id.clone(), new_id.clone()];
    expected.sort();
    assert_eq!(recorded, expected, "{manifest:?}");
    assert!(!dest.join("data/master.key").exists());

    // With both keys, each version decrypts with the key it records.
    let s = restore_with(&dest, &both, &admin).await;
    for _ in 0..500 {
        if s.state.not_ready_reasons().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        s.state.not_ready_reasons().is_empty(),
        "{:?}",
        s.state.not_ready_reasons()
    );
    assert_eq!(
        read_secret(&s, &old_ref).await.as_deref(),
        Some("sealed-with-the-old-key")
    );
    assert_eq!(
        read_secret(&s, &new_ref).await.as_deref(),
        Some("sealed-with-the-new-key")
    );
    drop(stop(s).await);

    // Without the retired key, or with no key at all, the restored server is
    // not ready, names the missing key and runs no deployment.
    let new_only = new_key.to_base64().to_string();
    for (keys, missing) in [
        (vec![new_only.as_str()], old_id.as_str()),
        (vec![], new_id.as_str()),
    ] {
        let s = restore_with(&dest, &keys, &admin).await;
        let (st, ready) = s.call(axum::http::Method::GET, "/readyz", None, None).await;
        assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE, "{ready}");
        let reasons = ready["reasons"].as_array().unwrap();
        assert!(
            reasons.contains(&json!(format!("master_key_missing:{missing}")))
                || (keys.is_empty()
                    && reasons
                        .iter()
                        .any(|r| r.as_str().unwrap().starts_with("master_key_missing:"))),
            "{ready}"
        );
        // Only key IDs are named, never key material.
        assert!(!ready.to_string().contains(&old_key), "{ready}");
        assert!(!ready.to_string().contains(&new_only), "{ready}");
        let (st, b) = s
            .post(
                &format!("{ENV}/deploys"),
                &s.admin,
                json!({"delta_id": delta["id"], "expected_generation": 0}),
            )
            .await;
        assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE, "{b}");
        assert_eq!(b["error"], "SRV-002", "{b}");
        let (st, page) = s.get(&format!("{ENV}/deploys"), &s.admin).await;
        assert_eq!(st, StatusCode::OK, "{page}");
        assert_eq!(page["deployments"], json!([]), "{page}");
        // Nothing decrypts without its own key.
        assert_eq!(read_secret(&s, &old_ref).await, None);
        // No key was generated in place of the missing one.
        let key_file = s.dir.path().join("master.key");
        let written = std::fs::read_to_string(&key_file).unwrap_or_default();
        assert_eq!(written.trim(), keys.join("\n"), "{keys:?}");
        drop(stop(s).await);
    }
    drop(dir);
}
