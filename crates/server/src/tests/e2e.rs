//! End to end in one process: the started server's runner deploys a function
//! workload with a database and a queue trigger through the built-in drivers,
//! redeploys it unchanged, and replays a recorded snapshot after the
//! configuration changed.

use axum::http::StatusCode;
use openporch_core::catalog::builtin_types;
use openporch_core::workload::Workload;
use openporch_store::Store;
use serde_json::{Value, json};

use super::{Server, server};

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

/// The worker's resolved variables, as its `openporch/none` definition stored
/// them.
fn variables(active: &std::collections::BTreeMap<String, Value>) -> Value {
    active[WORKLOAD]["outputs"]["spec"]["function"]["variables"].clone()
}

#[tokio::test]
async fn a_function_with_a_database_and_a_queue_deploys_and_replays() {
    let s = server().await;
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
