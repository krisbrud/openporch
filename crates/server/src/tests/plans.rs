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
