//! Previews, permits, and acceptance refusing what they do not cover.

use std::sync::Arc;

use axum::http::StatusCode;
use openporch_engine::planner::{DestructiveAction, PermitCheck, PermitRequest};
use serde_json::{Value, json};

use super::resources::{DB, ENV_DEV, Fake};
use super::*;
use crate::deploy::StorePermits;

const PLANS: &str = "/orgs/acme/apps/shop/envs/development/plans";

impl Server {
    async fn dev_generation(&self) -> Value {
        self.get(ENV_DEV, &self.admin).await.1["generation"].clone()
    }

    /// Previews `body` as an administrator.
    async fn plan(&self, mut body: Value) -> (StatusCode, Value) {
        body["expected_generation"] = self.dev_generation().await;
        self.post(PLANS, &self.admin, body).await
    }

    /// The desired set, deployed again.
    async fn redeploy_body(&self) -> Value {
        json!({"set_id": self.get(ENV_DEV, &self.admin).await.1["desired_set_id"]})
    }
}

fn entry<'a>(plan: &'a Value, descriptor: &str) -> &'a Value {
    plan["resources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["descriptor"] == descriptor)
        .unwrap_or_else(|| panic!("{descriptor} is not in {plan}"))
}

#[tokio::test]
async fn a_preview_shows_each_change_and_is_kept() {
    let s = server().await;
    s.provisioning(Arc::default()).await;
    s.deploy_web("nginx:1", false).await;
    let body = s.redeploy_body().await;

    // A stale generation is refused.
    let (status, b) = s
        .post(
            PLANS,
            &s.admin,
            json!({"set_id": body["set_id"], "expected_generation": 99}),
        )
        .await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");

    let (status, plan) = s.plan(body).await;
    assert_eq!(status, StatusCode::CREATED, "{plan}");
    assert_eq!(plan["kind"], "deploy");
    assert_eq!(plan["complete"], false);
    let web = entry(&plan, "workload.default#modules.web");
    assert_eq!(web["action"], "update", "{plan}");
    assert_eq!(web["permit_required"], false);

    let id = plan["id"].as_str().unwrap();
    let (status, again) = s.get(&format!("{PLANS}/{id}"), &s.admin).await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["resources"], plan["resources"]);
    // Plans belong to their environment.
    let (status, b) = s
        .get(
            &format!("/orgs/acme/apps/shop/envs/nope/plans/{id}"),
            &s.admin,
        )
        .await;
    assert_error(status, &b, StatusCode::NOT_FOUND, "API-001");

    // Nothing here needs a permit.
    let (status, b) = s
        .post(
            &format!("{PLANS}/{id}/permits"),
            &s.admin,
            json!({"permits": [{"descriptor": "workload.default#modules.web",
                                "action": "delete"}]}),
        )
        .await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
}

#[tokio::test]
async fn previews_and_permits_need_deploy_permission() {
    let s = server().await;
    s.provisioning(Arc::default()).await;
    s.deploy_web("nginx:1", true).await;
    let dev = s.member("dev", "member").await;
    let body = json!({"kind": "delete_env", "expected_generation": s.dev_generation().await});
    let (status, b) = s.post(PLANS, &dev, body.clone()).await;
    assert_error(status, &b, StatusCode::NOT_FOUND, "API-001");
    s.grant_app("shop", "dev", "developer").await;
    let (status, b) = s.post(PLANS, &dev, body.clone()).await;
    assert_error(status, &b, StatusCode::FORBIDDEN, "AUTH-002");
    s.grant_deployer("development", "dev").await;
    let (status, plan) = s.post(PLANS, &dev, body).await;
    assert_eq!(status, StatusCode::CREATED, "{plan}");
    assert_eq!(entry(&plan, DB)["action"], "delete");
    assert_eq!(entry(&plan, DB)["permit_required"], true);
    let (status, issued) = s
        .post(
            &format!("{PLANS}/{}/permits", plan["id"].as_str().unwrap()),
            &dev,
            json!({"permits": [{"descriptor": DB, "action": "delete"}]}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{issued}");
    assert_eq!(issued["permits"][0]["issuer"], "dev");

    // Detaching needs an administrator to issue the permit.
    let (status, plan) = s
        .post(
            PLANS,
            &dev,
            json!({"kind": "detach", "descriptor": DB,
                   "expected_generation": s.dev_generation().await}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{plan}");
    let (status, b) = s
        .post(
            &format!("{PLANS}/{}/permits", plan["id"].as_str().unwrap()),
            &dev,
            json!({"permits": [{"descriptor": DB, "action": "detach"}]}),
        )
        .await;
    assert_error(status, &b, StatusCode::FORBIDDEN, "AUTH-002");
}

#[tokio::test]
async fn a_direct_deploy_cannot_bypass_permits() {
    let s = server().await;
    let fake = Arc::new(Fake::default());
    s.provisioning(fake.clone()).await;
    s.deploy_web("nginx:1", true).await;
    let generation = s.dev_generation().await;

    // Removing the database is refused at acceptance, whatever is presented.
    for permits in [json!([]), json!(["prm-made-up"])] {
        let (status, b) = s
            .post(
                &format!("{ENV_DEV}/deploys"),
                &s.admin,
                json!({"delta_id": removing_web(&s).await, "expected_generation": generation,
                       "permits": permits}),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{b}");
        assert!(b["error"] == "DPL-001" || b["error"] == "DPL-003", "{b}");
    }
    assert_eq!(s.dev_generation().await, generation);

    // So is deleting the application.
    let (status, b) = s
        .call(
            axum::http::Method::DELETE,
            "/orgs/acme/apps/shop",
            Some(&s.admin),
            None,
        )
        .await;
    assert_error(status, &b, StatusCode::CONFLICT, "DPL-001");
    assert!(fake.deleted.lock().unwrap().is_empty());
}

/// A delta that removes `web`.
async fn removing_web(s: &Server) -> Value {
    let (status, delta) = s
        .post(
            "/orgs/acme/apps/shop/deltas",
            &s.admin,
            json!({"modules": {"remove": ["web"]}}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{delta}");
    delta["id"].clone()
}

#[tokio::test]
async fn stale_permits_fail() {
    let s = server().await;
    s.provisioning(Arc::default()).await;
    s.deploy_web("nginx:1", true).await;
    let delta = removing_web(&s).await;
    let permits = s.permits_for(ENV_DEV, json!({"delta_id": delta})).await;
    assert!(!permits.is_empty(), "{permits:?}");

    // Another deployment moves the environment on.
    s.deploy_web("nginx:2", true).await;
    let (status, b) = s
        .post(
            &format!("{ENV_DEV}/deploys"),
            &s.admin,
            json!({"delta_id": removing_web(&s).await,
                   "expected_generation": s.dev_generation().await, "permits": permits}),
        )
        .await;
    assert_error(status, &b, StatusCode::CONFLICT, "DPL-003");
    assert!(b["message"].as_str().unwrap().contains("generation"), "{b}");

    // A permit from a fresh preview works once.
    let d = s.deploy_web_with("nginx:3", false, true).await;
    assert_eq!(d["status"], "succeeded", "{d}");
}

#[tokio::test]
async fn binding_and_using_permits_are_audited() {
    let s = server().await;
    s.provisioning(Arc::default()).await;
    s.deploy_web("nginx:1", true).await;
    let delta = removing_web(&s).await;
    let permits = s.permits_for(ENV_DEV, json!({"delta_id": delta})).await;
    assert!(!permits.is_empty(), "{permits:?}");
    let (status, d) = s
        .post(
            &format!("{ENV_DEV}/deploys"),
            &s.admin,
            json!({"delta_id": delta,
                   "expected_generation": s.dev_generation().await, "permits": permits}),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{d}");
    let id = d["id"].as_str().unwrap();
    let d = s.finished_at(&format!("{ENV_DEV}/deploys/{id}")).await;
    assert_eq!(d["status"], "succeeded", "{d}");

    let (status, page) = s.get("/orgs/acme/audit-logs?limit=200", &s.admin).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let entries = page["entries"].as_array().unwrap();
    let issued = entries
        .iter()
        .find(|e| e["action"] == "permit.create")
        .unwrap();
    for (action, outcome) in [("permit.bind", "accepted"), ("permit.use", "succeeded")] {
        let found: Vec<&Value> = entries.iter().filter(|e| e["action"] == action).collect();
        assert_eq!(found.len(), permits.len(), "{action}: {page}");
        for e in found {
            assert_eq!(e["outcome"], outcome, "{e}");
            assert_eq!(e["operation_id"], id, "{e}");
            assert_eq!(e["correlates"], issued["id"], "{e}");
            assert!(
                e["target"]
                    .as_str()
                    .unwrap()
                    .starts_with("shop/development/"),
                "{e}"
            );
        }
    }
}

#[tokio::test]
async fn a_refused_driver_plan_shows_in_the_next_preview() {
    let s = server().await;
    s.provisioning(Arc::default()).await;
    s.deploy_web("nginx:1", true).await;
    let db = s.resource(DB).await;
    let guresid = db["guresid"].as_str().unwrap().to_owned();
    let latest = s.get(&format!("{ENV_DEV}/deploys"), &s.admin).await.1["deployments"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // The deployment's driver planned a replacement nothing permitted.
    let check = StorePermits(s.state.store.clone());
    let planned = vec![("replace".to_owned(), "aws_db_instance.main".to_owned())];
    let request = |deployment: &'static str| PermitRequest {
        org: "acme",
        app: "shop",
        env: "development",
        descriptor: DB,
        guresid: Some(&guresid),
        deployment: Some(deployment),
        action: DestructiveAction::Replace,
        permits: &[],
        plan: Some("fp-1"),
        planned: &planned,
    };
    let leaked: &'static str = Box::leak(latest.into_boxed_str());
    let e = check.check(request(leaked)).await.unwrap_err();
    assert!(e.contains("preview the environment again"), "{e}");

    let (status, plan) = s.plan(s.redeploy_body().await).await;
    assert_eq!(status, StatusCode::CREATED, "{plan}");
    let replace = entry(&plan, DB);
    assert_eq!(replace["action"], "replace", "{plan}");
    assert_eq!(replace["driver_plan"]["fingerprint"], "fp-1");
    assert_eq!(
        replace["driver_plan"]["actions"][0][1],
        "aws_db_instance.main"
    );

    // A permit for another plan is refused; one for this plan binds.
    let permits = format!("{PLANS}/{}/permits", plan["id"].as_str().unwrap());
    let (status, b) = s
        .post(
            &permits,
            &s.admin,
            json!({"permits": [{"descriptor": DB, "action": "replace", "fingerprint": "fp-2"}]}),
        )
        .await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
    let (status, issued) = s
        .post(
            &permits,
            &s.admin,
            json!({"permits": [{"descriptor": DB, "action": "replace"}]}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{issued}");
    assert_eq!(issued["permits"][0]["plan_fingerprint"], "fp-1");
    let permit = issued["permits"][0]["id"].clone();

    // Without it the redeploy is refused; with it, it is accepted and the
    // replacement it covers passes the check.
    let mut body = s.redeploy_body().await;
    body["expected_generation"] = s.dev_generation().await;
    let (status, b) = s
        .post(&format!("{ENV_DEV}/deploys"), &s.admin, body.clone())
        .await;
    assert_error(status, &b, StatusCode::CONFLICT, "DPL-001");
    body["permits"] = json!([permit]);
    let (status, d) = s.post(&format!("{ENV_DEV}/deploys"), &s.admin, body).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{d}");
    let id: &'static str = Box::leak(d["id"].as_str().unwrap().to_owned().into_boxed_str());
    check.check(request(id)).await.unwrap();
}

#[tokio::test]
async fn a_driver_that_cannot_preview_needs_an_unknown_impact_permit() {
    let s = server().await;
    s.provisioning(Arc::default()).await;
    s.deploy_web("nginx:1", true).await;
    // This server cannot tell what the database's driver will do.
    let mut registry = openporch_drivers::DriverRegistry::default();
    registry
        .register(Arc::new(openporch_drivers::NoneDriver::new()) as Arc<_>)
        .ok();
    s.state.drivers.set(Arc::new(registry), None);

    let (status, plan) = s.plan(s.redeploy_body().await).await;
    assert_eq!(status, StatusCode::CREATED, "{plan}");
    let db = entry(&plan, DB);
    assert_eq!(db["action"], "unknown", "{plan}");
    assert_eq!(db["known"], false);
    assert_eq!(db["permit_required"], true);

    let mut body = s.redeploy_body().await;
    body["expected_generation"] = s.dev_generation().await;
    let (status, b) = s
        .post(&format!("{ENV_DEV}/deploys"), &s.admin, body.clone())
        .await;
    assert_error(status, &b, StatusCode::CONFLICT, "DPL-001");
    body["permits"] = json!(s.permits_for(ENV_DEV, s.redeploy_body().await).await);
    let (status, d) = s.post(&format!("{ENV_DEV}/deploys"), &s.admin, body).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{d}");
}

#[tokio::test]
async fn a_refused_deletion_in_a_driver_plan_gets_a_delete_permit() {
    let s = server().await;
    s.provisioning(Arc::default()).await;
    s.deploy_web("nginx:1", true).await;
    let guresid = s.resource(DB).await["guresid"].as_str().unwrap().to_owned();
    let latest = s.get(&format!("{ENV_DEV}/deploys"), &s.admin).await.1["deployments"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let check = StorePermits(s.state.store.clone());
    let planned = vec![("delete".to_owned(), "aws_db_parameter_group.old".to_owned())];
    let request = |deployment: String| {
        let deployment: &'static str = Box::leak(deployment.into_boxed_str());
        let guresid: &'static str = Box::leak(guresid.clone().into_boxed_str());
        let planned: &'static [(String, String)] = Box::leak(planned.clone().into_boxed_slice());
        PermitRequest {
            org: "acme",
            app: "shop",
            env: "development",
            descriptor: DB,
            guresid: Some(guresid),
            deployment: Some(deployment),
            action: DestructiveAction::Delete,
            permits: &[],
            plan: Some("fp-del"),
            planned,
        }
    };
    check.check(request(latest)).await.unwrap_err();

    // The preview shows the deletion the plan makes, and a permit for it is
    // the one the run asks for.
    let (status, plan) = s.plan(s.redeploy_body().await).await;
    assert_eq!(status, StatusCode::CREATED, "{plan}");
    assert_eq!(entry(&plan, DB)["action"], "delete", "{plan}");
    let mut body = s.redeploy_body().await;
    body["expected_generation"] = s.dev_generation().await;
    body["permits"] = json!(s.permits_for(ENV_DEV, s.redeploy_body().await).await);
    let deploys = format!("{ENV_DEV}/deploys");
    let retry = || {
        s.call_with(
            axum::http::Method::POST,
            &deploys,
            Some(&s.admin),
            Some(body.clone()),
            &[("idempotency-key", "fix-db")],
        )
    };
    let (status, d) = retry().await;
    assert_eq!(status, StatusCode::ACCEPTED, "{d}");
    // A retry with the same key returns the deployment its permits are bound to.
    let (status, again) = retry().await;
    assert_eq!(status, StatusCode::ACCEPTED, "{again}");
    assert_eq!(again["id"], d["id"]);
    check
        .check(request(d["id"].as_str().unwrap().to_owned()))
        .await
        .unwrap();
}

#[tokio::test]
async fn a_state_backend_move_is_refused_before_acceptance() {
    let s = server().await;
    s.provisioning(Arc::default()).await;
    // The database is an OpenTofu module, provisioned with local state.
    let (status, b) = s
        .post(
            "/orgs/acme/resources/defs",
            &s.admin,
            json!({"id": "pg-tofu", "type": "postgres", "driver_type": "openporch/opentofu",
                   "driver_inputs": {"values": {"script": "output \"host\" { value = \"db\" }"}},
                   "criteria": [{"app_id": "shop"}]}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{b}");
    s.deploy_web("nginx:1", true).await;
    let (status, plan) = s.plan(s.redeploy_body().await).await;
    assert_eq!(status, StatusCode::CREATED, "{plan}");
    assert_eq!(entry(&plan, DB)["action"], "update", "{plan}");

    // The server is reconfigured with S3 state.
    let mut tofu = openporch_drivers::OpenTofuConfig::new(s.dir.path().join("opentofu"));
    let s3 = openporch_drivers::OpenTofuS3State::new("state", "eu-north-1");
    tofu.state = openporch_drivers::OpenTofuStateBackend::S3(Box::new(s3));
    let backend = tofu.state.location();
    let registry = openporch_drivers::DriverRegistry::with_builtins().with_opentofu(tofu);
    s.state.drivers.set(Arc::new(registry), Some(backend));

    let (status, b) = s.plan(s.redeploy_body().await).await;
    assert_error(status, &b, StatusCode::CONFLICT, "DPL-002");
    assert!(
        b["message"].as_str().unwrap().contains(
            "keeps its state in local state, but its driver now uses s3://state/openporch"
        ),
        "{b}"
    );
    let mut body = s.redeploy_body().await;
    body["expected_generation"] = s.dev_generation().await;
    let (status, b) = s.post(&format!("{ENV_DEV}/deploys"), &s.admin, body).await;
    assert_error(status, &b, StatusCode::CONFLICT, "DPL-002");
}

#[tokio::test]
async fn destructive_permits_in_a_protected_environment_need_an_administrator() {
    let s = server().await;
    s.provisioning(Arc::default()).await;
    s.deploy_web("nginx:1", true).await;
    let dev = s.member("dev", "member").await;
    s.grant_app("shop", "dev", "developer").await;
    s.grant_deployer("development", "dev").await;
    let protect = |by: String, protected: bool| {
        let s = &s;
        async move {
            s.call(
                axum::http::Method::PATCH,
                "/orgs/acme/env-types/development",
                Some(&by),
                Some(json!({"protected": protected})),
            )
            .await
        }
    };
    // Only an administrator marks a type protected.
    let (status, b) = protect(dev.clone(), true).await;
    assert_error(status, &b, StatusCode::FORBIDDEN, "AUTH-002");
    let (status, t) = protect(s.admin.clone(), true).await;
    assert_eq!(status, StatusCode::OK, "{t}");
    assert_eq!(t["protected"], true);

    // A developer who may deploy there previews deleting the database, but
    // gets no permit for it; an administrator does.
    let body = json!({"kind": "delete_env", "expected_generation": s.dev_generation().await});
    let (status, plan) = s.post(PLANS, &dev, body).await;
    assert_eq!(status, StatusCode::CREATED, "{plan}");
    let permits = format!("{PLANS}/{}/permits", plan["id"].as_str().unwrap());
    let ask = json!({"permits": [{"descriptor": DB, "action": "delete"}]});
    let (status, b) = s.post(&permits, &dev, ask.clone()).await;
    assert_error(status, &b, StatusCode::FORBIDDEN, "AUTH-002");
    assert!(b["message"].as_str().unwrap().contains("protected"), "{b}");
    let (status, issued) = s.post(&permits, &s.admin, ask.clone()).await;
    assert_eq!(status, StatusCode::CREATED, "{issued}");

    // A permit a developer got before the type was protected no longer holds.
    let (status, _) = protect(s.admin.clone(), false).await;
    assert_eq!(status, StatusCode::OK);
    let delta = removing_web(&s).await;
    let body = json!({"delta_id": delta, "expected_generation": s.dev_generation().await});
    let (status, plan) = s.post(PLANS, &dev, body).await;
    assert_eq!(status, StatusCode::CREATED, "{plan}");
    let asks: Vec<Value> = plan["resources"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["permit_required"] == true)
        .map(|r| json!({"descriptor": r["descriptor"], "action": r["action"]}))
        .collect();
    assert!(!asks.is_empty(), "{plan}");
    let (status, issued) = s
        .post(
            &format!("{PLANS}/{}/permits", plan["id"].as_str().unwrap()),
            &dev,
            json!({"permits": asks}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{issued}");
    let ids: Vec<Value> = issued["permits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].clone())
        .collect();
    let (status, _) = protect(s.admin.clone(), true).await;
    assert_eq!(status, StatusCode::OK);
    let (status, b) = s
        .post(
            &format!("{ENV_DEV}/deploys"),
            &dev,
            json!({"delta_id": delta, "expected_generation": s.dev_generation().await,
                   "permits": ids}),
        )
        .await;
    assert_error(status, &b, StatusCode::CONFLICT, "DPL-003");
    assert!(b["message"].as_str().unwrap().contains("protected"), "{b}");
}

#[tokio::test]
async fn a_protected_environment_refuses_changes_of_unknown_impact() {
    let s = server().await;
    s.provisioning(Arc::default()).await;
    s.deploy_web("nginx:1", true).await;
    let mut registry = openporch_drivers::DriverRegistry::default();
    registry
        .register(Arc::new(openporch_drivers::NoneDriver::new()) as Arc<_>)
        .ok();
    s.state.drivers.set(Arc::new(registry), None);
    let (status, t) = s
        .call(
            axum::http::Method::PATCH,
            "/orgs/acme/env-types/development",
            Some(&s.admin),
            Some(json!({"protected": true})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{t}");

    // Not even an administrator gets a permit for a change no driver planned.
    let (status, plan) = s.plan(s.redeploy_body().await).await;
    assert_eq!(status, StatusCode::CREATED, "{plan}");
    assert_eq!(entry(&plan, DB)["action"], "unknown", "{plan}");
    let (status, b) = s
        .post(
            &format!("{PLANS}/{}/permits", plan["id"].as_str().unwrap()),
            &s.admin,
            json!({"permits": [{"descriptor": DB, "action": "unknown"}]}),
        )
        .await;
    assert_error(status, &b, StatusCode::FORBIDDEN, "AUTH-002");
    assert!(b["message"].as_str().unwrap().contains("protected"), "{b}");
}

/// `test/planned`: `openporch/echo`, but its plans are what the test says
/// and it records the permit fingerprint of each `put`.
#[derive(Debug)]
struct Planned {
    echo: openporch_drivers::Echo,
    info: openporch_drivers::DriverInfo,
    plan: std::sync::Mutex<openporch_drivers::Plan>,
    puts: std::sync::Mutex<Vec<Option<String>>>,
}

impl openporch_drivers::Driver for Planned {
    fn info(&self) -> &openporch_drivers::DriverInfo {
        &self.info
    }

    fn plan<'a>(
        &'a self,
        _req: &'a openporch_drivers::DriverRequest,
    ) -> openporch_drivers::DriverFuture<'a, openporch_drivers::Plan> {
        let plan = self.plan.lock().unwrap().clone();
        Box::pin(async move { Ok(plan) })
    }

    fn plan_delete<'a>(
        &'a self,
        req: &'a openporch_drivers::DriverRequest,
    ) -> openporch_drivers::DriverFuture<'a, openporch_drivers::Plan> {
        self.echo.plan_delete(req)
    }

    fn put<'a>(
        &'a self,
        req: &'a openporch_drivers::DriverRequest,
    ) -> openporch_drivers::DriverFuture<'a, openporch_drivers::PutOutcome> {
        self.puts.lock().unwrap().push(req.permit.clone());
        self.echo.put(req)
    }

    fn get<'a>(
        &'a self,
        req: &'a openporch_drivers::DriverRequest,
    ) -> openporch_drivers::DriverFuture<'a, openporch_drivers::Observation> {
        self.echo.get(req)
    }

    fn delete<'a>(
        &'a self,
        req: &'a openporch_drivers::DriverRequest,
    ) -> openporch_drivers::DriverFuture<'a, openporch_drivers::DeleteOutcome> {
        self.echo.delete(req)
    }
}

impl Planned {
    fn set(&self, plan: openporch_drivers::Plan) {
        *self.plan.lock().unwrap() = plan;
    }

    fn puts(&self) -> Vec<Option<String>> {
        self.puts.lock().unwrap().clone()
    }
}

/// A plan that replaces the database instance.
fn replacement(fingerprint: &str) -> openporch_drivers::Plan {
    openporch_drivers::Plan::Actions {
        actions: vec![(
            openporch_drivers::Action::Replace,
            "aws_db_instance.main".into(),
        )],
        fingerprint: fingerprint.into(),
    }
}

const DNS: &str = "dns.default#modules.web.externals.db";

/// A started server whose deployments run through the engine, with the
/// database on `test/planned`. The database's driver input `host` is the
/// output of a DNS resource its definition references, so the database's
/// driver can plan only once that DNS resource has finished in the same run.
/// `web` with its database has been deployed once.
async fn planned_server() -> (Server, Arc<Planned>) {
    use openporch_drivers::{Driver as _, DriverRegistry, Echo};
    let dir = tempfile::tempdir().unwrap();
    let mut config = crate::Config::new("acme", dir.path());
    config.images.pin_other_registries = false;
    let echo = Echo::new();
    let mut info = echo.info().clone();
    info.id = "test/planned".into();
    let driver = Arc::new(Planned {
        echo,
        info,
        plan: std::sync::Mutex::new(openporch_drivers::Plan::NoEffects),
        puts: std::sync::Mutex::default(),
    });
    let mut drivers = DriverRegistry::with_builtins();
    drivers.register(driver.clone() as Arc<_>).unwrap();
    let state = crate::state::start_with_drivers(
        config,
        None,
        Arc::new(openporch_engine::cloud::AwsSts::new()),
        Some(Arc::new(drivers)),
    )
    .await
    .unwrap();
    let s = super::server_from(dir, state);
    // A record, so definitions may name the driver; it runs in-process.
    sqlx::query(
        "INSERT INTO drivers (org_id, id, body, created_at) \
         VALUES ('acme', 'test/planned', '{}', 0)",
    )
    .execute(s.state.store.pool())
    .await
    .unwrap();
    s.app("shop").await;
    for (id, ty, driver, inputs) in [
        ("env", "base-env", "openporch/none", json!({})),
        ("workloads", "workload", "openporch/none", json!({})),
        (
            "names",
            "dns",
            "openporch/echo",
            json!({"values": {"host": "db.${context.env.id}.internal"}}),
        ),
        (
            "pg",
            "postgres",
            "test/planned",
            json!({"values": {"host": "${resources.dns.outputs.host}", "name": "shop",
                              "port": 5432},
                   "secret_refs": {"username": {"value": "shop"},
                                   "password": {"value": "change-me"}}}),
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
    let first = s.deploy_web("nginx:1", true).await;
    assert_eq!(first["status"], "succeeded", "{first}");
    assert_eq!(
        s.resource(DB).await["outputs"]["host"],
        "db.development.internal"
    );
    (s, driver)
}

impl Server {
    /// Redeploys the desired set with `permits` and waits for the outcome.
    async fn redeploy_with(&self, permits: &[String]) -> Value {
        let mut body = self.redeploy_body().await;
        body["expected_generation"] = self.dev_generation().await;
        body["permits"] = json!(permits);
        let (status, d) = self
            .post(&format!("{ENV_DEV}/deploys"), &self.admin, body)
            .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{d}");
        self.finished_at(&format!("{ENV_DEV}/deploys/{}", d["id"].as_str().unwrap()))
            .await
    }

    /// Whether permit `id` has been used.
    async fn permit_used(&self, id: &str) -> bool {
        let used: Option<i64> = sqlx::query_scalar("SELECT used_at FROM permits WHERE id = ?")
            .bind(id)
            .fetch_one(self.state.store.pool())
            .await
            .unwrap();
        used.is_some()
    }
}

/// `deployment-safety`: `Database replacement` and `Inputs become resolvable
/// later`. The preview can only say the database is updated, because its
/// driver plans once the DNS name it takes as input is provisioned. When the
/// run's plan replaces the database, the DNS name has already been updated
/// and stays tracked; the database is not changed, the deployment fails with
/// `approval_required` naming it, and the plan is recorded against that
/// deployment. The next preview shows it, and a permit for it authorizes a
/// new attempt that applies it.
#[tokio::test]
async fn a_replacement_found_while_running_needs_a_permit_for_that_plan() {
    use openporch_store::PermitStore as _;
    let (s, driver) = planned_server().await;
    let before = s.resource(DB).await;
    let (status, plan) = s.plan(s.redeploy_body().await).await;
    assert_eq!(status, StatusCode::CREATED, "{plan}");
    let db = entry(&plan, DB);
    assert_eq!(db["action"], "update", "{plan}");
    assert_eq!(db["known"], false, "{plan}");
    assert_eq!(db["permit_required"], false, "{plan}");
    // Only the ordinary update is permitted: nothing needs a permit.
    assert_eq!(
        s.permits_for(ENV_DEV, s.redeploy_body().await).await,
        Vec::<String>::new()
    );

    driver.set(replacement("fp-1"));
    let puts = driver.puts().len();
    let failed = s.redeploy_with(&[]).await;
    assert_eq!(failed["status"], "failed", "{failed}");
    let error = failed["error"].as_str().unwrap();
    assert!(error.contains("approval_required"), "{error}");
    assert!(error.contains(DB), "{error}");
    // Apply did not run.
    assert_eq!(driver.puts().len(), puts);
    let db = s.resource(DB).await;
    assert_eq!(db["guresid"], before["guresid"]);
    assert_eq!(
        db["usage"]["last_deploy_id"],
        before["usage"]["last_deploy_id"]
    );
    // The dependency that finished first stays tracked with this attempt.
    let dns = s.resource(DNS).await;
    assert_eq!(dns["status"], "provisioned", "{dns}");
    assert_eq!(dns["usage"]["last_deploy_id"], failed["id"], "{dns}");
    // The deployment's refused plan is recorded with it.
    let requests = s
        .state
        .store
        .approval_requests("acme", "shop", "development")
        .await
        .unwrap();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(requests[0].deployment_id, failed["id"].as_str().unwrap());
    assert_eq!(requests[0].descriptor, DB);
    assert_eq!(requests[0].action, "replace");
    assert_eq!(requests[0].fingerprint, "fp-1");
    assert_eq!(
        requests[0].actions,
        [("replace".to_owned(), "aws_db_instance.main".to_owned())]
    );

    // The next preview shows the replacement and its plan.
    let (status, plan) = s.plan(s.redeploy_body().await).await;
    assert_eq!(status, StatusCode::CREATED, "{plan}");
    let db = entry(&plan, DB);
    assert_eq!(db["action"], "replace", "{plan}");
    assert_eq!(db["permit_required"], true, "{plan}");
    assert_eq!(db["driver_plan"]["fingerprint"], "fp-1", "{plan}");
    assert_eq!(
        db["driver_plan"]["actions"],
        json!([["replace", "aws_db_instance.main"]])
    );

    // Explicitly authorized, the new attempt applies that plan.
    let permits = s.permits_for(ENV_DEV, s.redeploy_body().await).await;
    assert_eq!(permits.len(), 1, "{permits:?}");
    let done = s.redeploy_with(&permits).await;
    assert_eq!(done["status"], "succeeded", "{done}");
    assert_eq!(driver.puts().last().unwrap().as_deref(), Some("fp-1"));
    assert!(s.permit_used(&permits[0]).await);
}

/// `deployment-safety`: `Dependency output reveals replacement`. An attempt
/// whose driver could not tell what it would do gets an unknown-impact
/// permit; when the next attempt's plan, now able to see its input, is a
/// replacement, that permit is not consumed for it, and the resource waits
/// for a replacement permit for that plan.
#[tokio::test]
async fn an_unknown_impact_permit_does_not_cover_a_replacement() {
    let (s, driver) = planned_server().await;

    // The driver cannot tell what the change does.
    driver.set(openporch_drivers::Plan::Unknown);
    let failed = s.redeploy_with(&[]).await;
    assert_eq!(failed["status"], "failed", "{failed}");
    assert!(
        failed["error"]
            .as_str()
            .unwrap()
            .contains("approval_required"),
        "{failed}"
    );
    let (status, plan) = s.plan(s.redeploy_body().await).await;
    assert_eq!(status, StatusCode::CREATED, "{plan}");
    assert_eq!(entry(&plan, DB)["action"], "unknown", "{plan}");
    let unknown = s.permits_for(ENV_DEV, s.redeploy_body().await).await;
    assert_eq!(unknown.len(), 1, "{unknown:?}");

    // With its input now known, the driver plans a replacement.
    driver.set(replacement("fp-2"));
    let puts = driver.puts().len();
    let refused = s.redeploy_with(&unknown).await;
    assert_eq!(refused["status"], "failed", "{refused}");
    let error = refused["error"].as_str().unwrap();
    assert!(error.contains("approval_required"), "{error}");
    assert_eq!(driver.puts().len(), puts, "the replacement was applied");
    assert!(
        !s.permit_used(&unknown[0]).await,
        "the unknown-impact permit was consumed by a replacement"
    );

    // It waits for a permit for that replacement.
    let (status, plan) = s.plan(s.redeploy_body().await).await;
    assert_eq!(status, StatusCode::CREATED, "{plan}");
    let db = entry(&plan, DB);
    assert_eq!(db["action"], "replace", "{plan}");
    assert_eq!(db["driver_plan"]["fingerprint"], "fp-2", "{plan}");
    let replace = s.permits_for(ENV_DEV, s.redeploy_body().await).await;
    let done = s.redeploy_with(&replace).await;
    assert_eq!(done["status"], "succeeded", "{done}");
    assert_eq!(driver.puts().last().unwrap().as_deref(), Some("fp-2"));
}
