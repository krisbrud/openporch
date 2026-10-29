//! End to end in one process: the started server's runner deploys a function
//! workload with a database and a queue trigger through the built-in drivers,
//! redeploys it unchanged, and replays a recorded snapshot after the
//! configuration changed.

use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use openporch_core::catalog::builtin_types;
use openporch_core::workload::Workload;
use openporch_engine::cloud::{
    AssumeRole, AwsCredentials, CallerIdentity, CloudError, EcrDigest, EcrImage, Latest, S3Object,
    S3Version, Sts,
};
use openporch_store::Store;
use openporch_store::secrets::BoxFuture;
use serde_json::{Value, json};

use super::{Server, server, server_from, server_in};

/// S3 and ECR as the account `aws-dev` sees them: bucket `artifacts` keeps
/// versions, and its objects' current version is `version`; bucket `plain`
/// does not; nothing else exists.
#[derive(Default)]
struct ArtifactSts {
    version: Mutex<String>,
    /// Image digests deleted from ECR.
    gone_images: Mutex<Vec<String>>,
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
    /// ECR repositories `web` and `lambda-api` have tags `1` and `2`, each at
    /// a digest of its digit repeated.
    fn image_digest<'a>(
        &'a self,
        credentials: &'a AwsCredentials,
        image: &'a EcrImage,
    ) -> BoxFuture<'a, Result<Option<String>, CloudError>> {
        assert_eq!(credentials.access_key_id, "AKIAEXAMPLE");
        let digest = (matches!(image.repository.as_str(), "web" | "lambda-api")
            && matches!(image.tag.as_str(), "1" | "2"))
        .then(|| format!("sha256:{}", image.tag.repeat(64)));
        Box::pin(async move { Ok(digest) })
    }
    fn image_exists<'a>(
        &'a self,
        credentials: &'a AwsCredentials,
        image: &'a EcrDigest,
    ) -> BoxFuture<'a, Result<bool, CloudError>> {
        assert_eq!(credentials.access_key_id, "AKIAEXAMPLE");
        let gone = self.gone_images.lock().unwrap().contains(&image.digest);
        Box::pin(async move { Ok(!gone) })
    }
}

/// A server whose cloud accounts see S3 as `sts` does, with the static
/// account `aws-dev`.
async fn server_with_s3(sts: Arc<ArtifactSts>) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let mut config = crate::Config::new("acme", dir.path());
    config.images.pin_other_registries = false;
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
        "templates": {"outputs": format!("host: \"{{{{ driver.values.host }}}}\"\nname: shop\nport: {port}\n"),
                      "secrets": "username: shop\npassword: change-me\n"},
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
            json!({"values": {"url": "https://sqs.example/orders",
                              "arn": "arn:aws:sqs:eu-north-1:111122223333:orders",
                              "region": "eu-north-1"}}),
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
        json!({"host": "db.internal", "name": "shop", "port": 5432})
    );
    assert_eq!(
        variables(&dev),
        json!({"DB_HOST": "db.internal", "DB_PORT": 5432, "API_HOST": "api.shared",
               "REGION": "eu-north-1"})
    );
    let triggers = &dev[WORKLOAD]["outputs"]["spec"]["triggers"];
    assert_eq!(
        triggers["orders"]["queue"]["outputs"],
        json!({"url": "https://sqs.example/orders",
               "arn": "arn:aws:sqs:eu-north-1:111122223333:orders", "region": "eu-north-1"})
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
        json!({"host": "db.internal", "name": "shop", "port": 5432})
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
        json!({"host": "db.internal", "name": "shop", "port": 6432})
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
            json!({"values": {"host": "db.internal", "name": "shop", "port": 5432},
                   "secret_refs": {"username": {"value": "shop"},
                                   "password": {"store": "openporch", "ref": "shop/pg-password"}}}),
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
    let sts = Arc::new(ArtifactSts::default());
    let s = server_with_s3(sts.clone()).await;
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
    // With `pin_other_registries` off, another registry's image is not pinned.
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
    // The pinned digest is deleted from the repository: a replay of the
    // snapshot fails before anything runs, naming it.
    sts.gone_images
        .lock()
        .unwrap()
        .push(format!("sha256:{}", "1".repeat(64)));
    let (_, env) = s.get(&format!("{APP}/envs/development"), &s.admin).await;
    let (st, replay) = s
        .post(
            &format!("{APP}/envs/development/deploys"),
            &s.admin,
            json!({"deploy_id": id, "replay": "snapshot",
                   "expected_generation": env["generation"]}),
        )
        .await;
    assert_eq!(st, StatusCode::ACCEPTED, "{replay}");
    let replay = s
        .finished_at(&format!(
            "{APP}/envs/development/deploys/{}",
            replay["id"].as_str().unwrap()
        ))
        .await;
    assert_eq!(replay["status"], "failed", "{replay}");
    let error = replay["error"].to_string();
    assert!(
        error.contains(&format!("image `{ECR}/web@sha256:{}`", "1".repeat(64)))
            && error.contains("no longer exists"),
        "{replay}"
    );
}

/// A registry without authentication: `web:1` is at a digest of ones until
/// it is deleted, and `private` answers only with credentials.
/// A registry whose `web` repository is served while `state` is `public`,
/// needs credentials when it is `private` and is gone when it is `deleted`.
async fn fake_registry() -> (String, Arc<Mutex<&'static str>>) {
    use axum::extract::{Path, State};
    use axum::response::IntoResponse;
    let state = Arc::new(Mutex::new("public"));
    async fn manifest(
        State(state): State<Arc<Mutex<&'static str>>>,
        Path((repository, reference)): Path<(String, String)>,
    ) -> axum::response::Response {
        let digest = format!("sha256:{}", "1".repeat(64));
        let state = *state.lock().unwrap();
        match repository.as_str() {
            "web" if state == "private" => StatusCode::UNAUTHORIZED.into_response(),
            "web" if state == "public" && (reference == "1" || reference == digest) => {
                ([("docker-content-digest", digest)], "{}").into_response()
            }
            "private" => StatusCode::UNAUTHORIZED.into_response(),
            _ => StatusCode::NOT_FOUND.into_response(),
        }
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let host = listener.local_addr().unwrap().to_string();
    let app = axum::Router::new()
        .route(
            "/v2/{repository}/manifests/{reference}",
            axum::routing::get(manifest),
        )
        .with_state(state.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (host, state)
}

#[tokio::test]
async fn images_in_other_registries_are_pinned_and_checked() {
    let (registry, registry_state) = fake_registry().await;
    let dir = tempfile::tempdir().unwrap();
    let mut config = crate::Config::new("acme", dir.path());
    config.images.insecure_registries = vec![registry.clone()];
    let s = server_from(dir, crate::start(config, None).await.unwrap());
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
    let deploy = |image: String| {
        let s = &s;
        async move {
            let doc = "apiVersion: openporch.dev/v1\nkind: Workload\nmetadata: {name: web}\n\
                       containers: {web: {image: \".\"}}\n";
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

    // A tag that does not exist, or one the registry hides without
    // credentials, is refused before anything is accepted.
    let (st, b) = deploy(format!("{registry}/web:2")).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
    assert!(
        b["message"].as_str().unwrap().contains("does not exist"),
        "{b}"
    );
    let (st, b) = deploy(format!("{registry}/private:1")).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
    assert!(
        b["message"].as_str().unwrap().contains("name it by digest"),
        "{b}"
    );

    let hex = "1".repeat(64);
    let image_of = async |id: &str| {
        let snapshot = s
            .state
            .store
            .deployment_snapshot("acme", "shop", "development", id)
            .await
            .unwrap()
            .unwrap();
        snapshot["graph"]["nodes"]["workload.default#modules.web"]["params"]["spec"]["containers"]
            ["web"]["image"]
            .clone()
    };

    // A private image named by digest drops any tag and is not checked.
    let (st, d) = deploy(format!("{registry}/private:1@sha256:{hex}")).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{d}");
    let id = d["id"].as_str().unwrap().to_owned();
    let d = s
        .finished_at(&format!("{APP}/envs/development/deploys/{id}"))
        .await;
    assert_eq!(d["status"], "succeeded", "{d}");
    assert_eq!(
        image_of(&id).await,
        format!("{registry}/private@sha256:{hex}")
    );

    // A public tag is pinned to its digest in the snapshot, keeping the tag.
    let (st, d) = deploy(format!("{registry}/web:1")).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{d}");
    let id = d["id"].as_str().unwrap().to_owned();
    let d = s
        .finished_at(&format!("{APP}/envs/development/deploys/{id}"))
        .await;
    assert_eq!(d["status"], "succeeded", "{d}");
    let pinned = format!("{registry}/web:1@sha256:{hex}");
    assert_eq!(image_of(&id).await, pinned);

    // The repository turns private, then the digest is deleted: each time a
    // replay fails before anything runs, naming the image.
    for (state, why) in [
        ("private", "no longer shows it without credentials"),
        ("deleted", "no longer exists"),
    ] {
        *registry_state.lock().unwrap() = state;
        let (_, env) = s.get(&format!("{APP}/envs/development"), &s.admin).await;
        let (st, replay) = s
            .post(
                &format!("{APP}/envs/development/deploys"),
                &s.admin,
                json!({"deploy_id": id, "replay": "snapshot",
                       "expected_generation": env["generation"]}),
            )
            .await;
        assert_eq!(st, StatusCode::ACCEPTED, "{replay}");
        let replay = s
            .finished_at(&format!(
                "{APP}/envs/development/deploys/{}",
                replay["id"].as_str().unwrap()
            ))
            .await;
        assert_eq!(replay["status"], "failed", "{replay}");
        let error = replay["error"].to_string();
        assert!(
            error.contains(&format!("image `{pinned}`")) && error.contains(why),
            "{state}: {replay}"
        );
    }
}

const ORDER_WORKER: &str = include_str!("../../../../examples/lambda-order-worker/workload.yaml");
const LAMBDA_API: &str = include_str!("../../../../examples/lambda-api/workload.yaml");
const WORKER_FN: &str = "workload.aws-lambda#modules.order-worker";
const API_FN: &str = "workload.aws-lambda#modules.lambda-api";
const ORDERS: &str = "dynamodb-table.default#modules.order-worker.externals.orders";
const INBOX: &str = "sqs.default#modules.order-worker.externals.inbox";
const ORDERS_POLICY: &str = "aws-policy.dynamodb-table#modules.order-worker.externals.orders";
const INBOX_POLICY: &str = "aws-policy.sqs#modules.order-worker.externals.inbox";

/// The resource inputs each active resource of `env` last ran with, such as a
/// workload's resolved document, with each secret as `{"secret": <its
/// expression>}`.
async fn inputs(s: &Server, env: &str) -> std::collections::BTreeMap<String, Value> {
    /// A stored `Resolved` as JSON.
    fn plain(r: &Value) -> Value {
        if let Some(v) = r.get("value") {
            v.clone()
        } else if let Some(secret) = r.get("secret") {
            json!({"secret": secret})
        } else if let Some(items) = r.get("array") {
            items.as_array().unwrap().iter().map(plain).collect()
        } else {
            let fields = r["object"].as_array().unwrap();
            Value::Object(
                fields
                    .iter()
                    .map(|kv| (kv[0].as_str().unwrap().to_owned(), plain(&kv[1])))
                    .collect(),
            )
        }
    }
    s.state
        .store
        .active_resources("acme", "shop", env)
        .await
        .unwrap()
        .into_iter()
        .map(|r| (r.descriptor, plain(&r.inputs["resource"])))
        .collect()
}

/// Definitions shaped like `examples/aws-platform`'s, with fake drivers: the
/// `aws-lambda` class stores its resolved document (`openporch/none`), and
/// tables, queues and their co-provisioned policies echo outputs named like
/// the real modules'.
async fn example_platform(s: &Server, queue_url: &str) {
    for (ty, class) in [
        ("workload", "aws-lambda"),
        ("aws-policy", "dynamodb-table"),
        ("aws-policy", "sqs"),
    ] {
        let (st, b) = s
            .post(
                &format!("/orgs/acme/resources/types/{ty}/classes"),
                &s.admin,
                json!({"id": class}),
            )
            .await;
        assert_eq!(st, StatusCode::CREATED, "{b}");
    }
    let name = "${context.app.id}.${context.env.id}.${context.res.id}";
    let defs = [
        json!({"id": "base-env", "type": "base-env", "driver_type": "openporch/none", "criteria": [{}]}),
        json!({"id": "aws-lambda-workload", "type": "workload", "driver_type": "openporch/none",
               "driver_account": "aws-dev", "criteria": [{"class": "aws-lambda"}]}),
        json!({"id": "dynamodb-table", "type": "dynamodb-table", "driver_type": "openporch/echo",
               "driver_inputs": {"values": {
                   "name": name, "region": "eu-north-1",
                   "arn": format!("arn:aws:dynamodb:eu-north-1:111122223333:table/{name}")}},
               "provision": {"aws-policy.dynamodb-table": {"match_dependents": true}},
               "criteria": [{}]}),
        json!({"id": "sqs", "type": "sqs", "driver_type": "openporch/echo",
               "driver_inputs": {"values": {
                   "url": queue_url, "region": "eu-north-1",
                   "arn": "arn:aws:sqs:eu-north-1:111122223333:openporch-${context.res.guresid}"}},
               "provision": {"aws-policy.sqs": {"match_dependents": true}},
               "criteria": [{}]}),
        json!({"id": "dynamodb-table-policy", "type": "aws-policy", "driver_type": "openporch/echo",
               "driver_inputs": {"values": {
                   "arn": "arn:aws:iam::111122223333:policy/openporch/openporch-${context.res.guresid}",
                   "table_arn": "${resources.dynamodb-table.default.outputs.arn}"}},
               "criteria": [{"class": "dynamodb-table"}]}),
        json!({"id": "sqs-policy", "type": "aws-policy", "driver_type": "openporch/echo",
               "driver_inputs": {"values": {
                   "arn": "arn:aws:iam::111122223333:policy/openporch/openporch-${context.res.guresid}",
                   "queue_arn": "${resources.sqs.default.outputs.arn}"}},
               "criteria": [{"class": "sqs"}]}),
    ];
    for def in defs {
        let (st, b) = s.post("/orgs/acme/resources/defs", &s.admin, def).await;
        assert_eq!(st, StatusCode::CREATED, "{b}");
    }
}

/// 5.7's end-to-end run with the files in `examples/lambda-order-worker` and
/// `examples/lambda-api`, deployed as CI deploys them, against definitions
/// shaped like `examples/aws-platform`'s with fake drivers.
#[tokio::test]
async fn the_example_lambda_workloads_deploy_and_replay() {
    let sts = Arc::new(ArtifactSts::default());
    sts.set("v1");
    let s = server_with_s3(sts.clone()).await;
    s.app("shop").await;
    example_platform(
        &s,
        "https://sqs.eu-north-1.amazonaws.com/111122223333/inbox-1",
    )
    .await;
    let (st, b) = s
        .post(
            &format!("{APP}/values"),
            &s.admin,
            json!({"key": "ORDER_SIGNING_KEY", "description": "",
                   "secret_ref": {"value": "order-signing-key-1"}}),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{b}");

    // Each workload is deployed from its file, with what CI supplies.
    let mut deploys = Vec::new();
    for (doc, overrides) in [
        (
            ORDER_WORKER,
            json!({"artifacts": {"main": "s3://artifacts/order-worker/0f3a.zip"}}),
        ),
        (
            LAMBDA_API,
            json!({"images": {"main": format!("{ECR}/lambda-api:1")}}),
        ),
    ] {
        let (st, delta) = s
            .post(
                &format!("{APP}/deltas"),
                &s.admin,
                json!({"env": "development", "workload": doc, "overrides": overrides}),
            )
            .await;
        assert_eq!(st, StatusCode::CREATED, "{delta}");
        deploys.push(
            s.deploy_to("development", json!({"delta_id": delta["id"]}))
                .await,
        );
    }
    let last = deploys.pop().unwrap();
    let set_id = last["set_id"].as_str().unwrap().to_owned();
    let dev = s.active("development").await;
    assert_eq!(
        dev.keys().map(String::as_str).collect::<Vec<_>>(),
        [
            ORDERS_POLICY,
            INBOX_POLICY,
            "base-env.default#base-env",
            ORDERS,
            INBOX,
            API_FN,
            WORKER_FN,
        ]
    );

    // The worker got the table's name, its secret only as a secret, the
    // queue's outputs on its trigger, its schedule, and both policies.
    let resolved = inputs(&s, "development").await;
    let worker = &resolved[WORKER_FN]["spec"];
    let table = "shop.development.modules.order-worker.externals.orders";
    assert_eq!(worker["function"]["variables"]["TABLE"], table);
    assert_eq!(worker["function"]["variables"]["LOG_LEVEL"], "info");
    assert_eq!(
        worker["function"]["variables"]["SIGNING_KEY"]["secret"]["parts"][0]["secret"]["ref"],
        "apps/shop/values/ORDER_SIGNING_KEY"
    );
    assert_eq!(
        worker["function"]["package"]["artifact"],
        "s3://artifacts/order-worker/0f3a.zip?versionId=v1"
    );
    assert_eq!(
        worker["triggers"]["inbox"]["queue"]["outputs"]["url"],
        "https://sqs.eu-north-1.amazonaws.com/111122223333/inbox-1"
    );
    // The module subscribes the function by the queue's ARN.
    assert_eq!(
        worker["triggers"]["inbox"]["queue"]["outputs"]["arn"],
        dev[INBOX]["outputs"]["arn"]
    );
    assert_eq!(
        worker["triggers"]["nightly"]["schedule"],
        json!({"cron": "0 3 * * *", "timezone": "Europe/Oslo", "input": {"task": "report"}})
    );
    let rendered = serde_json::to_string(&(&dev, &resolved)).unwrap();
    assert!(!rendered.contains("order-signing-key-1"), "{rendered}");
    let dependencies = &dev[WORKER_FN]["outputs"]["dependencies"];
    for policy in [ORDERS_POLICY, INBOX_POLICY] {
        assert!(
            dependencies[policy]["arn"]
                .as_str()
                .unwrap()
                .starts_with("arn:aws:iam::111122223333:policy/openporch/"),
            "{dependencies}"
        );
        assert!(
            dev[WORKER_FN]["dependencies"]
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d == policy)
        );
    }
    // Each policy names its own resource.
    assert_eq!(
        dev[ORDERS_POLICY]["outputs"]["table_arn"],
        dev[ORDERS]["outputs"]["arn"]
    );
    assert_eq!(
        dev[INBOX_POLICY]["outputs"]["queue_arn"],
        dev[INBOX]["outputs"]["arn"]
    );

    // The API runs the digest its tag pointed at, behind an explicit public
    // URL.
    let api = &resolved[API_FN]["spec"];
    assert_eq!(
        api["function"]["package"]["image"],
        format!("{ECR}/lambda-api@sha256:{}", "1".repeat(64))
    );
    assert_eq!(api["triggers"]["web"], json!({"http": {"auth": "none"}}));

    // Deploying the same set again keeps every resource's identity.
    let again = s.deploy_to("development", json!({"set_id": set_id})).await;
    assert_eq!(again["set_id"], set_id.as_str());
    let unchanged = s.active("development").await;
    for (descriptor, resource) in &dev {
        assert_eq!(unchanged[descriptor]["guresid"], resource["guresid"]);
    }

    // The configuration changes: a new queue URL and a new upload of the
    // worker's zip.
    let (st, b) = s.get("/orgs/acme/resources/defs/sqs", &s.admin).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    let mut queue_inputs = b["driver_inputs"].clone();
    queue_inputs["values"]["url"] =
        json!("https://sqs.eu-north-1.amazonaws.com/111122223333/inbox-2");
    let (st, b) = s
        .put(
            "/orgs/acme/resources/defs/sqs",
            &s.admin,
            json!({"driver_type": "openporch/echo", "driver_inputs": queue_inputs,
                   "provision": b["provision"]}),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    sts.set("v2");

    // Replaying the snapshot runs what it recorded.
    s.deploy_to(
        "development",
        json!({"deploy_id": last["id"], "replay": "snapshot"}),
    )
    .await;
    let old = inputs(&s, "development").await;
    let worker = &old[WORKER_FN]["spec"];
    assert_eq!(
        worker["triggers"]["inbox"]["queue"]["outputs"]["url"],
        "https://sqs.eu-north-1.amazonaws.com/111122223333/inbox-1"
    );
    assert_eq!(
        worker["function"]["package"]["artifact"],
        "s3://artifacts/order-worker/0f3a.zip?versionId=v1"
    );
    // With the current configuration, the new queue URL and upload.
    s.deploy_to(
        "development",
        json!({"deploy_id": last["id"], "replay": "current"}),
    )
    .await;
    let resolved = inputs(&s, "development").await;
    let worker = &resolved[WORKER_FN]["spec"];
    assert_eq!(
        worker["triggers"]["inbox"]["queue"]["outputs"]["url"],
        "https://sqs.eu-north-1.amazonaws.com/111122223333/inbox-2"
    );
    assert_eq!(
        worker["function"]["package"]["artifact"],
        "s3://artifacts/order-worker/0f3a.zip?versionId=v2"
    );
    let current = s.active("development").await;
    for (descriptor, resource) in &dev {
        assert_eq!(current[descriptor]["guresid"], resource["guresid"]);
    }
}

/// The deployments of `development`.
async fn deploys(s: &Server) -> Value {
    let (st, b) = s
        .get(&format!("{APP}/envs/development/deploys"), &s.admin)
        .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    b
}

/// Uninstalling, step one: stopping the server and starting it again later
/// changes no infrastructure. Every resource stays active and no deletion runs.
#[tokio::test]
async fn stopping_and_restarting_the_server_deletes_nothing() {
    let s = server().await;
    s.app("shop").await;
    for (id, ty) in [("env", "base-env"), ("workloads", "workload")] {
        let def = json!({"id": id, "type": ty, "driver_type": "openporch/none",
                         "driver_inputs": {}, "criteria": [{}]});
        let (st, b) = s.post("/orgs/acme/resources/defs", &s.admin, def).await;
        assert_eq!(st, StatusCode::CREATED, "{b}");
    }
    let (st, delta) = s
        .post(
            &format!("{APP}/deltas"),
            &s.admin,
            json!({"modules": {"add": {"web": {"spec": {"containers": {
                "web": {"image": "nginx:1"}}}}}}}),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{delta}");
    s.deploy_to("development", json!({"delta_id": delta["id"]}))
        .await;
    let before = s.active("development").await;
    assert_eq!(before.len(), 2, "{before:?}");
    let history = deploys(&s).await;

    // Stop the server as a signal would, then start it again on the same data.
    let Server {
        dir,
        state,
        router,
        admin,
        ..
    } = s;
    assert!(
        state
            .drain(std::time::Duration::from_secs(5))
            .await
            .is_empty()
    );
    state.store.pool().close().await;
    drop((state, router));
    let mut s = server_in(dir).await;
    s.admin = admin;

    assert_eq!(s.active("development").await, before);
    assert_eq!(deploys(&s).await, history, "no deployment ran");
    let (st, apps) = s.get("/orgs/acme/apps", &s.admin).await;
    assert_eq!(st, StatusCode::OK, "{apps}");
    assert!(apps.to_string().contains("\"shop\""), "{apps}");
}

/// An application with more environments than the server runs deployments at
/// once is still deleted: the deletions it cannot start at once wait for room.
#[tokio::test]
async fn an_application_larger_than_the_server_is_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = crate::Config::new("acme", dir.path());
    config.images.pin_other_registries = false;
    config.limits.running_deployments = 2;
    let s = super::server_with(dir, config).await;
    s.app("shop").await;
    for (id, ty) in [("env", "base-env"), ("workloads", "workload")] {
        let def = json!({"id": id, "type": ty, "driver_type": "openporch/none",
                         "driver_inputs": {}, "criteria": [{}]});
        let (st, b) = s.post("/orgs/acme/resources/defs", &s.admin, def).await;
        assert_eq!(st, StatusCode::CREATED, "{b}");
    }
    let (st, delta) = s
        .post(
            &format!("{APP}/deltas"),
            &s.admin,
            json!({"modules": {"add": {"web": {"spec": {"containers": {
                "web": {"image": "nginx:1"}}}}}}}),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{delta}");
    let envs = ["development", "staging", "qa"];
    for env in &envs[1..] {
        let (st, b) = s
            .post(
                &format!("{APP}/envs"),
                &s.admin,
                json!({"id": env, "type": "development"}),
            )
            .await;
        assert_eq!(st, StatusCode::CREATED, "{b}");
    }
    for env in envs {
        s.deploy_to(env, json!({"delta_id": delta["id"]})).await;
        assert!(!s.active(env).await.is_empty());
    }

    let body = json!({"permits": s.app_deletion_permits(APP).await});
    let delete = || {
        s.call(
            axum::http::Method::DELETE,
            APP,
            Some(&s.admin),
            Some(body.clone()),
        )
    };
    let (st, b) = delete().await;
    assert_eq!(st, StatusCode::ACCEPTED, "{b}");
    assert_eq!(b["deletions"].as_array().unwrap().len(), 3, "{b}");
    // Repeating it while the deletions run is not refused for want of room.
    // (Once an environment is gone, its permits are too, so a late repeat may
    // be refused for those instead.)
    let (st, again) = delete().await;
    assert_ne!(st, StatusCode::TOO_MANY_REQUESTS, "{again}");
    for _ in 0..3000 {
        if s.get(APP, &s.admin).await.0 == StatusCode::NOT_FOUND {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("the application was not deleted");
}

/// A web workload that uses the shared DNS name `api-dns`.
const API: &str = r#"
apiVersion: openporch.dev/v1
kind: Workload
metadata: {name: web}
containers:
  web:
    image: nginx
    variables:
      API: "https://${resources.dns.host}/api"
resources:
  dns: {type: dns, id: api-dns}
"#;

/// `resource-graph`'s `Shared DNS per environment`: `shared.api-dns`
/// deployed to two environments is a separate active resource in each, with
/// its own identity and outputs, and each environment's workload reads its
/// own.
#[tokio::test]
async fn a_shared_dns_name_is_its_own_resource_in_each_environment() {
    let s = server().await;
    s.app("shop").await;
    for (id, ty, driver, inputs) in [
        ("env", "base-env", "openporch/none", json!({})),
        ("workloads", "workload", "openporch/none", json!({})),
        (
            "dns",
            "dns",
            "openporch/echo",
            json!({"values": {"host": "api.${context.env.id}.example.com"}}),
        ),
    ] {
        let (st, b) = s
            .post(
                "/orgs/acme/resources/defs",
                &s.admin,
                json!({"id": id, "type": ty, "driver_type": driver,
                       "driver_inputs": inputs, "criteria": [{}]}),
            )
            .await;
        assert_eq!(st, StatusCode::CREATED, "{b}");
    }
    let (st, b) = s
        .post(
            &format!("{APP}/envs"),
            &s.admin,
            json!({"id": "staging", "name": "Staging", "type": "development"}),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{b}");
    let module = Workload::parse(API, None)
        .unwrap()
        .to_module(&builtin_types())
        .unwrap()
        .module;
    let (st, delta) = s
        .post(
            &format!("{APP}/deltas"),
            &s.admin,
            json!({"modules": {"add": {"web": module}}}),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{delta}");
    let first = s
        .deploy_to("development", json!({"delta_id": delta["id"]}))
        .await;
    s.deploy_to("staging", json!({"set_id": first["set_id"]}))
        .await;

    const DNS: &str = "dns.default#shared.api-dns";
    const WEB: &str = "workload.default#modules.web";
    let dev = s.active("development").await;
    let staging = s.active("staging").await;
    for (env, active) in [("development", &dev), ("staging", &staging)] {
        let dns = &active[DNS];
        assert_eq!(dns["status"], "provisioned", "{env}: {dns}");
        assert_eq!(
            dns["outputs"]["host"],
            format!("api.{env}.example.com"),
            "{env}: {dns}"
        );
        // The workload of each environment resolved its own environment's name.
        let web = active[WEB]["outputs"].to_string();
        assert!(
            web.contains(&format!("https://api.{env}.example.com/api")),
            "{env}: {web}"
        );
        let other = if env == "staging" {
            "development"
        } else {
            "staging"
        };
        assert!(
            !web.contains(&format!("api.{other}.example.com")),
            "{env}: {web}"
        );
    }
    assert_ne!(dev[DNS]["guresid"], staging[DNS]["guresid"]);
    // Two active resources, one per environment.
    let records: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM active_resources WHERE app_id = 'shop' AND descriptor = ?",
    )
    .bind(DNS)
    .fetch_one(s.state.store.pool())
    .await
    .unwrap();
    assert_eq!(records, 2);
}

/// `${shared.api-dns.host}` resolves to the shared DNS name, whether the set's
/// `shared` section or a workload dependency with that `id` requests it, when
/// the DNS name's definition references a route without an ID: the route
/// inherits the ID `shared.api-dns` but is not the shared resource.
#[tokio::test]
async fn a_dependency_inheriting_a_shared_id_is_not_the_shared_resource() {
    let by_workload = Workload::parse(API, None)
        .unwrap()
        .to_module(&builtin_types())
        .unwrap()
        .module;
    let by_set = json!({
        "modules": {"add": {"web": {"spec": {"containers": {"web": {"image": "nginx",
            "variables": {"API": "https://${shared.api-dns.host}/api"}}}}}}},
        "shared": [{"op": "add", "path": "/api-dns", "value": {"type": "dns"}}],
    });
    for delta in [by_set, json!({"modules": {"add": {"web": by_workload}}})] {
        let s = server().await;
        s.app("shop").await;
        for (id, ty, driver, inputs) in [
            ("env", "base-env", "openporch/none", json!({})),
            ("workloads", "workload", "openporch/none", json!({})),
            (
                "dns",
                "dns",
                "openporch/echo",
                json!({"values": {"host": "api-${resources.route.guresid}.example.com"}}),
            ),
            ("routes", "route", "openporch/echo", json!({})),
        ] {
            let (st, b) = s
                .post(
                    "/orgs/acme/resources/defs",
                    &s.admin,
                    json!({"id": id, "type": ty, "driver_type": driver,
                           "driver_inputs": inputs, "criteria": [{}]}),
                )
                .await;
            assert_eq!(st, StatusCode::CREATED, "{b}");
        }
        let (st, delta) = s.post(&format!("{APP}/deltas"), &s.admin, delta).await;
        assert_eq!(st, StatusCode::CREATED, "{delta}");
        s.deploy_to("development", json!({"delta_id": delta["id"]}))
            .await;
        let active = s.active("development").await;
        let route = &active["route.default#shared.api-dns"];
        let host = format!("api-{}.example.com", route["guresid"].as_str().unwrap());
        assert_eq!(
            active["dns.default#shared.api-dns"]["outputs"]["host"],
            host
        );
        let web = active["workload.default#modules.web"]["outputs"].to_string();
        assert!(web.contains(&format!("https://{host}/api")), "{web}");
    }
}
