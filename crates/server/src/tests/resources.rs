use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use openporch_core::model::SecretRef;
use openporch_engine::planner::{
    DeprovisionJob, ExecFailure, Executor, Limits, Observed, Planner, ProvisionJob, ProvisionResult,
};
use serde_json::{Value, json};

use super::*;
use crate::deploy::{PlannerRunner, StorePermits};

/// Provisions every resource except those of the failing types.
#[derive(Debug, Default)]
pub(super) struct Fake {
    failing: Mutex<BTreeSet<String>>,
    /// The GUResID each job was handed, by descriptor.
    guresids: Mutex<Vec<(String, String)>>,
    /// The descriptors deleted, in order.
    pub(super) deleted: Mutex<Vec<String>>,
    /// Types whose deletion fails.
    undeletable: Mutex<BTreeSet<String>>,
    /// A secret value the executor resolves for postgres, and then leaks into
    /// its output and error.
    leak: Mutex<Option<String>>,
    /// Types whose provisioning ends with an unknown outcome.
    uncertain: Mutex<BTreeSet<String>>,
    /// What observing a resource finds.
    observation: Mutex<Option<Observed>>,
}

impl Executor for Fake {
    async fn provision(&self, job: ProvisionJob) -> Result<ProvisionResult, ExecFailure> {
        let ty = job.descriptor.ty.to_string();
        self.guresids
            .lock()
            .unwrap()
            .push((job.descriptor.to_string(), job.guresid.to_string()));
        let leak = self.leak.lock().unwrap().clone();
        if let Some(secret) = leak.filter(|_| ty == "postgres") {
            let mut log = job.log.redacting();
            log.secret([&secret]);
            log.info(&format!("psql --password={secret}"));
            return Err(log.failure(ExecFailure::Permanent(format!(
                "password authentication failed: {secret}"
            ))));
        }
        if self.failing.lock().unwrap().contains(&ty) {
            return Err(ExecFailure::Permanent(format!("{ty} is unavailable")));
        }
        if self.uncertain.lock().unwrap().contains(&ty) {
            return Err(ExecFailure::Uncertain(format!("{ty} timed out")));
        }
        let mut result = ProvisionResult::default();
        if job.driver_type == "openporch/opentofu" {
            result.backend = Some("local state".into());
        }
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

    async fn deprovision(&self, job: DeprovisionJob) -> Result<(), ExecFailure> {
        let ty = job.job.descriptor.ty.to_string();
        if self.undeletable.lock().unwrap().contains(&ty) {
            return Err(ExecFailure::Permanent(format!(
                "{ty} has deletion protection"
            )));
        }
        self.deleted
            .lock()
            .unwrap()
            .push(job.job.descriptor.to_string());
        Ok(())
    }

    async fn observe(&self, _: ProvisionJob, _: bool) -> Result<Observed, ExecFailure> {
        Ok(self
            .observation
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| Observed::Unknown("nothing to observe".into())))
    }
}

const RESOURCES: &str = "/orgs/acme/apps/shop/envs/development/resources";
const DEPLOYS: &str = "/orgs/acme/apps/shop/envs/development/deploys";
pub(super) const DB: &str = "postgres.default#modules.web.externals.db";
pub(super) const ENV_DEV: &str = "/orgs/acme/apps/shop/envs/development";

impl Server {
    /// `shop` with definitions for its environment, workloads and postgres, and
    /// deployments run by the planner over `fake`.
    pub(super) async fn provisioning(&self, fake: Arc<Fake>) {
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
        let planner = Planner::new(Arc::new(self.state.store.clone()), fake, limits)
            .with_permit_check(Arc::new(StorePermits(self.state.store.clone())));
        self.state
            .runners
            .set(Arc::new(PlannerRunner::new(Arc::new(planner))));
    }

    /// Deploys `web` with image `image` and, when `db`, its database, and
    /// waits for the outcome.
    pub(super) async fn deploy_web(&self, image: &str, db: bool) -> Value {
        self.deploy_web_with(image, db, false).await
    }

    /// [`Self::deploy_web`] with a permit for every change that needs one.
    pub(super) async fn deploy_web_with(&self, image: &str, db: bool, permitted: bool) -> Value {
        self.deploy_web_in("development", image, db, permitted)
            .await
    }

    /// [`Self::deploy_web_with`] to environment `env`.
    pub(super) async fn deploy_web_in(
        &self,
        env: &str,
        image: &str,
        db: bool,
        permitted: bool,
    ) -> Value {
        let (s, d) = self.try_deploy_web(env, image, db, permitted).await;
        assert_eq!(s, StatusCode::ACCEPTED, "{d}");
        let id = d["id"].as_str().unwrap().to_owned();
        self.finished_at(&format!("/orgs/acme/apps/shop/envs/{env}/deploys/{id}"))
            .await
    }

    /// Triggers deploying `web` with image `image` and, when `db`, its
    /// database to `env`, with a permit for every change that needs one when
    /// `permitted`.
    pub(super) async fn try_deploy_web(
        &self,
        env: &str,
        image: &str,
        db: bool,
        permitted: bool,
    ) -> (StatusCode, Value) {
        let env_path = format!("/orgs/acme/apps/shop/envs/{env}");
        let externals = if db {
            json!({"db": {"type": "postgres"}})
        } else {
            json!({})
        };
        let (s, env) = self.get(&env_path, &self.admin).await;
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
        let permits = if permitted {
            self.permits_for(&env_path, json!({"delta_id": delta["id"]}))
                .await
        } else {
            Vec::new()
        };
        self.post(
            &format!("{env_path}/deploys"),
            &self.admin,
            json!({"delta_id": delta["id"], "expected_generation": env["generation"],
                "permits": permits}),
        )
        .await
    }

    /// Previews `body` in the environment at `env_path` and issues a permit,
    /// as an administrator, for every change the preview says needs one.
    pub(super) async fn permits_for(&self, env_path: &str, mut body: Value) -> Vec<String> {
        let (s, env) = self.get(env_path, &self.admin).await;
        assert_eq!(s, StatusCode::OK, "{env}");
        body["expected_generation"] = env["generation"].clone();
        let (s, plan) = self
            .post(&format!("{env_path}/plans"), &self.admin, body)
            .await;
        assert_eq!(s, StatusCode::CREATED, "{plan}");
        let asks: Vec<Value> = plan["resources"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["permit_required"] == true)
            .map(|r| json!({"descriptor": r["descriptor"], "action": r["action"]}))
            .collect();
        if asks.is_empty() {
            return Vec::new();
        }
        let (s, issued) = self
            .post(
                &format!("{env_path}/plans/{}/permits", plan["id"].as_str().unwrap()),
                &self.admin,
                json!({"permits": asks}),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED, "{issued}");
        issued["permits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["id"].as_str().unwrap().to_owned())
            .collect()
    }

    pub(super) async fn resource(&self, descriptor: &str) -> Value {
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
    let fake = Arc::new(Fake::default());
    s.provisioning(fake.clone()).await;
    s.deploy_web("nginx:1", true).await;
    let first = s.resource(DB).await;
    let d = s.deploy_web("nginx:2", true).await;
    assert_eq!(d["status"], "succeeded", "{d}");
    let second = s.resource(DB).await;
    assert_eq!(first["guresid"], second["guresid"]);
    assert_eq!(second["usage"]["last_deploy_id"], d["id"]);
    // The driver got the same identity both times.
    let handed: Vec<String> = fake
        .guresids
        .lock()
        .unwrap()
        .iter()
        .filter(|(d, _)| d == DB)
        .map(|(_, g)| g.clone())
        .collect();
    assert_eq!(handed.len(), 2);
    assert!(handed.iter().all(|g| *g == first["guresid"]));
}

#[tokio::test]
async fn usage_after_a_partial_failure() {
    let s = server().await;
    let fake = Arc::new(Fake::default());
    s.provisioning(fake.clone()).await;
    let good = s.deploy_web("nginx:1", true).await;
    assert_eq!(good["status"], "succeeded", "{good}");
    // A new definition version fails to provision.
    let (status, b) = s
        .call(
            Method::PUT,
            "/orgs/acme/resources/defs/pg",
            Some(&s.admin),
            Some(json!({"name": "Postgres v2", "driver_type": "openporch/echo"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{b}");
    fake.failing.lock().unwrap().insert("postgres".into());
    let bad = s.deploy_web("nginx:2", true).await;
    assert_eq!(bad["status"], "failed", "{bad}");

    let db = s.resource(DB).await;
    assert_eq!(db["status"], "failed");
    // It still describes the last success.
    assert_eq!(db["definition_version"], 1);
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
    // Moving it is a migration, refused before anything is accepted.
    let generation = s.get(ENV_DEV, &s.admin).await.1["generation"].clone();
    let (status, b) = s
        .try_deploy_web("development", "nginx:2", true, false)
        .await;
    assert_error(status, &b, StatusCode::CONFLICT, "DPL-002");
    let error = b["message"].as_str().unwrap();
    assert!(error.contains("migrate or replace the resource"), "{error}");
    assert_eq!(s.get(ENV_DEV, &s.admin).await.1["generation"], generation);
    // A preview says the same.
    let (status, b) = s
        .post(
            &format!("{ENV_DEV}/plans"),
            &s.admin,
            json!({"set_id": s.get(ENV_DEV, &s.admin).await.1["desired_set_id"],
                   "expected_generation": generation}),
        )
        .await;
    assert_error(status, &b, StatusCode::CONFLICT, "DPL-002");

    // The resource keeps its driver and identity.
    let after = s.resource(DB).await;
    assert_eq!(after["guresid"], before["guresid"]);
    assert_eq!(after["driver_type"], "openporch/echo");
    assert_eq!(after["status"], "provisioned");
}

#[tokio::test]
async fn a_resource_that_never_provisioned_can_switch_driver() {
    let s = server().await;
    let fake = Arc::new(Fake::default());
    s.provisioning(fake.clone()).await;
    fake.failing.lock().unwrap().insert("postgres".into());
    let d = s.deploy_web("nginx:1", true).await;
    assert_eq!(d["status"], "failed", "{d}");
    let failed = s.resource(DB).await;

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
    fake.failing.lock().unwrap().clear();
    let d = s.deploy_web("nginx:2", true).await;
    assert_eq!(d["status"], "succeeded", "{d}");
    let db = s.resource(DB).await;
    assert_eq!(db["guresid"], failed["guresid"]);
    assert_eq!(db["driver_type"], "openporch/template");
    assert_eq!(db["definition_version"], 2);
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

#[tokio::test]
async fn an_account_holding_resources_is_not_deleted() {
    let s = server().await;
    s.provisioning(Arc::default()).await;
    s.static_account("aws-prod").await;
    let def = |account: Value| json!({"driver_type": "openporch/echo", "driver_account": account});
    let (status, b) = s
        .call(
            Method::PUT,
            "/orgs/acme/resources/defs/pg",
            Some(&s.admin),
            Some(def(json!("aws-prod"))),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{b}");
    let d = s.deploy_web("nginx:1", true).await;
    assert_eq!(d["status"], "succeeded", "{d}");
    assert_eq!(s.resource(DB).await["driver_account"], "aws-prod");

    // The definition no longer names the account, but the database still
    // lives in it.
    let (status, b) = s
        .call(
            Method::PUT,
            "/orgs/acme/resources/defs/pg",
            Some(&s.admin),
            Some(def(Value::Null)),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{b}");
    let (status, b) = s
        .delete("/orgs/acme/resources/accounts/aws-prod", &s.admin)
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{b}");
    assert!(b["message"].as_str().unwrap().contains(DB), "{b}");
}

#[tokio::test]
async fn scheduled_deletion() {
    let s = server().await;
    let fake = Arc::new(Fake::default());
    s.provisioning(fake.clone()).await;
    s.deploy_web("nginx:1", true).await;
    let path = format!("{RESOURCES}/postgres/modules.web.externals.db");

    // Viewers may not schedule it, and members without a role do not see it.
    let viewer = s.member("vic", "member").await;
    let (status, b) = s
        .post(
            "/orgs/acme/apps/shop/users",
            &s.admin,
            json!({"id": "vic", "role": "viewer"}),
        )
        .await;
    assert!(status.is_success(), "{b}");
    let (status, b) = s.delete(&path, &viewer).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{b}");
    let stranger = s.member("sam", "member").await;
    let (status, b) = s.delete(&path, &stranger).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{b}");
    let (status, b) = s
        .delete(
            &format!("{RESOURCES}/postgres/modules.web.externals.nope"),
            &s.admin,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{b}");

    let (status, db) = s.delete(&path, &s.admin).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{db}");
    assert_eq!(db["scheduled_deletion"], true);
    assert_eq!(db["status"], "provisioned");

    // A deployment that still includes it keeps it.
    let d = s.deploy_web("nginx:2", true).await;
    assert_eq!(d["status"], "succeeded", "{d}");
    assert_eq!(s.resource(DB).await["scheduled_deletion"], true);

    // Leaving it out without a permit is refused, and the database stays.
    let (status, b) = s
        .try_deploy_web("development", "nginx:3", false, false)
        .await;
    assert_error(status, &b, StatusCode::CONFLICT, "DPL-001");
    assert!(b["message"].as_str().unwrap().contains(DB), "{b}");
    assert_eq!(s.resource(DB).await["status"], "provisioned");
    assert!(fake.deleted.lock().unwrap().is_empty());

    // With a permit, the driver deletes it and the record disappears.
    let d = s.deploy_web_with("nginx:4", false, true).await;
    assert_eq!(d["status"], "succeeded", "{d}");
    assert_eq!(*fake.deleted.lock().unwrap(), vec![DB.to_owned()]);
    let (status, b) = s.get(&path, &s.admin).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{b}");
    let (status, b) = s.delete(&path, &s.admin).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{b}");
    let (_, audit) = s.get("/orgs/acme/audit-logs?method=DELETE", &s.admin).await;
    assert!(
        audit.to_string().contains("resource.schedule_deletion"),
        "{audit}"
    );
}

#[tokio::test]
async fn detach() {
    let s = server().await;
    let fake = Arc::new(Fake::default());
    s.provisioning(fake.clone()).await;
    s.deploy_web("nginx:1", true).await;
    let db = s.resource(DB).await;
    let path = format!("{RESOURCES}/postgres/modules.web.externals.db");
    let detach = format!("{path}/detach");
    let body = |reason: &str, permits: &[String]| json!({"reason": reason, "permits": permits});
    let permits = s
        .permits_for(ENV_DEV, json!({"kind": "detach", "descriptor": DB}))
        .await;
    assert_eq!(permits.len(), 1);

    // Only administrators detach.
    let developer = s.member("dev", "member").await;
    let (status, b) = s
        .post(
            "/orgs/acme/apps/shop/users",
            &s.admin,
            json!({"id": "dev", "role": "developer"}),
        )
        .await;
    assert!(status.is_success(), "{b}");
    let (status, b) = s
        .post(&detach, &developer, body("moving it", &permits))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{b}");
    // A reason and a matching permit are required.
    let (status, b) = s.post(&detach, &s.admin, body("  ", &permits)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{b}");
    let (status, b) = s.post(&detach, &s.admin, body("moving it", &[])).await;
    assert_error(status, &b, StatusCode::CONFLICT, "DPL-001");
    assert_eq!(s.resource(DB).await["guresid"], db["guresid"]);

    let (status, tombstone) = s
        .post(
            &detach,
            &s.admin,
            body("moving it to the shared cluster", &permits),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{tombstone}");
    assert_eq!(tombstone["descriptor"], DB);
    assert_eq!(tombstone["guresid"], db["guresid"]);
    assert_eq!(tombstone["reason"], "detached");
    assert_eq!(
        tombstone["body"]["reason"],
        "moving it to the shared cluster"
    );
    assert_eq!(tombstone["body"]["outputs"]["host"], "db.internal");
    assert!(
        !tombstone.to_string().contains("resources/db/password"),
        "{tombstone}"
    );

    // The record is gone and the driver was never asked to delete anything.
    let (status, b) = s.get(&path, &s.admin).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{b}");
    assert!(fake.deleted.lock().unwrap().is_empty());
    let (status, list) = s
        .get("/orgs/acme/apps/shop/envs/development/tombstones", &s.admin)
        .await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list["tombstones"][0]["guresid"], db["guresid"]);
    let (_, audit) = s.get("/orgs/acme/audit-logs?method=POST", &s.admin).await;
    assert!(
        audit
            .to_string()
            .contains("moving it to the shared cluster"),
        "{audit}"
    );

    // A deployment that still includes it does not silently recreate it.
    let d = s.deploy_web("nginx:2", true).await;
    assert_eq!(d["status"], "failed", "{d}");
    assert!(d["error"].as_str().unwrap().contains("detached"), "{d}");
    let created: Vec<String> = fake
        .guresids
        .lock()
        .unwrap()
        .iter()
        .filter(|(d, _)| d == DB)
        .map(|(_, g)| g.clone())
        .collect();
    assert_eq!(created.len(), 1);
}

#[tokio::test]
async fn inspect_the_graph_after_a_failed_deployment() {
    let s = server().await;
    let graph = format!("{RESOURCES}/graph");
    let fake = Arc::new(Fake::default());
    s.provisioning(fake.clone()).await;
    let (status, b) = s.get(&graph, &s.admin).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{b}");

    fake.failing.lock().unwrap().insert("postgres".into());
    let d = s.deploy_web("nginx:1", true).await;
    assert_eq!(d["status"], "failed", "{d}");

    let (status, g) = s.get(&graph, &s.admin).await;
    assert_eq!(status, StatusCode::OK, "{g}");
    assert_eq!(g["deployment_id"], d["id"]);
    assert_eq!(g["status"], "failed");
    let nodes: std::collections::BTreeMap<String, Value> = g["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| (n["descriptor"].as_str().unwrap().to_owned(), n.clone()))
        .collect();
    // Every node shows its status, and the failed one its error.
    let statuses: std::collections::BTreeMap<&str, &str> = nodes
        .iter()
        .map(|(k, n)| (k.as_str(), n["status"].as_str().unwrap()))
        .collect();
    assert_eq!(
        statuses,
        std::collections::BTreeMap::from([
            ("base-env.default#base-env", "provisioned"),
            (DB, "failed"),
            ("workload.default#modules.web", "pending"),
        ])
    );
    assert!(nodes["base-env.default#base-env"]["error"].is_null(), "{g}");
    assert_eq!(nodes[DB]["status"], "failed", "{g}");
    assert_eq!(nodes[DB]["definition_id"], "pg");
    assert!(
        nodes[DB]["error"]
            .as_str()
            .unwrap()
            .contains("postgres is unavailable"),
        "{g}"
    );
    assert_eq!(
        nodes["workload.default#modules.web"]["status"], "pending",
        "{g}"
    );
    assert!(
        g["edges"]
            .as_array()
            .unwrap()
            .contains(&json!({"from": "workload.default#modules.web", "to": DB})),
        "{g}"
    );

    // Someone who cannot see the application cannot see that it exists.
    let stranger = s.member("sam", "member").await;
    let (status, b) = s.get(&graph, &stranger).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{b}");
}

#[tokio::test]
async fn read_a_deployments_redacted_log() {
    const SECRET: &str = "hunter2-leaked-91";
    let s = server().await;
    let fake = Arc::new(Fake::default());
    s.provisioning(fake.clone()).await;
    *fake.leak.lock().unwrap() = Some(SECRET.into());
    let d = s.deploy_web("nginx:1", true).await;
    assert_eq!(d["status"], "failed", "{d}");
    let logs = format!("{DEPLOYS}/{}/logs", d["id"].as_str().unwrap());

    let (status, page) = s.get(&logs, &s.admin).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(!page.to_string().contains(SECRET), "{page}");
    assert_eq!(page["finished"], true, "{page}");
    let lines = page["lines"].as_array().unwrap();
    let of_db: Vec<_> = lines.iter().filter(|l| l["descriptor"] == DB).collect();
    let messages: Vec<_> = of_db
        .iter()
        .map(|l| l["message"].as_str().unwrap())
        .collect();
    assert!(messages.contains(&"psql --password=[REDACTED]"), "{page}");
    assert!(
        messages.contains(&"failed, nothing changed; fix the cause and deploy again: password authentication failed: [REDACTED]"),
        "{page}"
    );
    assert!(
        of_db
            .iter()
            .all(|l| l["guresid"].is_string() && l["operation_id"].is_string())
    );
    assert!(
        lines.last().unwrap()["message"]
            .as_str()
            .unwrap()
            .starts_with("deployment failed"),
        "{page}"
    );
    // The resource's error is redacted the same way.
    let (_, g) = s.get(&format!("{RESOURCES}/graph"), &s.admin).await;
    assert!(!g.to_string().contains(SECRET), "{g}");

    // Pages follow each other through `next`.
    let mut seen = Vec::new();
    let mut after = 0;
    loop {
        let (status, page) = s
            .get(&format!("{logs}?after={after}&limit=2"), &s.admin)
            .await;
        assert_eq!(status, StatusCode::OK, "{page}");
        let got = page["lines"].as_array().unwrap();
        assert!(got.len() <= 2);
        seen.extend(got.iter().cloned());
        after = page["next"].as_i64().unwrap();
        if page["finished"] == true {
            break;
        }
    }
    assert_eq!(&seen, lines);
    // Reading past the end returns nothing and keeps the position.
    let (_, end) = s.get(&format!("{logs}?after={after}"), &s.admin).await;
    assert_eq!(end, json!({"lines": [], "next": after, "finished": true}));

    // The same log by relative reference.
    let (status, latest) = s.get(&format!("{DEPLOYS}/+0/logs"), &s.admin).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(latest["lines"], page["lines"]);

    // A page that ends exactly at the last line is the final one.
    let n = lines.len();
    let (_, exact) = s.get(&format!("{logs}?after=0&limit={n}"), &s.admin).await;
    assert_eq!(exact["finished"], true, "{exact}");
    let (_, short) = s
        .get(&format!("{logs}?after=0&limit={}", n - 1), &s.admin)
        .await;
    assert_eq!(short["finished"], false, "{short}");

    let (status, b) = s.get(&format!("{logs}?limit=0"), &s.admin).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{b}");
    let (status, b) = s.get(&format!("{DEPLOYS}/nope/logs"), &s.admin).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{b}");
    // Someone who cannot see the application cannot see that it exists.
    let stranger = s.member("sam", "member").await;
    let (status, b) = s.get(&logs, &stranger).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{b}");
}

#[tokio::test]
async fn delete_a_deployed_environment() {
    const ENV: &str = "/orgs/acme/apps/shop/envs/pr-42";
    let s = server().await;
    let fake = Arc::new(Fake::default());
    s.provisioning(fake.clone()).await;
    let (status, b) = s
        .post(
            "/orgs/acme/apps/shop/envs",
            &s.admin,
            json!({"id": "pr-42", "name": "PR 42", "type": "development"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{b}");
    let d = s.deploy_web_in("pr-42", "nginx:1", true, false).await;
    assert_eq!(d["status"], "succeeded", "{d}");
    fake.undeletable.lock().unwrap().insert("postgres".into());

    // A deployed environment needs its generation and permits.
    let (status, b) = s.delete(ENV, &s.admin).await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
    let (_, env) = s.get(ENV, &s.admin).await;
    let (status, b) = s
        .call(
            Method::DELETE,
            "/orgs/acme/apps/shop/envs/development",
            Some(&s.admin),
            Some(json!({"expected_generation": 0})),
        )
        .await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");

    // The database cannot be deleted, so the environment stays with it.
    let deletion = s.delete_env(ENV, env["generation"].as_i64().unwrap()).await;
    assert_eq!(deletion["retained"], json!([]), "{deletion}");
    assert_eq!(deletion["deployment"]["kind"], "delete_env");
    let id = deletion["deployment"]["id"].as_str().unwrap();
    let d = s.finished_at(&format!("{ENV}/deploys/{id}")).await;
    assert_eq!(d["status"], "failed", "{d}");
    assert!(d["error"].as_str().unwrap().contains(DB), "{d}");
    let (_, env) = s.get(ENV, &s.admin).await;
    assert_eq!(env["state"], "deletion_failed", "{env}");
    assert_eq!(env["latest_deployment"]["kind"], "delete_env", "{env}");
    let (status, graph) = s.get(&format!("{ENV}/resources/graph"), &s.admin).await;
    assert_eq!(status, StatusCode::OK, "{graph}");
    let node = |descriptor: &str| {
        graph["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["descriptor"] == descriptor)
            .cloned()
            .unwrap_or_else(|| panic!("{descriptor}: {graph}"))
    };
    assert_eq!(node(DB)["status"], "failed", "{graph}");
    assert!(
        node(DB)["error"]
            .as_str()
            .unwrap()
            .contains("deletion protection")
    );
    assert_eq!(node("workload.default#modules.web")["status"], "deleted");
    let (_, resources) = s.get(&format!("{ENV}/resources"), &s.admin).await;
    let db = resources["resources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["descriptor"] == DB)
        .cloned()
        .unwrap();
    assert_eq!(db["status"], "deletion_failed");
    // It is not deployed again while the deletion waits for a retry.
    let (status, b) = s
        .post(
            &format!("{ENV}/deploys"),
            &s.admin,
            json!({"deploy_id": "+1", "expected_generation": env["generation"]}),
        )
        .await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");

    // The retry deletes the rest and then the environment.
    fake.undeletable.lock().unwrap().clear();
    let permits = s.permits_for(ENV, json!({"kind": "delete_env"})).await;
    let body = json!({"expected_generation": env["generation"], "permits": permits});
    let retry = || {
        s.call_with(
            Method::DELETE,
            ENV,
            Some(&s.admin),
            Some(body.clone()),
            &[("idempotency-key", "drop-pr-42")],
        )
    };
    let (status, first) = retry().await;
    assert_eq!(status, StatusCode::ACCEPTED, "{first}");
    // A retry while it runs returns the same deletion.
    let (status, again) = retry().await;
    if status == StatusCode::ACCEPTED {
        assert_eq!(again["deployment"]["id"], first["deployment"]["id"]);
    } else {
        assert_error(status, &again, StatusCode::NOT_FOUND, "API-001");
    }
    let mut gone = false;
    for _ in 0..500 {
        if s.get(ENV, &s.admin).await.0 == StatusCode::NOT_FOUND {
            gone = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(gone, "the environment was not deleted");
    let deleted = fake.deleted.lock().unwrap().clone();
    let at = |d: &str| deleted.iter().position(|x| x == d).unwrap();
    assert!(at("workload.default#modules.web") < at(DB), "{deleted:?}");
    // Once the environment is gone, a retry finds nothing to delete.
    let (status, b) = retry().await;
    assert_error(status, &b, StatusCode::NOT_FOUND, "API-001");
}

#[tokio::test]
async fn delete_a_deployed_application() {
    const APP: &str = "/orgs/acme/apps/shop";
    let s = server().await;
    let fake = Arc::new(Fake::default());
    s.provisioning(fake.clone()).await;
    let (status, b) = s
        .post(
            &format!("{APP}/envs"),
            &s.admin,
            json!({"id": "pr-42", "name": "PR 42", "type": "development"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{b}");
    let d = s.deploy_web_in("pr-42", "nginx:1", true, false).await;
    assert_eq!(d["status"], "succeeded", "{d}");
    fake.undeletable.lock().unwrap().insert("postgres".into());

    // Every environment is deleted; the empty `development` goes at once.
    let delete = || async {
        let permits = s.app_deletion_permits(APP).await;
        s.call(
            Method::DELETE,
            APP,
            Some(&s.admin),
            Some(json!({"permits": permits})),
        )
        .await
    };
    let (status, deletion) = delete().await;
    assert_eq!(status, StatusCode::ACCEPTED, "{deletion}");
    assert_eq!(deletion["removed"], false, "{deletion}");
    let envs: Vec<_> = deletion["deletions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["deployment"]["env"].as_str().unwrap())
        .collect();
    assert_eq!(envs, ["pr-42", "development"], "{deletion}");
    assert_eq!(
        deletion["deletions"][1]["deployment"]["status"],
        "succeeded"
    );
    let (_, app) = s.get(APP, &s.admin).await;
    assert_eq!(app["state"], "deleting", "{app}");
    let (status, b) = s
        .post(
            &format!("{APP}/envs"),
            &s.admin,
            json!({"id": "pr-43", "name": "PR 43", "type": "development"}),
        )
        .await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");

    // The database cannot be deleted, so its environment and the application
    // stay.
    let id = deletion["deletions"][0]["deployment"]["id"]
        .as_str()
        .unwrap();
    let d = s
        .finished_at(&format!("{APP}/envs/pr-42/deploys/{id}"))
        .await;
    assert_eq!(d["status"], "failed", "{d}");
    let (_, env) = s.get(&format!("{APP}/envs/pr-42"), &s.admin).await;
    assert_eq!(env["state"], "deletion_failed", "{env}");
    assert_eq!(
        s.get(&format!("{APP}/envs/development"), &s.admin).await.0,
        StatusCode::NOT_FOUND
    );

    // Repeating the request retries it, and then the application goes.
    fake.undeletable.lock().unwrap().clear();
    let (status, retry) = delete().await;
    assert_eq!(status, StatusCode::ACCEPTED, "{retry}");
    assert_eq!(retry["deletions"].as_array().unwrap().len(), 1, "{retry}");
    let mut gone = false;
    for _ in 0..500 {
        if s.get(APP, &s.admin).await.0 == StatusCode::NOT_FOUND {
            gone = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(gone, "the application was not deleted");
    let deleted = fake.deleted.lock().unwrap().clone();
    let at = |d: &str| deleted.iter().position(|x| x == d).unwrap();
    assert!(at("workload.default#modules.web") < at(DB), "{deleted:?}");
}

#[tokio::test]
async fn a_resource_secret_output_is_used_only_in_its_environment() {
    use openporch_store::{SecretOwner, SecretUseStore, StoreError};
    let s = server().await;
    let fake = Arc::new(Fake::default());
    s.provisioning(fake.clone()).await;
    let d = s.deploy_web_in("development", "nginx:1", true, false).await;
    assert_eq!(d["status"], "succeeded", "{d}");
    let (status, b) = s
        .post(
            "/orgs/acme/apps/shop/envs",
            &s.admin,
            json!({"id": "pr-42", "name": "PR 42", "type": "development"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{b}");
    let (_, resources) = s
        .get("/orgs/acme/apps/shop/envs/development/resources", &s.admin)
        .await;
    let guresid = resources["resources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["descriptor"] == DB)
        .unwrap()["guresid"]
        .as_str()
        .unwrap()
        .to_owned();
    let output = |reference: String| SecretRef {
        store: "openporch".into(),
        reference,
        version: None,
    };
    let own = output(format!("resources/{guresid}/outputs/password"));
    let authorize = |env: &'static str, r: SecretRef| {
        let store = s.state.store.clone();
        let guresid = guresid.clone();
        async move {
            store
                .authorize_secret_use("acme", "shop", env, SecretOwner::Resource(&guresid), &r)
                .await
        }
    };
    authorize("development", own.clone()).await.unwrap();
    // Not in another environment, and not another resource's secret.
    for (env, r) in [
        ("pr-42", own),
        (
            "development",
            output("resources/0000/outputs/password".into()),
        ),
    ] {
        let e = authorize(env, r).await.unwrap_err();
        assert!(matches!(e, StoreError::Forbidden(_)), "{e}");
    }
}

impl Server {
    /// Permits for deleting every environment of the application at `app`.
    async fn app_deletion_permits(&self, app: &str) -> Vec<String> {
        let (status, envs) = self.get(&format!("{app}/envs"), &self.admin).await;
        assert_eq!(status, StatusCode::OK, "{envs}");
        let mut permits = Vec::new();
        for env in envs.as_array().unwrap() {
            let path = format!("{app}/envs/{}", env["id"].as_str().unwrap());
            permits.extend(self.permits_for(&path, json!({"kind": "delete_env"})).await);
        }
        permits
    }

    /// Starts deleting `env` with permits for everything it deletes.
    async fn delete_env(&self, env: &str, generation: i64) -> Value {
        let permits = self.permits_for(env, json!({"kind": "delete_env"})).await;
        let (status, b) = self
            .call(
                Method::DELETE,
                env,
                Some(&self.admin),
                Some(json!({"expected_generation": generation, "permits": permits})),
            )
            .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{b}");
        b
    }

    /// The deployment at `path` once it finished.
    pub(super) async fn finished_at(&self, path: &str) -> Value {
        for _ in 0..500 {
            let (status, d) = self.get(path, &self.admin).await;
            assert_eq!(status, StatusCode::OK, "{d}");
            if d["status"] != "in_progress" {
                return d;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("{path} did not finish");
    }
}

#[tokio::test]
async fn an_unknown_outcome_is_recovered_or_resolved() {
    let s = server().await;
    let fake = Arc::new(Fake::default());
    s.provisioning(fake.clone()).await;
    fake.uncertain.lock().unwrap().insert("postgres".into());
    let d = s.deploy_web("nginx:1", true).await;
    assert_eq!(d["status"], "failed", "{d}");
    let db = s.resource(DB).await;
    assert_eq!(db["status"], "unknown", "{db}");
    let path = format!("{RESOURCES}/postgres/modules.web.externals.db");
    let (recover, resolve) = (format!("{path}/recover"), format!("{path}/resolve"));

    // Nothing changes it until its outcome is known.
    let (status, b) = s
        .try_deploy_web("development", "nginx:2", true, false)
        .await;
    assert_error(status, &b, StatusCode::CONFLICT, "DPL-004");

    // The driver cannot tell, so it waits for an administrator.
    let (status, b) = s.post(&recover, &s.admin, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{b}");
    assert_eq!(b["outcome"], "recovery_required", "{b}");
    assert!(
        b["reason"].as_str().unwrap().contains("nothing to observe"),
        "{b}"
    );
    assert_eq!(b["resource"]["status"], "recovery_required", "{b}");

    // Only an administrator resolves it, with a reason.
    let developer = s.member("dev", "member").await;
    let (status, b) = s
        .post(
            "/orgs/acme/apps/shop/users",
            &s.admin,
            json!({"id": "dev", "role": "developer"}),
        )
        .await;
    assert!(status.is_success(), "{b}");
    let (status, b) = s
        .post(&resolve, &developer, json!({"reason": "checked"}))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{b}");
    let (status, b) = s.post(&resolve, &s.admin, json!({"reason": " "})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{b}");
    let (status, b) = s
        .post(
            &resolve,
            &s.admin,
            json!({"reason": "no database exists in the console"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "failed", "{b}");
    assert_eq!(b["guresid"], db["guresid"], "{b}");
    let (status, b) = s.post(&resolve, &s.admin, json!({"reason": "again"})).await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");
    let (_, audit) = s.get("/orgs/acme/audit-logs?method=POST", &s.admin).await;
    assert!(
        audit
            .to_string()
            .contains("no database exists in the console"),
        "{audit}"
    );

    // The next deployment repeats it with the same identity.
    fake.uncertain.lock().unwrap().clear();
    let d = s.deploy_web("nginx:2", true).await;
    assert_eq!(d["status"], "succeeded", "{d}");
    assert_eq!(s.resource(DB).await["guresid"], db["guresid"]);

    // Observation settles an outcome by itself when it can.
    fake.uncertain.lock().unwrap().insert("postgres".into());
    let d = s.deploy_web("nginx:3", true).await;
    assert_eq!(d["status"], "failed", "{d}");
    *fake.observation.lock().unwrap() = Some(Observed::InProgress);
    let (status, b) = s.post(&recover, &s.admin, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{b}");
    assert_eq!(b["outcome"], "in_progress", "{b}");
    assert_eq!(b["resource"]["status"], "unknown", "{b}");
    *fake.observation.lock().unwrap() = Some(Observed::Absent { repeatable: true });
    let (status, b) = s.post(&recover, &s.admin, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{b}");
    assert_eq!(b["outcome"], "repeatable", "{b}");
    assert_eq!(b["resource"]["status"], "failed", "{b}");
    let (status, b) = s.post(&recover, &s.admin, json!({})).await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");
    fake.uncertain.lock().unwrap().clear();
    let d = s.deploy_web("nginx:4", true).await;
    assert_eq!(d["status"], "succeeded", "{d}");
    assert_eq!(s.resource(DB).await["guresid"], db["guresid"]);
}

#[tokio::test]
async fn conflicting_shared_parameters_provision_nothing() {
    let s = server().await;
    let fake = Arc::new(Fake::default());
    s.provisioning(fake.clone()).await;
    let (st, b) = s
        .post(
            "/orgs/acme/resources/defs",
            &s.admin,
            json!({"id": "ddb", "type": "dynamodb-table", "driver_type": "openporch/echo",
                   "criteria": [{}]}),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{b}");
    let module = |key: &str| {
        json!({"externals": {"orders": {"type": "dynamodb-table", "id": "orders",
                "params": {"hash_key": key}}},
               "spec": {"containers": {"web": {"image": "nginx"}}}})
    };
    let s = &s;
    let deploy = |alpha: &str, beta: &str| {
        let body = json!({"modules": {"add": {"alpha": module(alpha), "beta": module(beta)}}});
        async move {
            let (st, delta) = s.post("/orgs/acme/apps/shop/deltas", &s.admin, body).await;
            assert_eq!(st, StatusCode::CREATED, "{delta}");
            let (_, env) = s.get(ENV_DEV, &s.admin).await;
            s.post(
                DEPLOYS,
                &s.admin,
                json!({"delta_id": delta["id"], "expected_generation": env["generation"]}),
            )
            .await
        }
    };

    // Two workloads that disagree on the shared table's key are refused,
    // naming both, before anything is provisioned.
    let (st, b) = deploy("id", "order_id").await;
    assert_error(st, &b, StatusCode::BAD_REQUEST, "API-003");
    let message = b["message"].as_str().unwrap();
    for path in [
        "dynamodb-table.default#shared.orders",
        "modules.alpha.externals.orders",
        "modules.beta.externals.orders",
    ] {
        assert!(message.contains(path), "{message}");
    }
    assert!(fake.guresids.lock().unwrap().is_empty());
    let (_, list) = s.get(DEPLOYS, &s.admin).await;
    assert_eq!(list["deployments"], json!([]), "{list}");

    // Agreeing on it, they share one table.
    let (st, d) = deploy("id", "id").await;
    assert_eq!(st, StatusCode::ACCEPTED, "{d}");
    let done = s
        .finished_at(&format!("{DEPLOYS}/{}", d["id"].as_str().unwrap()))
        .await;
    assert_eq!(done["status"], "succeeded", "{done}");
    let tables: Vec<String> = fake
        .guresids
        .lock()
        .unwrap()
        .iter()
        .filter(|(d, _)| d == "dynamodb-table.default#shared.orders")
        .map(|(_, g)| g.clone())
        .collect();
    assert_eq!(tables.len(), 1, "{tables:?}");
}
