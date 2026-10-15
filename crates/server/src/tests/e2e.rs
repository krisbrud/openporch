//! End to end in one process: the started server's runner deploys a function
//! workload with a database and a queue trigger through the built-in drivers,
//! redeploys it unchanged, and replays a recorded snapshot after the
//! configuration changed.

use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use openporch_core::catalog::builtin_types;
use openporch_core::workload::Workload;
use openporch_engine::cloud::{
    AssumeRole, AwsCredentials, CallerIdentity, CloudError, EcrImage, Latest, S3Object, S3Version,
    Sts,
};
use openporch_store::Store;
use openporch_store::secrets::BoxFuture;
use serde_json::{Value, json};

use super::{Server, server, server_from};

/// S3 and ECR as the account `aws-dev` sees them: bucket `artifacts` keeps
/// versions, and its objects' current version is `version`; bucket `plain`
/// does not; nothing else exists.
#[derive(Default)]
struct ArtifactSts {
    version: Mutex<String>,
}

impl ArtifactSts {
    fn set(&self, version: &str) {
        *self.version.lock().unwrap() = version.to_owned();
    }
}

impl Sts for ArtifactSts {
    fn assume_role(&self, _: AssumeRole) -> BoxFuture<'_, Result<AwsCredentials, CloudError>> {
        Box::pin(async { Err(CloudError::Unavailable("no STS here".into())) })
    }

    fn caller_identity<'a>(
        &'a self,
        _: &'a AwsCredentials,
    ) -> BoxFuture<'a, Result<CallerIdentity, CloudError>> {
        Box::pin(async {
            Ok(CallerIdentity {
                arn: "arn:aws:iam::111122223333:user/openporch".into(),
                account: "111122223333".into(),
                user_id: "AIDAEXAMPLE".into(),
            })
        })
    }

    fn object_exists<'a>(
        &'a self,
        credentials: Option<&'a AwsCredentials>,
        object: &'a S3Version,
    ) -> BoxFuture<'a, Result<bool, CloudError>> {
        assert!(credentials.is_some());
        let exists = object.bucket == "artifacts";
        Box::pin(async move { Ok(exists) })
    }

    fn latest_version<'a>(
        &'a self,
        credentials: Option<&'a AwsCredentials>,
        object: &'a S3Object,
    ) -> BoxFuture<'a, Result<Latest, CloudError>> {
        Box::pin(async move {
            let c = credentials.expect("read with the definition's account");
            assert_eq!(c.access_key_id, "AKIAEXAMPLE");
            Ok(match object.bucket.as_str() {
                "artifacts" => Latest::Version(self.version.lock().unwrap().clone()),
                "plain" => Latest::Unversioned,
                _ => Latest::Missing,
            })
        })
    }
    /// ECR repository `web` has tags `1` and `2`, each at a digest of its
    /// digit repeated.
    fn image_digest<'a>(
        &'a self,
        credentials: &'a AwsCredentials,
        image: &'a EcrImage,
    ) -> BoxFuture<'a, Result<Option<String>, CloudError>> {
        assert_eq!(credentials.access_key_id, "AKIAEXAMPLE");
        let digest = (image.repository == "web" && matches!(image.tag.as_str(), "1" | "2"))
            .then(|| format!("sha256:{}", image.tag.repeat(64)));
        Box::pin(async move { Ok(digest) })
    }
}

/// A server whose cloud accounts see S3 as `sts` does, with the static
/// account `aws-dev`.
async fn server_with_s3(sts: Arc<ArtifactSts>) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let config = crate::Config::new("acme", dir.path());
    let state = crate::start_with(config, None, sts).await.unwrap();
    let s = server_from(dir, state);
    s.static_account("aws-dev").await;
    s
}

/// A zip function that uses a database and reads from a queue.
const WORKER: &str = r#"
apiVersion: openporch.dev/v1
kind: Workload
metadata: {name: worker}
function:
  package: {type: zip, artifact: "s3://artifacts/worker.zip", runtime: python3.12, handler: app.handler}
  variables:
    DB_HOST: "${resources.db.host}"
    DB_PORT: "${resources.db.port}"
    API_HOST: "${values.API_HOST}"
    REGION: "${values.REGION}"
triggers:
  orders: {queue: {resource: queue, batchSize: 10}}
resources:
  db: {type: postgres}
  queue: {type: sqs}
"#;

const APP: &str = "/orgs/acme/apps/shop";
const WORKLOAD: &str = "workload.default#modules.worker";
const DB: &str = "postgres.default#modules.worker.externals.db";
const QUEUE: &str = "sqs.default#modules.worker.externals.queue";

/// The database's definition: a template that renders its outputs.
fn postgres(port: u16) -> Value {
    json!({"values": {
        "host": "db.internal",
        "templates": {"outputs": format!("host: \"{{{{ driver.values.host }}}}\"\nport: {port}\n")},
    }})
}

impl Server {
    /// Deploys `body` to `env` at its current generation and waits for the
    /// outcome.
    async fn deploy_to(&self, env: &str, mut body: Value) -> Value {
        let path = format!("{APP}/envs/{env}");
        let (_, record) = self.get(&path, &self.admin).await;
        body["expected_generation"] = record["generation"].clone();
        let (s, d) = self
            .post(&format!("{path}/deploys"), &self.admin, body)
            .await;
        assert_eq!(s, StatusCode::ACCEPTED, "{d}");
        let id = d["id"].as_str().unwrap();
        for _ in 0..500 {
            let (_, done) = self.get(&format!("{path}/deploys/{id}"), &self.admin).await;
            if done["status"] != "in_progress" {
                assert_eq!(done["status"], "succeeded", "{done}");
                return done;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("deployment {id} did not finish");
    }

    /// The active resources of `env`, by descriptor.
    async fn active(&self, env: &str) -> std::collections::BTreeMap<String, Value> {
        let (s, b) = self
            .get(&format!("{APP}/envs/{env}/resources"), &self.admin)
            .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        b["resources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| (r["descriptor"].as_str().unwrap().to_owned(), r.clone()))
            .collect()
    }
}

/// The artifact the worker deployed.
fn artifact(active: &std::collections::BTreeMap<String, Value>) -> Value {
    active[WORKLOAD]["outputs"]["spec"]["function"]["package"]["artifact"].clone()
}

/// The worker's resolved variables, as its `openporch/none` definition stored
/// them.
fn variables(active: &std::collections::BTreeMap<String, Value>) -> Value {
    active[WORKLOAD]["outputs"]["spec"]["function"]["variables"].clone()
}

#[tokio::test]
async fn a_function_with_a_database_and_a_queue_deploys_and_replays() {
    let sts = Arc::new(ArtifactSts::default());
    sts.set("v1");
    let s = server_with_s3(sts.clone()).await;
    s.app("shop").await;
    for (id, ty, driver, inputs) in [
        ("env", "base-env", "openporch/none", json!({})),
        ("workloads", "workload", "openporch/none", json!({})),
        ("pg", "postgres", "openporch/template", postgres(5432)),
        (
            "queues",
            "sqs",
            "openporch/echo",
            json!({"values": {"url": "https://sqs.example/orders"}}),
        ),
    ] {
        let mut def = json!({"id": id, "type": ty, "driver_type": driver,
                             "driver_inputs": inputs, "criteria": [{}]});
        // The worker's artifact is read with the workload definition's account.
        if ty == "workload" {
            def["driver_account"] = json!("aws-dev");
        }
        let (st, b) = s.post("/orgs/acme/resources/defs", &s.admin, def).await;
        assert_eq!(st, StatusCode::CREATED, "{b}");
    }
    // Shared values, with a staging override for the API host.
    for (key, value) in [("API_HOST", "api.shared"), ("REGION", "eu-north-1")] {
        let (st, b) = s
            .post(
                &format!("{APP}/values"),
                &s.admin,
                json!({"key": key, "description": "", "value": value}),
            )
            .await;
        assert_eq!(st, StatusCode::CREATED, "{b}");
    }
    let (st, b) = s
        .post(
            &format!("{APP}/values"),
            &s.admin,
            json!({"key": "TOKEN", "description": "", "secret_ref": {"value": "hunter2-token"}}),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{b}");
    let (st, b) = s
        .post(
            &format!("{APP}/envs"),
            &s.admin,
            json!({"id": "staging", "name": "Staging", "type": "development"}),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{b}");
    let (st, b) = s
        .post(
            &format!("{APP}/envs/staging/values"),
            &s.admin,
            json!({"key": "API_HOST", "description": "", "value": "api.staging"}),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{b}");

    // Deploy a delta that adds the worker.
    let module = Workload::parse(WORKER, None)
        .unwrap()
        .to_module(&builtin_types())
        .unwrap()
        .module;
    let (st, delta) = s
        .post(
            &format!("{APP}/deltas"),
            &s.admin,
            json!({"modules": {"add": {"worker": module}}}),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{delta}");
    let first = s
        .deploy_to("development", json!({"delta_id": delta["id"]}))
        .await;
    let set_id = first["set_id"].as_str().unwrap().to_owned();
    let (_, env) = s.get(&format!("{APP}/envs/development"), &s.admin).await;
    assert_eq!(env["desired_set_id"], set_id.as_str());
    assert_eq!(env["last_successful_set_id"], set_id.as_str());
    let (_, frozen) = s
        .get(
            &format!("{APP}/deltas/{}", delta["id"].as_str().unwrap()),
            &s.admin,
        )
        .await;
    assert_eq!(frozen["frozen"], true);
    // The snapshot records every value it resolved, a secret as a reference
    // only.
    let snapshot = s
        .state
        .store
        .deployment_snapshot("acme", "shop", "development", first["id"].as_str().unwrap())
        .await
        .unwrap()
        .unwrap();
    let values = &snapshot["values"];
    assert!(values["versions"]["app"].is_string(), "{values}");
    assert_eq!(
        values["resolved"]["API_HOST"],
        json!({"value": "api.shared"})
    );
    assert!(
        values["resolved"]["TOKEN"]["secret_ref"]["ref"].is_string(),
        "{values}"
    );
    assert!(!snapshot.to_string().contains("hunter2"), "{snapshot}");
    let dev = s.active("development").await;
    assert_eq!(
        dev.keys().map(String::as_str).collect::<Vec<_>>(),
        ["base-env.default#base-env", DB, QUEUE, WORKLOAD]
    );
    // The template rendered the database's outputs, and the worker got them,
    // the application's values and the queue's outputs.
    assert_eq!(
        dev[DB]["outputs"],
        json!({"host": "db.internal", "port": 5432})
    );
    assert_eq!(
        variables(&dev),
        json!({"DB_HOST": "db.internal", "DB_PORT": 5432, "API_HOST": "api.shared",
               "REGION": "eu-north-1"})
    );
    let triggers = &dev[WORKLOAD]["outputs"]["spec"]["triggers"];
    assert_eq!(
        triggers["orders"]["queue"]["outputs"],
        json!({"url": "https://sqs.example/orders"})
    );
    assert!(
        dev[WORKLOAD]["dependencies"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d == DB)
    );
    // The artifact deployed is the object version current at acceptance; the
    // set keeps the name it was given.
    assert_eq!(artifact(&dev), "s3://artifacts/worker.zip?versionId=v1");
    let (_, set) = s.get(&format!("{APP}/sets/{set_id}"), &s.admin).await;
    assert_eq!(
        set["modules"]["worker"]["spec"]["function"]["package"]["artifact"],
        "s3://artifacts/worker.zip"
    );
    sts.set("v2");

    // The same set in staging resolves the override.
    let staged = s.deploy_to("staging", json!({"set_id": set_id})).await;
    assert_eq!(staged["set_id"], set_id.as_str());
    let staging = s.active("staging").await;
    assert_eq!(variables(&staging)["API_HOST"], "api.staging");
    assert_eq!(variables(&staging)["REGION"], "eu-north-1");
    assert_ne!(staging[DB]["guresid"], dev[DB]["guresid"]);

    // Deploying the same set again keeps every resource's identity.
    let again = s.deploy_to("development", json!({"set_id": set_id})).await;
    assert_eq!(again["set_id"], set_id.as_str());
    let unchanged = s.active("development").await;
    for (descriptor, resource) in &dev {
        assert_eq!(unchanged[descriptor]["guresid"], resource["guresid"]);
    }
    assert_eq!(unchanged[DB]["usage"]["last_deploy_id"], again["id"]);

    // The configuration changes: a new database port and API host.
    let (st, b) = s
        .put(
            "/orgs/acme/resources/defs/pg",
            &s.admin,
            json!({"driver_type": "openporch/template", "driver_inputs": postgres(6432)}),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    let (st, b) = s
        .put(
            &format!("{APP}/values/API_HOST"),
            &s.admin,
            json!({"description": "", "value": "api.v2"}),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{b}");

    // Replaying the first deployment's snapshot uses the configuration it
    // recorded.
    let replayed = s
        .deploy_to(
            "development",
            json!({"deploy_id": first["id"], "replay": "snapshot"}),
        )
        .await;
    assert_eq!(replayed["set_id"], set_id.as_str());
    let old = s.active("development").await;
    assert_eq!(
        old[DB]["outputs"],
        json!({"host": "db.internal", "port": 5432})
    );
    assert_eq!(variables(&old)["API_HOST"], "api.shared");
    assert_eq!(artifact(&old), "s3://artifacts/worker.zip?versionId=v1");
    assert_eq!(old[DB]["guresid"], dev[DB]["guresid"]);
    assert_eq!(old[DB]["definition_version"], 1);

    // Redeploying it with the current configuration resolves the new one.
    s.deploy_to(
        "development",
        json!({"deploy_id": first["id"], "replay": "current"}),
    )
    .await;
    let current = s.active("development").await;
    assert_eq!(
        current[DB]["outputs"],
        json!({"host": "db.internal", "port": 6432})
    );
    assert_eq!(variables(&current)["API_HOST"], "api.v2");
    assert_eq!(artifact(&current), "s3://artifacts/worker.zip?versionId=v2");
    assert_eq!(current[DB]["guresid"], dev[DB]["guresid"]);
    assert_eq!(current[DB]["definition_version"], 2);
    assert_eq!(variables(&current)["DB_PORT"], 6432);
    // Staging keeps its override.
    let staged_again = s.deploy_to("staging", json!({"set_id": set_id})).await;
    assert_eq!(staged_again["status"], "succeeded");
    let staging = s.active("staging").await;
    assert_eq!(variables(&staging)["API_HOST"], "api.staging");
    assert_eq!(variables(&staging)["DB_PORT"], 6432);
}

/// `Mutable artifact` in `workload-spec`: an artifact without a fixed identity
/// is pinned, or the deployment is refused before anything changes.
#[tokio::test]
async fn artifacts_without_a_fixed_identity_are_refused() {
    let s = server_with_s3(Arc::new(ArtifactSts::default())).await;
    s.app("shop").await;
    for (id, ty) in [("env", "base-env"), ("workloads", "workload")] {
        let (st, b) = s
            .post(
                "/orgs/acme/resources/defs",
                &s.admin,
                json!({"id": id, "type": ty, "driver_type": "openporch/none",
                       "driver_account": "aws-dev", "criteria": [{}]}),
            )
            .await;
        assert_eq!(st, StatusCode::CREATED, "{b}");
    }
    for (artifact, expected) in [
        ("s3://plain/worker.zip", "enable versioning"),
        ("s3://elsewhere/worker.zip", "does not exist"),
        ("https://example.com/worker.zip", "no fixed identity"),
    ] {
        let doc = format!(
            "apiVersion: openporch.dev/v1\nkind: Workload\nmetadata: {{name: worker}}\n\
             function: {{package: {{type: zip, artifact: \"{artifact}\", runtime: python3.12, \
             handler: app.handler}}}}\n"
        );
        let (st, delta) = s
            .post(
                &format!("{APP}/deltas"),
                &s.admin,
                json!({"env": "development", "workload": doc}),
            )
            .await;
        assert_eq!(st, StatusCode::CREATED, "{delta}");
        let (st, b) = s
            .post(
                &format!("{APP}/envs/development/deploys"),
                &s.admin,
                json!({"delta_id": delta["id"], "expected_generation": delta["base_generation"]}),
            )
            .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{artifact}: {b}");
        assert!(
            b["message"].as_str().unwrap().contains(expected),
            "{artifact}: {b}"
        );
    }
    // Nothing was deployed.
    let (_, page) = s
        .get(&format!("{APP}/envs/development/deploys"), &s.admin)
        .await;
    assert_eq!(page["deployments"], json!([]));
}

/// Writes a new version of the built-in secret `name`.
async fn write_secret(s: &Server, name: &str, value: &str) {
    use openporch_store::secrets::{BuiltinSecretStore, SecretStore, SecretValue};
    let keyring = s.state.keyring.clone().unwrap();
    BuiltinSecretStore::new(&s.state.store, keyring, "acme")
        .write(name, &SecretValue::new(value))
        .await
        .unwrap();
}

#[tokio::test]
async fn a_deployment_pins_its_versions_and_replay_refuses_missing_inputs() {
    let s = server().await;
    s.app("shop").await;
    write_secret(&s, "shop/pg-password", "first").await;
    s.grant_secret("openporch", "shop/pg-password").await;
    for (id, ty, driver, inputs) in [
        ("env", "base-env", "openporch/none", json!({})),
        ("workloads", "workload", "openporch/none", json!({})),
        (
            "pg",
            "postgres",
            "openporch/echo",
            json!({"values": {"host": "db.internal"},
                   "secret_refs": {"password": {"store": "openporch", "ref": "shop/pg-password"}}}),
        ),
    ] {
        let (st, b) = s
            .post(
                "/orgs/acme/resources/defs",
                &s.admin,
                json!({"id": id, "type": ty, "driver_type": driver, "driver_inputs": inputs,
                       "criteria": [{}]}),
            )
            .await;
        assert_eq!(st, StatusCode::CREATED, "{b}");
    }
    let (st, b) = s
        .post(
            &format!("{APP}/values"),
            &s.admin,
            json!({"key": "API_HOST", "description": "", "value": "api.v1"}),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{b}");
    let (st, delta) = s
        .post(
            &format!("{APP}/deltas"),
            &s.admin,
            json!({"modules": {"add": {"web": {"externals": {"db": {"type": "postgres"}},
                "spec": {"containers": {"web": {"image": "web:1"}},
                         "env": {"API_HOST": "${values.API_HOST}"}}}}}}),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{delta}");
    let first = s
        .deploy_to("development", json!({"delta_id": delta["id"]}))
        .await;
    let first_id = first["id"].as_str().unwrap().to_owned();
    let read = |id: String| {
        let s = &s;
        async move {
            let (st, d) = s
                .get(&format!("{APP}/envs/development/deploys/{id}"), &s.admin)
                .await;
            assert_eq!(st, StatusCode::OK, "{d}");
            d
        }
    };
    let snapshot = |id: String| {
        let s = &s;
        async move {
            s.state
                .store
                .deployment_snapshot("acme", "shop", "development", &id)
                .await
                .unwrap()
                .unwrap()
        }
    };
    let password = |snapshot: &Value| {
        snapshot["definitions"]["pg"]["definition"]["driver_inputs"]["secret_refs"]["password"]
            ["version"]
            .clone()
    };
    // The deployment names the value set version it resolved, and its
    // snapshot pins the secret version the definition left open.
    assert_eq!(
        read(first_id.clone()).await["value_versions"],
        json!({"app": "1", "env": null})
    );
    assert_eq!(password(&snapshot(first_id.clone()).await), "1");

    // A value changes and the secret rotates.
    let (st, b) = s
        .put(
            &format!("{APP}/values/API_HOST"),
            &s.admin,
            json!({"description": "", "value": "api.v2"}),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    write_secret(&s, "shop/pg-password", "second").await;
    assert_eq!(
        read(first_id.clone()).await["value_versions"]["app"],
        "1",
        "the record still names the earlier version"
    );

    // Snapshot replay uses the recorded versions; `current` the new ones.
    let replayed = s
        .deploy_to(
            "development",
            json!({"deploy_id": first_id, "replay": "snapshot"}),
        )
        .await;
    let replayed_id = replayed["id"].as_str().unwrap().to_owned();
    assert_eq!(
        read(replayed_id.clone()).await["value_versions"]["app"],
        "1"
    );
    assert_eq!(password(&snapshot(replayed_id).await), "1");
    let web = "workload.default#modules.web";
    assert_eq!(
        s.active("development").await[web]["outputs"]["spec"]["env"]["API_HOST"],
        "api.v1"
    );
    let current = s
        .deploy_to(
            "development",
            json!({"deploy_id": first_id, "replay": "current"}),
        )
        .await;
    let current_id = current["id"].as_str().unwrap().to_owned();
    assert_eq!(read(current_id.clone()).await["value_versions"]["app"], "2");
    assert_eq!(password(&snapshot(current_id.clone()).await), "2");
    assert_eq!(
        s.active("development").await[web]["outputs"]["spec"]["env"]["API_HOST"],
        "api.v2"
    );

    // A secret version the snapshot pinned is gone: replay fails before
    // anything runs, naming it.
    sqlx::query("DELETE FROM secret_versions WHERE ref = 'shop/pg-password' AND version = 1")
        .execute(s.state.store.pool())
        .await
        .unwrap();
    let path = format!("{APP}/envs/development");
    let generation = s.get(&path, &s.admin).await.1["generation"].clone();
    let (st, b) = s
        .post(
            &format!("{path}/deploys"),
            &s.admin,
            json!({"deploy_id": first_id, "replay": "snapshot",
                   "expected_generation": generation}),
        )
        .await;
    super::assert_error(st, &b, StatusCode::BAD_REQUEST, "API-003");
    assert!(
        b["message"]
            .as_str()
            .unwrap()
            .contains("secret `openporch/shop/pg-password` version 1, which no longer exists"),
        "{b}"
    );
    // So is a driver this server no longer runs.
    let mut registry = openporch_drivers::DriverRegistry::default();
    registry
        .register(std::sync::Arc::new(openporch_drivers::NoneDriver::new()) as _)
        .ok();
    s.state.drivers.set(std::sync::Arc::new(registry), None);
    let (st, b) = s
        .post(
            &format!("{path}/deploys"),
            &s.admin,
            json!({"deploy_id": current_id, "replay": "snapshot",
                   "expected_generation": generation}),
        )
        .await;
    super::assert_error(st, &b, StatusCode::BAD_REQUEST, "API-003");
    assert!(
        b["message"].as_str().unwrap().contains(
            "driver `openporch/echo` at protocol builtin, which this server no longer runs"
        ),
        "{b}"
    );
    let (_, env) = s.get(&path, &s.admin).await;
    assert_eq!(env["generation"], generation, "nothing was accepted");
}

const ECR: &str = "111122223333.dkr.ecr.eu-west-1.amazonaws.com";

/// Image tags in ECR are pinned to digests at acceptance: the snapshot runs
/// the digest, the set keeps the tag, and a missing tag deploys nothing.
#[tokio::test]
async fn ecr_image_tags_are_pinned_to_digests_at_acceptance() {
    let s = server_with_s3(Arc::new(ArtifactSts::default())).await;
    s.app("shop").await;
    for (id, ty) in [("env", "base-env"), ("workloads", "workload")] {
        let (st, b) = s
            .post(
                "/orgs/acme/resources/defs",
                &s.admin,
                json!({"id": id, "type": ty, "driver_type": "openporch/none",
                       "driver_account": "aws-dev", "criteria": [{}]}),
            )
            .await;
        assert_eq!(st, StatusCode::CREATED, "{b}");
    }
    let deploy = |image: String| {
        let s = &s;
        async move {
            let doc = "apiVersion: openporch.dev/v1\nkind: Workload\nmetadata: {name: web}\n\
                       containers: {web: {image: \".\"}, sidecar: {image: \"nginx:1\"}}\n";
            let (st, delta) = s
                .post(
                    &format!("{APP}/deltas"),
                    &s.admin,
                    json!({"env": "development", "workload": doc,
                           "overrides": {"images": {"web": image}}}),
                )
                .await;
            assert_eq!(st, StatusCode::CREATED, "{delta}");
            s.post(
                &format!("{APP}/envs/development/deploys"),
                &s.admin,
                json!({"delta_id": delta["id"], "expected_generation": delta["base_generation"]}),
            )
            .await
        }
    };

    let (st, b) = deploy(format!("{ECR}/web:missing")).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
    assert!(
        b["message"].as_str().unwrap().contains("does not exist"),
        "{b}"
    );

    let (st, d) = deploy(format!("{ECR}/web:1")).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{d}");
    let id = d["id"].as_str().unwrap().to_owned();
    let d = s
        .finished_at(&format!("{APP}/envs/development/deploys/{id}"))
        .await;
    assert_eq!(d["status"], "succeeded", "{d}");
    let snapshot = s
        .state
        .store
        .deployment_snapshot("acme", "shop", "development", &id)
        .await
        .unwrap()
        .unwrap();
    let containers =
        &snapshot["graph"]["nodes"]["workload.default#modules.web"]["params"]["spec"]["containers"];
    assert_eq!(
        containers["web"]["image"],
        format!("{ECR}/web@sha256:{}", "1".repeat(64))
    );
    // Another registry's image is not pinned.
    assert_eq!(containers["sidecar"]["image"], "nginx:1");
    // The set keeps the tag it was given.
    let (_, set) = s
        .get(
            &format!("{APP}/sets/{}", d["set_id"].as_str().unwrap()),
            &s.admin,
        )
        .await;
    assert_eq!(
        set["modules"]["web"]["spec"]["containers"]["web"]["image"],
        format!("{ECR}/web:1")
    );
}
