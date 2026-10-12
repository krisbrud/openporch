use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use openporch_core::model::SecretRef;
use openporch_engine::planner::{
    DeprovisionJob, ExecFailure, Executor, Limits, PermitCheck, PermitRequest, Planner,
    ProvisionJob, ProvisionResult,
};
use serde_json::{Value, json};

use super::*;
use crate::deploy::PlannerRunner;

/// Provisions every resource except those of the failing types.
#[derive(Debug, Default)]
struct Fake {
    failing: Mutex<BTreeSet<String>>,
    /// The GUResID each job was handed, by descriptor.
    guresids: Mutex<Vec<(String, String)>>,
    /// The descriptors deleted, in order.
    deleted: Mutex<Vec<String>>,
    /// A secret value the executor resolves for postgres, and then leaks into
    /// its output and error.
    leak: Mutex<Option<String>>,
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

    async fn deprovision(&self, job: DeprovisionJob) -> Result<(), ExecFailure> {
        self.deleted
            .lock()
            .unwrap()
            .push(job.job.descriptor.to_string());
        Ok(())
    }
}

/// Accepts the permit `permit-1` for any destructive action.
#[derive(Debug)]
struct OnePermit;

impl PermitCheck for OnePermit {
    fn check<'a>(
        &'a self,
        req: PermitRequest<'a>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        let ok = req.permits.iter().any(|p| p == "permit-1");
        Box::pin(async move {
            if ok {
                Ok(())
            } else {
                Err("no matching permit".into())
            }
        })
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
        let planner = Planner::new(Arc::new(self.state.store.clone()), fake, limits)
            .with_permit_check(Arc::new(OnePermit));
        self.state.permits.set(Arc::new(OnePermit));
        self.state
            .runners
            .set(Arc::new(PlannerRunner::new(Arc::new(planner))));
    }

    /// Deploys `web` with image `image` and, when `db`, its database, and
    /// waits for the outcome.
    async fn deploy_web(&self, image: &str, db: bool) -> Value {
        self.deploy_web_with(image, db, &[]).await
    }

    /// [`Self::deploy_web`] with destructive-action permits.
    async fn deploy_web_with(&self, image: &str, db: bool, permits: &[&str]) -> Value {
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
                json!({"delta_id": delta["id"], "expected_generation": env["generation"],
                    "permits": permits}),
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

    // Leaving it out without a permit fails, and the database stays.
    let d = s.deploy_web("nginx:3", false).await;
    assert_eq!(d["status"], "failed", "{d}");
    assert!(
        d["error"].as_str().unwrap().contains("approval_required"),
        "{d}"
    );
    assert_eq!(s.resource(DB).await["status"], "provisioned");
    assert!(fake.deleted.lock().unwrap().is_empty());

    // With a permit, the driver deletes it and the record disappears.
    let d = s.deploy_web_with("nginx:4", false, &["permit-1"]).await;
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
    let body = |reason: &str, permits: &[&str]| json!({"reason": reason, "permits": permits});

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
        .post(&detach, &developer, body("moving it", &["permit-1"]))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{b}");
    // A reason and a matching permit are required.
    let (status, b) = s.post(&detach, &s.admin, body("  ", &["permit-1"])).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{b}");
    let (status, b) = s.post(&detach, &s.admin, body("moving it", &[])).await;
    assert_eq!(status, StatusCode::CONFLICT, "{b}");
    assert!(
        b["message"].as_str().unwrap().contains("approval_required"),
        "{b}"
    );
    assert_eq!(s.resource(DB).await["guresid"], db["guresid"]);

    let (status, tombstone) = s
        .post(
            &detach,
            &s.admin,
            body("moving it to the shared cluster", &["permit-1"]),
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
        messages.contains(&"failed: password authentication failed: [REDACTED]"),
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

    let (status, b) = s.get(&format!("{logs}?limit=0"), &s.admin).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{b}");
    let (status, b) = s.get(&format!("{DEPLOYS}/nope/logs"), &s.admin).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{b}");
    // Someone who cannot see the application cannot see that it exists.
    let stranger = s.member("sam", "member").await;
    let (status, b) = s.get(&logs, &stranger).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{b}");
}
