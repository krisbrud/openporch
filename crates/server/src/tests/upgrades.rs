//! Schema upgrade and downgrade fixtures (`control-plane-operations`, schema
//! compatibility).
//!
//! Each directory in `fixtures/schema` is the data directory of an
//! installation that a build with that schema version wrote, with
//! `observed.json` recording what its API returned then. Every build must
//! start on each of them, migrate it forward, serve what the older build
//! served and keep deploying. A fixture is added for every schema version a
//! release ships, by running the ignored `write_schema_fixture` test with
//! that release:
//!
//! ```text
//! OPENPORCH_SCHEMA_FIXTURE=crates/server/fixtures/schema/<version> \
//!     cargo test -p openporch-server write_schema_fixture -- --ignored
//! ```

use std::path::{Path, PathBuf};

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::{Server, server, server_in};

const APP: &str = "/orgs/acme/apps/shop";

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/schema")
}

/// The schema fixtures, oldest first.
fn fixture_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(fixtures())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    dirs
}

/// Copies fixture `from` into a fresh data directory.
fn install(from: &Path) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap();
        if name != "observed.json" {
            let to = dir.path().join(name);
            std::fs::copy(&path, &to).unwrap();
            // Git keeps no owner-only modes, which the key and tokens need.
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&to, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
    dir
}

async fn deploy(s: &Server, env: &str, body: Value) -> Value {
    let path = format!("{APP}/envs/{env}");
    let (_, record) = s.get(&path, &s.admin).await;
    let mut body = body;
    body["expected_generation"] = record["generation"].clone();
    let (st, d) = s.post(&format!("{path}/deploys"), &s.admin, body).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{d}");
    let id = d["id"].as_str().unwrap().to_owned();
    for _ in 0..500 {
        let (_, done) = s.get(&format!("{path}/deploys/{id}"), &s.admin).await;
        if done["status"] != "in_progress" {
            assert_eq!(done["status"], "succeeded", "{done}");
            return done;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("deployment {id} did not finish");
}

async fn delta(s: &Server, modules: Value) -> Value {
    let (st, d) = s
        .post(
            &format!("{APP}/deltas"),
            &s.admin,
            json!({"modules": modules}),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{d}");
    d["id"].clone()
}

/// Builds an installation that uses the stored features: members and tokens,
/// environment types and role grants, definitions, deployments and their
/// resources, values, a stored secret, and a preview with a permit.
async fn populate(s: &Server) {
    let developer = s.member("alice", "member").await;
    std::fs::write(s.dir.path().join("alice-token"), &developer).unwrap();
    let ok = |(st, b): (StatusCode, Value), want: StatusCode| assert_eq!(st, want, "{b}");
    ok(
        s.post("/orgs/acme/apps", &s.admin, json!({"id": "shop"}))
            .await,
        StatusCode::CREATED,
    );
    ok(
        s.post(
            "/orgs/acme/env-types",
            &s.admin,
            json!({"id": "production", "description": "Live traffic"}),
        )
        .await,
        StatusCode::CREATED,
    );
    ok(
        s.post(
            "/orgs/acme/env-types/production/users",
            &s.admin,
            json!({"id": "alice", "role": "deployer"}),
        )
        .await,
        StatusCode::CREATED,
    );
    ok(
        s.post(
            &format!("{APP}/envs"),
            &s.admin,
            json!({"id": "production", "type": "production"}),
        )
        .await,
        StatusCode::CREATED,
    );
    for (id, ty) in [("env", "base-env"), ("workloads", "workload")] {
        let def = json!({"id": id, "type": ty, "driver_type": "openporch/none",
                         "driver_inputs": {}, "criteria": [{}]});
        ok(
            s.post("/orgs/acme/resources/defs", &s.admin, def).await,
            StatusCode::CREATED,
        );
    }
    // An external driver no definition uses, so nothing calls it.
    ok(
        s.post(
            "/orgs/acme/resources/drivers",
            &s.admin,
            json!({"id": "acme/vault", "target": "https://driver.internal/vault",
                   "token": {"value": "fixture-driver-token"},
                   "capabilities": {"observe": true}}),
        )
        .await,
        StatusCode::CREATED,
    );
    ok(
        s.post(
            &format!("{APP}/values"),
            &s.admin,
            json!({"key": "API_URL", "description": "Public API", "value": "https://api.example"}),
        )
        .await,
        StatusCode::CREATED,
    );
    write_secret(s, "shop/key", "fixture-secret").await;

    let web = delta(
        s,
        json!({"add": {"web": {"spec": {"containers": {
        "web": {"image": "nginx:1"}}}}}}),
    )
    .await;
    deploy(s, "development", json!({"delta_id": web})).await;
    let worker = delta(
        s,
        json!({"add": {"worker": {"spec": {"containers": {
        "worker": {"image": "busybox:1"}}}}}}),
    )
    .await;
    deploy(s, "development", json!({"delta_id": worker})).await;
    deploy(s, "production", json!({"delta_id": web})).await;

    // A preview of the environment's deletion, with a permit nothing used.
    let (_, env) = s.get(&format!("{APP}/envs/production"), &s.admin).await;
    let (st, plan) = s
        .post(
            &format!("{APP}/envs/production/plans"),
            &s.admin,
            json!({"kind": "delete_env", "expected_generation": env["generation"]}),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{plan}");
    let permits: Vec<Value> = plan["resources"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["permit_required"] == true)
        .map(|r| json!({"descriptor": r["descriptor"], "action": r["action"]}))
        .collect();
    assert!(!permits.is_empty(), "{plan}");
    ok(
        s.post(
            &format!(
                "{APP}/envs/production/plans/{}/permits",
                plan["id"].as_str().unwrap()
            ),
            &s.admin,
            json!({"permits": permits}),
        )
        .await,
        StatusCode::CREATED,
    );
}

async fn write_secret(s: &Server, name: &str, value: &str) {
    use openporch_store::secrets::{BuiltinSecretStore, SecretStore, SecretValue};
    let keyring = s.state.keyring.clone().unwrap();
    BuiltinSecretStore::new(&s.state.store, keyring, "acme")
        .write(name, &SecretValue::new(value))
        .await
        .unwrap();
}

async fn read_secret(s: &Server, name: &str) -> String {
    use openporch_store::secrets::{BuiltinSecretStore, SecretStore};
    let keyring = s.state.keyring.clone().unwrap();
    BuiltinSecretStore::new(&s.state.store, keyring, "acme")
        .read(&openporch_core::model::SecretRef {
            store: "openporch".to_owned(),
            reference: name.to_owned(),
            version: None,
        })
        .await
        .unwrap()
        .expose()
        .to_owned()
}

/// What the API returns for the installation `populate` built.
async fn observe(s: &Server) -> Value {
    let mut out = serde_json::Map::new();
    let paths = [
        "/orgs/acme/apps".to_owned(),
        "/orgs/acme/users".to_owned(),
        "/orgs/acme/env-types".to_owned(),
        "/orgs/acme/env-types/production/users".to_owned(),
        "/orgs/acme/resources/defs".to_owned(),
        format!("{APP}/envs"),
        format!("{APP}/values"),
        format!("{APP}/envs/development/resources"),
        format!("{APP}/envs/production/resources"),
        format!("{APP}/envs/development/deploys"),
        format!("{APP}/envs/production/deploys"),
    ];
    for path in paths {
        let (st, body) = s.get(&path, &s.admin).await;
        assert_eq!(st, StatusCode::OK, "{path}: {body}");
        out.insert(path, body);
    }
    Value::Object(out)
}

/// Whether `actual` holds everything `expected` does: objects may gain
/// fields, arrays must have the same elements in order.
fn contains(expected: &Value, actual: &Value, at: &str) -> Result<(), String> {
    match (expected, actual) {
        (Value::Object(e), Value::Object(a)) => e.iter().try_for_each(|(k, v)| {
            let a = a.get(k).ok_or_else(|| format!("{at}.{k}: missing"))?;
            contains(v, a, &format!("{at}.{k}"))
        }),
        (Value::Array(e), Value::Array(a)) if e.len() == a.len() => e
            .iter()
            .zip(a)
            .enumerate()
            .try_for_each(|(i, (e, a))| contains(e, a, &format!("{at}[{i}]"))),
        _ if expected == actual => Ok(()),
        _ => Err(format!("{at}: expected {expected}, found {actual}")),
    }
}

/// Stops `s` and returns its data directory.
async fn stop(s: Server) -> tempfile::TempDir {
    let Server {
        dir, state, router, ..
    } = s;
    assert!(
        state
            .drain(std::time::Duration::from_secs(5))
            .await
            .is_empty()
    );
    state.store.pool().close().await;
    drop((state, router));
    dir
}

fn schema_version(dir: &Path) -> i64 {
    let name = dir.file_name().unwrap().to_string_lossy();
    name.parse()
        .unwrap_or_else(|_| panic!("fixture `{name}` is not named by its schema version"))
}

/// Writes the fixture of this build's schema to `OPENPORCH_SCHEMA_FIXTURE`.
#[tokio::test]
#[ignore = "writes a fixture; run when a release changes the schema"]
async fn write_schema_fixture() {
    let out = PathBuf::from(std::env::var("OPENPORCH_SCHEMA_FIXTURE").unwrap());
    assert!(
        !out.exists(),
        "{} exists: a fixture records what an older build wrote, so it is never rewritten",
        out.display()
    );
    let s = server().await;
    populate(&s).await;
    let observed = observe(&s).await;
    let dir = stop(s).await;
    std::fs::create_dir_all(&out).unwrap();
    for entry in std::fs::read_dir(dir.path()).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if path.is_file() && !name.starts_with("openporch.db") && name != "openporch.lock" {
            std::fs::copy(&path, out.join(&name)).unwrap();
        }
    }
    // One compact file, with nothing left in a write-ahead log.
    let db = out.join("openporch.db");
    let _ = std::fs::remove_file(&db);
    let pool = sqlx::SqlitePool::connect(&format!(
        "sqlite://{}",
        dir.path().join("openporch.db").display()
    ))
    .await
    .unwrap();
    sqlx::query("VACUUM INTO ?")
        .bind(db.to_string_lossy().into_owned())
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    // In the journal mode every installation's database has, so that opening
    // it changes no byte unless something is written.
    let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}", db.display()))
        .await
        .unwrap();
    sqlx::query("PRAGMA journal_mode = WAL")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    std::fs::write(
        out.join("observed.json"),
        serde_json::to_string_pretty(&observed).unwrap() + "\n",
    )
    .unwrap();
}

/// Every fixture starts, migrates to this build's schema, serves what the
/// build that wrote it served, keeps its secrets readable and deploys again.
#[tokio::test]
async fn every_schema_fixture_upgrades_to_this_build() {
    let dirs = fixture_dirs();
    assert!(!dirs.is_empty(), "no schema fixtures");
    let current = openporch_store::SCHEMA_VERSION;
    assert!(
        dirs.iter().any(|d| schema_version(d) == current),
        "add the fixture of schema {current}: see this module's documentation"
    );
    for fixture in dirs {
        let version = schema_version(&fixture);
        assert!(
            version <= current,
            "fixture {version} is newer than this build"
        );
        let dir = install(&fixture);
        let mut s = server_in(dir).await;
        // The fixtures' tokens expired a day after they were written; time
        // passing is not what this test checks.
        sqlx::query("UPDATE api_tokens SET expires_at = NULL")
            .execute(s.state.store.pool())
            .await
            .unwrap();
        s.admin = std::fs::read_to_string(s.dir.path().join("bootstrap-token"))
            .unwrap()
            .trim()
            .to_owned();
        let migrated: i64 = sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations")
            .fetch_one(s.state.store.pool())
            .await
            .unwrap();
        assert_eq!(migrated, current, "fixture {version}");

        let expected: Value =
            serde_json::from_str(&std::fs::read_to_string(fixture.join("observed.json")).unwrap())
                .unwrap();
        let actual = observe(&s).await;
        let keys = |v: &Value| v.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
        assert_eq!(keys(&expected), keys(&actual), "fixture {version}");
        contains(&expected, &actual, "")
            .unwrap_or_else(|e| panic!("fixture {version} after upgrade: {e}"));
        assert_eq!(read_secret(&s, "shop/key").await, "fixture-secret");

        // The member's token still authenticates, and deployments still run.
        let alice = std::fs::read_to_string(s.dir.path().join("alice-token")).unwrap();
        let (st, b) = s.get("/orgs/acme/apps", alice.trim()).await;
        assert_eq!(st, StatusCode::OK, "fixture {version}: {b}");
        let api = delta(
            &s,
            json!({"add": {"api": {"spec": {"containers": {
            "api": {"image": "nginx:1"}}}}}}),
        )
        .await;
        deploy(&s, "development", json!({"delta_id": api})).await;
        let (st, b) = s
            .get(&format!("{APP}/envs/development/resources"), &s.admin)
            .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert!(
            b.to_string().contains("modules.api"),
            "fixture {version}: {b}"
        );
        stop(s).await;
    }
}

/// A build that knows only the schema before a fixture's refuses that
/// fixture, before it changes anything, and names the way out. The older build
/// is played by this build's check with the older version: every build since
/// the schema guard checks the version this way before it migrates.
#[tokio::test]
async fn an_older_build_refuses_every_newer_fixture_unchanged() {
    let dirs = fixture_dirs();
    for fixture in dirs.iter().filter(|d| schema_version(d) > 1) {
        let version = schema_version(fixture);
        let dir = install(fixture);
        let db = dir.path().join("openporch.db");
        let before = std::fs::read(&db).unwrap();
        let older = openporch_store::check_schema(&db, version - 1)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            older.contains(&format!("schema version {version} is newer")),
            "{older}"
        );
        assert!(
            older.contains("run a compatible openporch version or restore a compatible backup"),
            "{older}"
        );
        assert_eq!(std::fs::read(&db).unwrap(), before, "fixture {version}");
    }
}
