use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use openporch_core::model::SecretRef;
use openporch_engine::planner::{
    ExecFailure, Executor, Limits, Planner, ProvisionJob, ProvisionResult,
};
use serde_json::{Value, json};

use super::*;
use crate::deploy::PlannerRunner;

/// Provisions every resource except those of the failing types.
#[derive(Debug, Default)]
struct Fake {
    failing: Mutex<BTreeSet<String>>,
}

impl Executor for Fake {
    async fn provision(&self, job: ProvisionJob) -> Result<ProvisionResult, ExecFailure> {
        let ty = job.descriptor.ty.to_string();
        if self.failing.lock().unwrap().contains(&ty) {
            return Err(ExecFailure::Permanent(format!("{ty} is unavailable")));
        }
        let mut result = ProvisionResult::default();
        if ty == "postgres" {
            result.outputs.insert("host".into(), json!("db.internal"));
            result.outputs.insert("port".into(), json!(5432));
            result.outputs.insert("name".into(), json!("shop"));
            result.secret_outputs.insert(
                "password".into(),
                SecretRef {
                    store: "openporch".into(),
                    reference: "resources/db/password".into(),
                    version: Some("1".into()),
                },
            );
        }
        Ok(result)
    }
}

const RESOURCES: &str = "/orgs/acme/apps/shop/envs/development/resources";
const DEPLOYS: &str = "/orgs/acme/apps/shop/envs/development/deploys";
const DB: &str = "postgres.default#modules.web.externals.db";

impl Server {
    /// `shop` with definitions for its environment, workloads and postgres, and
    /// deployments run by the planner over `fake`.
    async fn provisioning(&self, fake: Arc<Fake>) {
        self.app("shop").await;
        for (id, ty, driver) in [
            ("env", "base-env", "openporch/none"),
            ("workloads", "workload", "openporch/none"),
            ("pg", "postgres", "openporch/echo"),
        ] {
            let (s, b) = self
                .post(
                    "/orgs/acme/resources/defs",
                    &self.admin,
                    json!({"id": id, "type": ty, "driver_type": driver, "criteria": [{}]}),
                )
                .await;
            assert_eq!(s, StatusCode::CREATED, "{b}");
        }
        let limits = Limits {
            backoff: std::time::Duration::from_millis(1),
            max_backoff: std::time::Duration::from_millis(1),
            ..Limits::default()
        };
        let planner = Planner::new(Arc::new(self.state.store.clone()), fake, limits);
        self.state
            .runners
            .set(Arc::new(PlannerRunner::new(Arc::new(planner))));
    }

    /// Deploys `web` with image `image` and, when `db`, its database, and
    /// waits for the outcome.
    async fn deploy_web(&self, image: &str, db: bool) -> Value {
        let externals = if db {
            json!({"db": {"type": "postgres"}})
        } else {
            json!({})
        };
        let (s, env) = self
            .get("/orgs/acme/apps/shop/envs/development", &self.admin)
            .await;
        assert_eq!(s, StatusCode::OK, "{env}");
        // Replace `web` once the environment has it.
        let remove = if env["desired_set_id"].is_null() {
            json!([])
        } else {
            json!(["web"])
        };
        let (s, delta) = self
            .post(
                "/orgs/acme/apps/shop/deltas",
                &self.admin,
                json!({"modules": {"remove": remove, "add": {"web": {"externals": externals,
                    "spec": {"containers": {"web": {"image": image}}}}}}}),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED, "{delta}");
        let (s, d) = self
            .post(
                DEPLOYS,
                &self.admin,
                json!({"delta_id": delta["id"], "expected_generation": env["generation"]}),
            )
            .await;
        assert_eq!(s, StatusCode::ACCEPTED, "{d}");
        let id = d["id"].as_str().unwrap().to_owned();
        for _ in 0..500 {
            let (s, d) = self.get(&format!("{DEPLOYS}/{id}"), &self.admin).await;
            assert_eq!(s, StatusCode::OK, "{d}");
            if d["status"] != "in_progress" {
                return d;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("deployment {id} did not finish");
    }

    async fn resource(&self, descriptor: &str) -> Value {
        let (s, b) = self.get(RESOURCES, &self.admin).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        b["resources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["descriptor"] == descriptor)
            .cloned()
            .unwrap_or_else(|| panic!("{descriptor} is not listed: {b}"))
    }
}

#[tokio::test]
async fn read_outputs() {
    let s = server().await;
    s.provisioning(Arc::default()).await;
    let d = s.deploy_web("nginx:1", true).await;
    assert_eq!(d["status"], "succeeded", "{d}");

    let (status, db) = s
        .get(
            &format!("{RESOURCES}/postgres/modules.web.externals.db"),
            &s.admin,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{db}");
    assert_eq!(
        db["outputs"],
        json!({"host": "db.internal", "port": 5432, "name": "shop"})
    );
    assert_eq!(db["secret_outputs"], json!(["password"]));
    assert!(!db.to_string().contains("resources/db/password"), "{db}");
    assert_eq!(db["status"], "provisioned");
    assert_eq!(db["definition_id"], "pg");
    assert_eq!(db["ownership"], "owned");
    assert_eq!(db["usage"]["last_deploy_id"], d["id"]);
    assert_eq!(db["usage"]["deploys_ago"], 0);
    assert_eq!(db["usage"]["referenced"], true);

    // The workload depends on its database.
    let web = s.resource("workload.default#modules.web").await;
    assert_eq!(web["dependencies"], json!([DB]));

    let (status, b) = s
        .get(
            &format!("{RESOURCES}/postgres/modules.web.externals.nope"),
            &s.admin,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{b}");
    let (status, b) = s
        .get("/orgs/acme/apps/shop/envs/nope/resources", &s.admin)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{b}");
}

#[tokio::test]
async fn stable_across_deployments() {
    let s = server().await;
    s.provisioning(Arc::default()).await;
    s.deploy_web("nginx:1", true).await;
    let first = s.resource(DB).await;
    let d = s.deploy_web("nginx:2", true).await;
    assert_eq!(d["status"], "succeeded", "{d}");
    let second = s.resource(DB).await;
    assert_eq!(first["guresid"], second["guresid"]);
    assert_eq!(second["usage"]["last_deploy_id"], d["id"]);
}

#[tokio::test]
async fn usage_after_a_partial_failure() {
    let s = server().await;
    let fake = Arc::new(Fake::default());
    s.provisioning(fake.clone()).await;
    let good = s.deploy_web("nginx:1", true).await;
    assert_eq!(good["status"], "succeeded", "{good}");
    fake.failing.lock().unwrap().insert("postgres".into());
    let bad = s.deploy_web("nginx:2", true).await;
    assert_eq!(bad["status"], "failed", "{bad}");

    let db = s.resource(DB).await;
    assert_eq!(db["status"], "failed");
    assert_eq!(db["usage"]["last_deploy_id"], good["id"]);
    assert_eq!(db["usage"]["deploys_ago"], 1);
    assert_eq!(db["usage"]["referenced"], true);
    // The last successful outputs are kept.
    assert_eq!(db["outputs"]["host"], "db.internal");
}

#[tokio::test]
async fn dependency_removed_then_restored() {
    let s = server().await;
    s.provisioning(Arc::default()).await;
    s.deploy_web("nginx:1", true).await;
    let before = s.resource(DB).await;

    let d = s.deploy_web("nginx:2", false).await;
    assert_eq!(d["status"], "succeeded", "{d}");
    // Unreferenced, but still tracked and potentially billable.
    let retained = s.resource(DB).await;
    assert_eq!(retained["guresid"], before["guresid"]);
    assert_eq!(retained["status"], "provisioned");
    assert_eq!(retained["usage"]["referenced"], false);
    assert_eq!(retained["usage"]["deploys_ago"], 1);

    let d = s.deploy_web("nginx:3", true).await;
    assert_eq!(d["status"], "succeeded", "{d}");
    let restored = s.resource(DB).await;
    assert_eq!(restored["guresid"], before["guresid"]);
    assert_eq!(restored["usage"]["referenced"], true);
    assert_eq!(restored["usage"]["deploys_ago"], 0);
}

#[tokio::test]
async fn definition_switches_driver() {
    let s = server().await;
    s.provisioning(Arc::default()).await;
    s.deploy_web("nginx:1", true).await;
    let before = s.resource(DB).await;

    let (status, b) = s
        .call(
            Method::PUT,
            "/orgs/acme/resources/defs/pg",
            Some(&s.admin),
            Some(json!({"driver_type": "openporch/template",
                        "driver_inputs": {"values": {"template": "outputs: {}"}}})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{b}");
    let d = s.deploy_web("nginx:2", true).await;
    assert_eq!(d["status"], "failed", "{d}");
    let error = d["error"].as_str().unwrap();
    assert!(error.contains("migrate or replace the resource"), "{error}");

    // The resource keeps its driver and identity.
    let after = s.resource(DB).await;
    assert_eq!(after["guresid"], before["guresid"]);
    assert_eq!(after["driver_type"], "openporch/echo");
    assert_eq!(after["status"], "provisioned");
}

#[tokio::test]
async fn resources_of_a_definition() {
    let s = server().await;
    s.provisioning(Arc::default()).await;
    s.deploy_web("nginx:1", true).await;
    let (status, b) = s
        .get("/orgs/acme/resources/defs/pg/resources", &s.admin)
        .await;
    assert_eq!(status, StatusCode::OK, "{b}");
    let resources = b["resources"].as_array().unwrap();
    assert_eq!(resources.len(), 1, "{b}");
    assert_eq!(resources[0]["app"], "shop");
    assert_eq!(resources[0]["env"], "development");
    assert_eq!(resources[0]["descriptor"], DB);

    let (status, b) = s
        .get("/orgs/acme/resources/defs/nope/resources", &s.admin)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{b}");

    // Listing across applications needs organization-wide read access.
    let dev = s.member("dev", "member").await;
    s.grant_app("shop", "dev", "developer").await;
    let (status, b) = s.get("/orgs/acme/resources/defs/pg/resources", &dev).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{b}");
    let (status, b) = s.get(RESOURCES, &dev).await;
    assert_eq!(status, StatusCode::OK, "{b}");
}
