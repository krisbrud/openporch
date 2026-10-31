use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use openporch_core::model::SecretRef;
use openporch_engine::planner::{
    DeprovisionJob, ExecFailure, Executor, Limits, Observation, Observed, Planner, ProvisionJob,
    ProvisionResult,
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
    /// How many lines provisioning writes to the log first.
    chatter: Mutex<usize>,
    /// When set, each deletion waits for a permit from it.
    held_deletes: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
}

impl Executor for Fake {
    async fn provision(&self, job: ProvisionJob) -> Result<ProvisionResult, ExecFailure> {
        let ty = job.descriptor.ty.to_string();
        self.guresids
            .lock()
            .unwrap()
            .push((job.descriptor.to_string(), job.guresid.to_string()));
        let chatter = *self.chatter.lock().unwrap();
        if chatter > 0 {
            let log = job.log.redacting();
            for i in 0..chatter {
                log.info(&format!("{ty} line {i}"));
            }
        }
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
            return Err(ExecFailure::uncertain(format!("{ty} timed out")));
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
        let held = self.held_deletes.lock().unwrap().clone();
        if let Some(gate) = held
            && let Ok(permit) = gate.acquire().await
        {
            permit.forget();
        }
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

    async fn observe(&self, _: ProvisionJob, _: bool) -> Observation {
        self.observation
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| Observed::Unknown("nothing to observe".into()))
            .into()
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
    // Neither the application nor an environment being deleted can be renamed.
    for path in [APP.to_owned(), format!("{APP}/envs/pr-42")] {
        let (status, b) = s
            .call(
                Method::PATCH,
                &path,
                Some(&s.admin),
                Some(json!({"name": "x"})),
            )
            .await;
        assert_error(status, &b, StatusCode::CONFLICT, "API-004");
    }

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
    pub(super) async fn app_deletion_permits(&self, app: &str) -> Vec<String> {
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
    // It names the operation whose outcome is unknown.
    assert!(
        db["unknown_operation"]
            .as_str()
            .is_some_and(|o| !o.is_empty()),
        "{db}"
    );
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

/// Collects formatted log output for assertions.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

thread_local! {
    /// Where log lines written on this thread go, if anywhere.
    static CAPTURE: std::cell::RefCell<Option<Captured>> = const { std::cell::RefCell::new(None) };
}

/// Writes to this thread's capture, or nowhere.
struct ToCapture;

impl std::io::Write for ToCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        CAPTURE.with_borrow(|c| {
            if let Some(c) = c {
                c.0.lock().unwrap().extend_from_slice(buf);
            }
        });
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Captured {
    /// Captures the JSON log lines written on this thread from now on. The
    /// subscriber is global: a thread-local one misses spans whose interest
    /// tests on other threads cached first.
    fn start() -> Self {
        static INSTALLED: std::sync::Once = std::sync::Once::new();
        INSTALLED.call_once(|| {
            let _ = tracing::subscriber::set_global_default(
                tracing_subscriber::fmt()
                    .json()
                    .with_max_level(tracing::Level::INFO)
                    .with_writer(|| ToCapture)
                    .finish(),
            );
        });
        let c = Self::default();
        CAPTURE.set(Some(c.clone()));
        c
    }

    /// The captured JSON log events.
    fn events(&self) -> Vec<Value> {
        String::from_utf8(self.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
}

#[tokio::test]
async fn a_failed_operation_is_correlated_with_its_request() {
    const SECRET: &str = "hunter2-correlated-17";
    const REQUEST: &str = "req-correlate-1";
    // The test runtime is single-threaded, so spawned deployments log here too.
    let logs = Captured::start();
    let s = server().await;
    let fake = Arc::new(Fake::default());
    s.provisioning(fake.clone()).await;
    *fake.leak.lock().unwrap() = Some(SECRET.into());

    let env_path = "/orgs/acme/apps/shop/envs/development";
    let (_, env) = s.get(env_path, &s.admin).await;
    let (status, delta) = s
        .post(
            "/orgs/acme/apps/shop/deltas",
            &s.admin,
            json!({"modules": {"add": {"web": {"externals": {"db": {"type": "postgres"}},
                "spec": {"containers": {"web": {"image": "nginx:1"}}}}}}}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{delta}");
    let (status, d) = s
        .call_with(
            Method::POST,
            &format!("{env_path}/deploys"),
            Some(&s.admin),
            Some(json!({"delta_id": delta["id"], "expected_generation": env["generation"]})),
            &[("x-request-id", REQUEST)],
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{d}");
    let id = d["id"].as_str().unwrap().to_owned();
    let d = s.finished_at(&format!("{env_path}/deploys/{id}")).await;
    assert_eq!(d["status"], "failed", "{d}");

    let events = logs.events();
    let text = serde_json::to_string(&events).unwrap();
    assert!(!text.contains(SECRET), "a secret reached the logs");
    // Fields of an event and of every span it is inside.
    let fields = |e: &Value| {
        let mut all = e["fields"].as_object().cloned().unwrap_or_default();
        for span in e["spans"].as_array().into_iter().flatten() {
            all.extend(span.as_object().cloned().unwrap_or_default());
        }
        Value::Object(all)
    };
    let failed: Vec<_> = events
        .iter()
        .map(fields)
        .filter(|f| f["message"] == "driver finished" && f["outcome"] == "failed")
        .collect();
    assert_eq!(failed.len(), 1, "{text}");
    let op = &failed[0];
    assert_eq!(op["request_id"], REQUEST, "{op:?}");
    assert_eq!(op["deployment"], id.as_str(), "{op:?}");
    assert_eq!(op["resource"], DB, "{op:?}");
    assert!(
        op["operation"].is_string() && op["guresid"].is_string(),
        "{op:?}"
    );
    assert!(
        op["error"].as_str().unwrap().contains("[REDACTED]"),
        "{op:?}"
    );
    // The resource's span names the operation too, for events outside the
    // driver call such as a failed checkpoint.
    let resource_spans: Vec<_> = events
        .iter()
        .filter(|e| e["fields"]["message"] == "driver finished")
        .flat_map(|e| e["spans"].as_array().cloned().unwrap_or_default())
        .filter(|s| s["name"] == "resource" && s["resource"] == DB)
        .collect();
    assert!(!resource_spans.is_empty(), "{text}");
    assert!(
        resource_spans
            .iter()
            .all(|s| s["guresid"] == op["guresid"] && s["operation"] == op["operation"]),
        "{resource_spans:?}"
    );
    let finished: Vec<_> = events
        .iter()
        .map(fields)
        .filter(|f| f["message"] == "deployment finished")
        .collect();
    assert!(!finished.is_empty(), "{text}");
    assert!(
        finished
            .iter()
            .all(|f| f["request_id"] == REQUEST && f["deployment"] == id.as_str()),
        "{finished:?}"
    );

    // The audited outcome carries the request that started it.
    let (_, page) = s.get("/orgs/acme/audit-logs?limit=100", &s.admin).await;
    let of_request: Vec<_> = page["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["request_id"] == REQUEST)
        .collect();
    assert!(
        of_request
            .iter()
            .any(|e| e["action"] == "deployment.finish" && e["outcome"] == "failed"),
        "{page}"
    );
    assert!(of_request.len() >= 2, "{page}");
    assert!(!page.to_string().contains(SECRET), "{page}");
}

/// The value of the sample `name` (with its labels) in metrics `text`.
fn sample(text: &str, name: &str) -> f64 {
    text.lines()
        .find_map(|l| l.strip_prefix(name)?.strip_prefix(' '))
        .unwrap_or_else(|| panic!("no `{name}` in:\n{text}"))
        .parse()
        .unwrap()
}

#[tokio::test]
async fn metrics_count_work_outcomes_and_storage() {
    let s = server().await;
    let fake = Arc::new(Fake::default());
    s.provisioning(fake.clone()).await;
    *fake.leak.lock().unwrap() = Some("hunter2-metrics".into());
    let env_path = "/orgs/acme/apps/shop/envs/development";
    let (_, env) = s.get(env_path, &s.admin).await;
    let (status, delta) = s
        .post(
            "/orgs/acme/apps/shop/deltas",
            &s.admin,
            json!({"modules": {"add": {"web": {"externals": {"db": {"type": "postgres"}},
                "spec": {"containers": {"web": {"image": "nginx:1"}}}}}}}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{delta}");
    let (status, d) = s
        .post(
            &format!("{env_path}/deploys"),
            &s.admin,
            json!({"delta_id": delta["id"], "expected_generation": env["generation"]}),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{d}");
    let d = s
        .finished_at(&format!("{env_path}/deploys/{}", d["id"].as_str().unwrap()))
        .await;
    assert_eq!(d["status"], "failed", "{d}");

    let (status, text) = s.get_text("/orgs/acme/metrics", Some(&s.admin)).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert!(!text.contains("hunter2"), "{text}");
    // Counters are process-wide and other tests run alongside, so they are
    // at least this test's work.
    assert!(
        sample(
            &text,
            r#"openporch_deployments_finished_total{outcome="failed"}"#
        ) >= 1.0
    );
    assert!(sample(&text, "openporch_deployment_run_seconds_count") >= 1.0);
    let failed = r#"{action="provision",outcome="failed"}"#;
    assert!(
        sample(
            &text,
            &format!("openporch_driver_call_seconds_count{failed}")
        ) >= 1.0
    );
    assert!(sample(&text, &format!("openporch_driver_call_seconds_sum{failed}")) >= 0.0);
    // The rest describe this server alone.
    assert_eq!(
        sample(&text, r#"openporch_resources{status="failed"}"#),
        1.0,
        "{text}"
    );
    assert_eq!(sample(&text, "openporch_resources_needing_recovery"), 0.0);
    assert_eq!(sample(&text, "openporch_deployments_queued"), 0.0);
    assert_eq!(sample(&text, "openporch_deployments_running"), 0.0);
    assert_eq!(sample(&text, "openporch_ready"), 1.0);
    let wal = sample(&text, "openporch_database_wal_bytes");
    assert!(sample(&text, "openporch_database_bytes") > wal, "{text}");
    assert!(sample(&text, "openporch_audit_entries") >= 2.0);
    assert!(sample(&text, "openporch_deployment_log_lines") >= 1.0);

    // Metrics are for administrators and org viewers.
    let member = s.member("mia", "member").await;
    let (status, _) = s.get_text("/orgs/acme/metrics", Some(&member)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = s.get_text("/orgs/acme/metrics", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// The `Failure leaves a variables file` scenario at the server: a variables
/// file that a crashed OpenTofu run left in the installation is removed when
/// the server starts again, and diagnostics never show it.
#[tokio::test]
async fn a_restart_removes_a_crashed_runs_variables_file_and_diagnostics_omit_it() {
    let dir = tempfile::tempdir().unwrap();
    let run = dir
        .path()
        .join("opentofu/runs/0123456789abcdef0123456789abcdef/00000000deadbeef");
    std::fs::create_dir_all(&run).unwrap();
    std::fs::write(
        run.join("secrets.tfvars.json"),
        r#"{"password":"crashed-run-secret-1"}"#,
    )
    .unwrap();
    // The process group it names is gone: a command that already exited.
    let mut gone = std::process::Command::new("true").spawn().unwrap();
    let pid = gone.id();
    gone.wait().unwrap();
    std::fs::write(run.join(".group"), pid.to_string()).unwrap();

    // Another run's orphaned `tofu` still runs, so its directory stays.
    let orphan_run = run.with_file_name("00000000cafef00d");
    std::fs::create_dir_all(&orphan_run).unwrap();
    std::fs::write(
        orphan_run.join("secrets.tfvars.json"),
        r#"{"password":"orphaned-run-secret-2"}"#,
    )
    .unwrap();
    let mut orphan = {
        use std::os::unix::process::CommandExt as _;
        std::process::Command::new("sleep")
            .arg("60")
            .process_group(0)
            .spawn()
            .unwrap()
    };
    std::fs::write(orphan_run.join(".group"), orphan.id().to_string()).unwrap();

    let s = server_in(dir).await;
    assert!(!run.exists(), "the run directory was kept");
    assert!(orphan_run.join("secrets.tfvars.json").exists());
    let (status, report) = s.get("/orgs/acme/diagnostics", &s.admin).await;
    orphan.kill().unwrap();
    orphan.wait().unwrap();
    assert_eq!(status, StatusCode::OK, "{report}");
    let report = report.to_string();
    for leaked in [
        "crashed-run-secret-1",
        "orphaned-run-secret-2",
        "secrets.tfvars",
    ] {
        assert!(!report.contains(leaked), "{report}");
    }
}

#[tokio::test]
async fn diagnostics_correlate_a_timed_out_deployment_without_secrets() {
    let s = server().await;
    let fake = Arc::new(Fake::default());
    s.provisioning(fake.clone()).await;
    fake.uncertain.lock().unwrap().insert("postgres".into());
    // A long log is cut to its last lines.
    *fake.chatter.lock().unwrap() = 250;
    let timed_out = s.deploy_web("nginx:1", true).await;
    assert_eq!(timed_out["status"], "failed", "{timed_out}");
    let id = timed_out["id"].as_str().unwrap();
    let db = s.resource(DB).await;

    let (status, report) = s
        .get(
            &format!("/orgs/acme/diagnostics?app=shop&env=development&deployment={id}"),
            &s.admin,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert!(report["version"].is_string(), "{report}");
    assert_eq!(report["ready"], true, "{report}");
    assert_eq!(report["config"]["org"], "acme", "{report}");
    assert_eq!(report["resources"]["unknown"], 1, "{report}");
    // The operation whose outcome is unknown, and the resource waiting on it.
    let waiting = report["needing_recovery"].as_array().unwrap();
    assert_eq!(waiting.len(), 1, "{report}");
    assert_eq!(waiting[0]["id"], db["unknown_operation"], "{report}");
    assert_eq!(waiting[0]["deployment_id"], id, "{report}");
    assert_eq!(waiting[0]["resource"], DB, "{report}");
    assert_eq!(waiting[0]["state"], "unknown", "{report}");
    assert_eq!(waiting[0]["resource_status"], "unknown", "{report}");
    // The deployment, with its request, its operations, audit and log.
    let d = &report["deployment"];
    assert_eq!(d["id"], id, "{report}");
    assert_eq!(d["status"], "failed", "{report}");
    assert!(d["request_id"].is_string(), "{report}");
    let ops = d["operations"].as_array().unwrap();
    assert!(
        ops.iter().any(|o| o["id"] == db["unknown_operation"]),
        "{report}"
    );
    let audit = d["audit"].as_array().unwrap();
    assert!(audit.iter().any(|a| a["outcome"] == "accepted"), "{report}");
    assert!(audit.iter().any(|a| a["outcome"] == "failed"), "{report}");
    let log = d["log"].as_array().unwrap();
    assert_eq!(log.len(), 200, "{report}");
    assert!(
        log.last().unwrap()["message"]
            .as_str()
            .unwrap()
            .starts_with("deployment failed"),
        "{report}"
    );
    assert_eq!(d["log_truncated"], true, "{report}");

    // A deployment whose driver leaks a secret keeps it out of the report.
    fake.uncertain.lock().unwrap().clear();
    *fake.chatter.lock().unwrap() = 0;
    let (status, _) = s
        .post(
            &format!("{RESOURCES}/postgres/modules.web.externals.db/resolve"),
            &s.admin,
            json!({"reason": "nothing was created"}),
        )
        .await;
    assert!(status.is_success(), "{status}");
    *fake.leak.lock().unwrap() = Some("hunter2-diagnostics".into());
    let leaked = s.deploy_web("nginx:2", true).await;
    assert_eq!(leaked["status"], "failed", "{leaked}");
    let (status, report) = s
        .get(
            &format!(
                "/orgs/acme/diagnostics?app=shop&env=development&deployment={}",
                leaked["id"].as_str().unwrap()
            ),
            &s.admin,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert!(!report.to_string().contains("hunter2"), "{report}");
    assert!(
        report["deployment"]["operations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o["state"] == "failed"),
        "{report}"
    );

    // Expiring audit entries reports the resources it keeps.
    let (status, b) = s
        .post(
            "/orgs/acme/audit-logs/expire",
            &s.admin,
            json!({"dry_run": true}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{b}");
    let retained = &b["retained"];
    assert!(retained["resources"].as_i64().unwrap() >= 2, "{b}");
    assert!(
        retained["by_driver"]["openporch/echo"].as_i64().unwrap() >= 1,
        "{b}"
    );

    // Naming a deployment takes all three of app, env and deployment.
    let (status, b) = s.get("/orgs/acme/diagnostics?app=shop", &s.admin).await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
    let (status, b) = s
        .get(
            "/orgs/acme/diagnostics?app=shop&env=development&deployment=nope",
            &s.admin,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{b}");
    // Diagnostics are for administrators and org viewers.
    let member = s.member("mia", "member").await;
    let (status, _) = s.get("/orgs/acme/diagnostics", &member).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

const WEB: &str = "workload.default#modules.web";

/// `deployments`: `Partial success` and `Reading after partial failure`. The
/// database's update succeeds and the workload's then fails: the database
/// keeps its new result, the previous successful deployment stays
/// identified, the environment reports partial convergence, and the
/// deployment's graph shows both outcomes.
#[tokio::test]
async fn a_database_update_succeeds_and_the_workload_fails() {
    let s = server().await;
    let fake = Arc::new(Fake::default());
    s.provisioning(fake.clone()).await;
    let good = s.deploy_web("nginx:1", true).await;
    assert_eq!(good["status"], "succeeded", "{good}");
    let (_, env) = s.get(ENV_DEV, &s.admin).await;
    let good_set = env["last_successful_set_id"].clone();
    assert_eq!(env["convergence"], "converged", "{env}");

    // The database's definition changes, and the workload cannot be updated.
    let (status, b) = s
        .call(
            Method::PUT,
            "/orgs/acme/resources/defs/pg",
            Some(&s.admin),
            Some(json!({"name": "Postgres v2", "driver_type": "openporch/echo"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{b}");
    fake.failing.lock().unwrap().insert("workload".into());
    let bad = s.deploy_web("nginx:2", true).await;
    assert_eq!(bad["status"], "failed", "{bad}");
    assert!(
        bad["error"]
            .as_str()
            .unwrap()
            .contains("workload is unavailable"),
        "{bad}"
    );

    // The database's successful update is kept.
    let db = s.resource(DB).await;
    assert_eq!(db["status"], "provisioned", "{db}");
    assert_eq!(db["definition_version"], 2, "{db}");
    assert_eq!(db["usage"]["last_deploy_id"], bad["id"], "{db}");
    assert_eq!(db["outputs"]["host"], "db.internal", "{db}");
    let web = s.resource(WEB).await;
    assert_eq!(web["status"], "failed", "{web}");

    // The last success stays identified, and the environment does not claim
    // the old set is still fully deployed.
    let (_, env) = s.get(ENV_DEV, &s.admin).await;
    assert_eq!(env["last_successful_deploy_id"], good["id"], "{env}");
    assert_eq!(env["last_successful_set_id"], good_set, "{env}");
    assert_eq!(env["desired_set_id"], bad["set_id"], "{env}");
    assert_ne!(env["desired_set_id"], good_set, "{env}");
    assert_eq!(env["latest_deployment"]["id"], bad["id"], "{env}");
    assert_eq!(env["convergence"], "partial", "{env}");

    // Both branches' recorded outcomes are read back from the failed
    // deployment.
    let (status, g) = s.get(&format!("{RESOURCES}/graph"), &s.admin).await;
    assert_eq!(status, StatusCode::OK, "{g}");
    assert_eq!(g["deployment_id"], bad["id"], "{g}");
    assert_eq!(g["status"], "failed", "{g}");
    let node = |descriptor: &str| {
        g["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["descriptor"] == descriptor)
            .cloned()
            .unwrap_or_else(|| panic!("{descriptor}: {g}"))
    };
    assert_eq!(node(DB)["status"], "provisioned", "{g}");
    assert!(node(DB)["error"].is_null(), "{g}");
    assert_eq!(node(WEB)["status"], "failed", "{g}");
    assert!(
        node(WEB)["error"]
            .as_str()
            .unwrap()
            .contains("workload is unavailable"),
        "{g}"
    );
}

/// `deployments`: `Recover after a failed deployment`. After a failed
/// attempt, replaying the last successful deployment uses its recorded set
/// and snapshot, and is still subject to current permissions and to the
/// destructive-action policy: it removes a workload the failed attempt
/// added, which needs a permit.
#[tokio::test]
async fn recover_after_a_failed_deployment_by_replaying_the_last_success() {
    let s = server().await;
    let fake = Arc::new(Fake::default());
    s.provisioning(fake.clone()).await;
    let good = s.deploy_web("nginx:1", false).await;
    assert_eq!(good["status"], "succeeded", "{good}");
    let good_id = good["id"].as_str().unwrap().to_owned();

    // The next attempt adds `api` and a database for `web`; the database
    // fails, after `api` was provisioned.
    let (status, delta) = s
        .post(
            "/orgs/acme/apps/shop/deltas",
            &s.admin,
            json!({"modules": {"remove": ["web"], "add": {
                "web": {"externals": {"db": {"type": "postgres"}},
                        "spec": {"containers": {"web": {"image": "nginx:2"}}}},
                "api": {"spec": {"containers": {"api": {"image": "api:1"}}}}}}}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{delta}");
    fake.failing.lock().unwrap().insert("postgres".into());
    let (_, env) = s.get(ENV_DEV, &s.admin).await;
    let (status, d) = s
        .post(
            DEPLOYS,
            &s.admin,
            json!({"delta_id": delta["id"], "expected_generation": env["generation"]}),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{d}");
    let bad = s
        .finished_at(&format!("{DEPLOYS}/{}", d["id"].as_str().unwrap()))
        .await;
    assert_eq!(bad["status"], "failed", "{bad}");
    assert_eq!(
        s.resource("workload.default#modules.api").await["status"],
        "provisioned"
    );
    fake.failing.lock().unwrap().clear();

    let replay = |token: String, permits: Vec<String>| {
        let s = &s;
        async move {
            let (_, env) = s.get(ENV_DEV, &s.admin).await;
            s.post(
                DEPLOYS,
                &token,
                json!({"deploy_id": "last-successful", "expected_generation": env["generation"],
                       "permits": permits}),
            )
            .await
        }
    };
    // Current permissions apply: a developer without `deployer` is refused.
    let dev = s.member("dev", "member").await;
    s.grant_app("shop", "dev", "developer").await;
    let (status, b) = replay(dev.clone(), Vec::new()).await;
    assert_error(status, &b, StatusCode::FORBIDDEN, "AUTH-002");
    s.grant_deployer("development", "dev").await;

    // The destructive-action policy applies: removing `api` needs a permit.
    let (status, b) = replay(dev.clone(), Vec::new()).await;
    assert_error(status, &b, StatusCode::CONFLICT, "DPL-001");
    let (_, list) = s.get(DEPLOYS, &s.admin).await;
    assert_eq!(list["deployments"].as_array().unwrap().len(), 2, "{list}");

    let permits = s
        .permits_for(ENV_DEV, json!({"deploy_id": "last-successful"}))
        .await;
    assert_eq!(permits.len(), 1, "{permits:?}");
    let (status, d) = replay(dev, permits).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{d}");
    assert_eq!(d["set_id"], good["set_id"], "{d}");
    let id = d["id"].as_str().unwrap().to_owned();
    let snapshot = s
        .state
        .store
        .deployment_snapshot("acme", "shop", "development", &id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot["replayed_from"], good_id.as_str(), "{snapshot}");
    let good_snapshot = s
        .state
        .store
        .deployment_snapshot("acme", "shop", "development", &good_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot["graph"], good_snapshot["graph"]);
    let done = s.finished_at(&format!("{DEPLOYS}/{id}")).await;
    assert_eq!(done["status"], "succeeded", "{done}");
    assert!(
        fake.deleted
            .lock()
            .unwrap()
            .contains(&"workload.default#modules.api".to_owned())
    );
    let (_, env) = s.get(ENV_DEV, &s.admin).await;
    assert_eq!(env["last_successful_deploy_id"], id.as_str(), "{env}");
    assert_eq!(env["convergence"], "converged", "{env}");
}

/// `deployments`: `Stale or concurrent request`, with an environment
/// deletion still running: a deployment is refused with `409`, the desired
/// state does not change and the delta is not frozen.
#[tokio::test]
async fn a_deployment_while_the_environment_is_being_deleted_conflicts() {
    const ENV: &str = "/orgs/acme/apps/shop/envs/pr-42";
    let s = server().await;
    let fake = Arc::new(Fake::default());
    s.provisioning(fake.clone()).await;
    let (status, b) = s
        .post(
            "/orgs/acme/apps/shop/envs",
            &s.admin,
            json!({"id": "pr-42", "type": "development"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{b}");
    let d = s.deploy_web_in("pr-42", "nginx:1", true, false).await;
    assert_eq!(d["status"], "succeeded", "{d}");

    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    *fake.held_deletes.lock().unwrap() = Some(gate.clone());
    let (_, env) = s.get(ENV, &s.admin).await;
    let deletion = s.delete_env(ENV, env["generation"].as_i64().unwrap()).await;
    let deletion_id = deletion["deployment"]["id"].as_str().unwrap().to_owned();
    let (_, before) = s.get(ENV, &s.admin).await;
    assert_eq!(
        before["latest_deployment"]["status"], "in_progress",
        "{before}"
    );

    let (status, delta) = s
        .post(
            "/orgs/acme/apps/shop/deltas",
            &s.admin,
            json!({"modules": {"remove": ["web"], "add": {"web": {
                "spec": {"containers": {"web": {"image": "nginx:2"}}}}}}}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{delta}");
    let (status, b) = s
        .post(
            &format!("{ENV}/deploys"),
            &s.admin,
            json!({"delta_id": delta["id"], "expected_generation": before["generation"]}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{b}");
    // Nothing changed.
    let (_, after) = s.get(ENV, &s.admin).await;
    for field in [
        "generation",
        "desired_set_id",
        "last_successful_set_id",
        "state",
    ] {
        assert_eq!(after[field], before[field], "{field}: {after}");
    }
    assert_eq!(after["latest_deployment"]["id"], deletion_id.as_str());
    let (_, unfrozen) = s
        .get(
            &format!(
                "/orgs/acme/apps/shop/deltas/{}",
                delta["id"].as_str().unwrap()
            ),
            &s.admin,
        )
        .await;
    assert_eq!(unfrozen["frozen"], false, "{unfrozen}");

    // The deletion then finishes, and the environment is gone.
    gate.add_permits(100);
    let mut gone = false;
    for _ in 0..500 {
        if s.get(ENV, &s.admin).await.0 == StatusCode::NOT_FOUND {
            gone = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(gone, "the environment was not deleted");
}

/// `applications-and-environments`: `Delete an application with provisioned
/// resources`, with an external and an adopted resource: the owned workload
/// is deleted, and the other two stay and are listed in the deletion result.
#[tokio::test]
async fn an_application_deletion_lists_what_it_leaves_in_place() {
    const APP: &str = "/orgs/acme/apps/shop";
    const BASE: &str = "base-env.default#base-env";
    let s = server().await;
    let fake = Arc::new(Fake::default());
    s.provisioning(fake.clone()).await;
    let d = s.deploy_web("nginx:1", true).await;
    assert_eq!(d["status"], "succeeded", "{d}");
    // The base environment is managed elsewhere, and the database was adopted.
    for (descriptor, ownership) in [(BASE, "external"), (DB, "adopted")] {
        sqlx::query(
            "UPDATE active_resources SET ownership = ? WHERE app_id = 'shop' AND descriptor = ?",
        )
        .bind(ownership)
        .bind(descriptor)
        .execute(s.state.store.pool())
        .await
        .unwrap();
    }
    let guresid = |descriptor: &'static str| {
        let s = &s;
        async move { s.resource(descriptor).await["guresid"].clone() }
    };
    let (base, db) = (guresid(BASE).await, guresid(DB).await);

    let permits = s.app_deletion_permits(APP).await;
    let (status, deletion) = s
        .call(
            Method::DELETE,
            APP,
            Some(&s.admin),
            Some(json!({"permits": permits})),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{deletion}");
    let dev = deletion["deletions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["deployment"]["env"] == "development")
        .cloned()
        .unwrap_or_else(|| panic!("{deletion}"));
    let mut retained: Vec<Value> = dev["retained"].as_array().unwrap().clone();
    retained.sort_by_key(|r| r["descriptor"].as_str().unwrap().to_owned());
    assert_eq!(
        retained,
        [
            json!({"descriptor": BASE, "guresid": base, "ownership": "external"}),
            json!({"descriptor": DB, "guresid": db, "ownership": "adopted"}),
        ],
        "{deletion}"
    );

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
    assert_eq!(deleted, [WEB], "{deleted:?}");
}
