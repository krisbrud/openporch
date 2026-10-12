//! HTTP tests for triggering deployments from `deployments`: acceptance, stale and
//! concurrent requests, idempotent retries, roles, modes and redeploys. Running a
//! deployment's resources is verified with the engine.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use axum::http::{Method, StatusCode};
use openporch_store::Store;
use serde_json::{Value, json};
use tokio::sync::Semaphore;

use super::{Server, assert_error, server};
use crate::deploy::{self, Job, Runner};
use crate::state::AppState;

/// Holds each deployment in progress until the test releases it, then lets it
/// succeed.
#[derive(Debug)]
struct Held(Arc<Semaphore>);

impl Runner for Held {
    fn run(&self, state: AppState, job: Job) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        let gate = self.0.clone();
        Box::pin(async move {
            if let Ok(permit) = gate.acquire().await {
                permit.forget();
            }
            deploy::finish(&state, &job, None).await;
        })
    }
}

const DEPLOYS: &str = "/orgs/acme/apps/shop/envs/development/deploys";

impl Server {
    /// `shop` with definitions for `base-env` and default workloads, and deployments
    /// held until released.
    async fn deployable(&self) -> Arc<Semaphore> {
        self.app("shop").await;
        for (id, ty) in [("env", "base-env"), ("workloads", "workload")] {
            let (s, b) = self
                .post(
                    "/orgs/acme/resources/defs",
                    &self.admin,
                    json!({"id": id, "type": ty, "driver_type": "openporch/none",
                           "criteria": [{}]}),
                )
                .await;
            assert_eq!(s, StatusCode::CREATED, "{b}");
        }
        let gate = Arc::new(Semaphore::new(0));
        self.state.runners.set(Arc::new(Held(gate.clone())));
        gate
    }

    async fn delta(&self, image: &str) -> String {
        let (s, b) = self
            .post(
                "/orgs/acme/apps/shop/deltas",
                &self.admin,
                json!({"modules": {"add": {"web": {"spec": {"containers": {
                    "web": {"image": image}}}}}}}),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
        b["id"].as_str().unwrap().to_owned()
    }

    async fn generation(&self, env: &str) -> i64 {
        let (s, b) = self
            .get(&format!("/orgs/acme/apps/shop/envs/{env}"), &self.admin)
            .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        b["generation"].as_i64().unwrap()
    }

    async fn deploy(&self, token: &str, body: Value, key: Option<&str>) -> (StatusCode, Value) {
        let headers: Vec<(&str, &str)> = key.map(|k| ("idempotency-key", k)).into_iter().collect();
        self.call_with(Method::POST, DEPLOYS, Some(token), Some(body), &headers)
            .await
    }

    /// Waits until deployment `id` is no longer in progress.
    async fn finished(&self, id: &str) -> Value {
        for _ in 0..200 {
            let (s, d) = self.get(&format!("{DEPLOYS}/{id}"), &self.admin).await;
            assert_eq!(s, StatusCode::OK, "{d}");
            if d["status"] != "in_progress" {
                return d;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("deployment {id} did not finish");
    }
}

#[tokio::test]
async fn deploy_a_delta() {
    let s = server().await;
    let gate = s.deployable().await;
    let delta = s.delta("web:1").await;
    let generation = s.generation("development").await;
    let (status, d) = s
        .deploy(
            &s.admin,
            json!({"delta_id": delta, "expected_generation": generation, "comment": "first"}),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{d}");
    assert_eq!(d["status"], "in_progress");
    assert_eq!(d["delta_id"], delta.as_str());
    assert_eq!(d["comment"], "first");
    assert_eq!(d["generation"], generation + 1);
    let id = d["id"].as_str().unwrap().to_owned();

    let (_, env) = s
        .get("/orgs/acme/apps/shop/envs/development", &s.admin)
        .await;
    assert_eq!(env["desired_set_id"], d["set_id"]);
    assert_eq!(env["last_successful_set_id"], Value::Null);
    assert_eq!(env["generation"], generation + 1);
    let (_, frozen) = s
        .get(&format!("/orgs/acme/apps/shop/deltas/{delta}"), &s.admin)
        .await;
    assert_eq!(frozen["frozen"], true);
    let (_, set) = s
        .get(
            &format!(
                "/orgs/acme/apps/shop/sets/{}",
                d["set_id"].as_str().unwrap()
            ),
            &s.admin,
        )
        .await;
    assert_eq!(
        set["modules"]["web"]["spec"]["containers"]["web"]["image"],
        "web:1"
    );

    // The snapshot holds the graph and the definitions it matched.
    let snapshot = s
        .state
        .store
        .deployment_snapshot("acme", "shop", "development", &id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        snapshot["graph"]["nodes"]["workload.default#modules.web"].is_object(),
        "{snapshot}"
    );
    assert_eq!(snapshot["definitions"]["workloads"]["version"], 1);
    assert_eq!(snapshot["mode"], "full");

    // Success sets the last successful deployment and set.
    gate.add_permits(1);
    let done = s.finished(&id).await;
    assert_eq!(done["status"], "succeeded");
    let (_, env) = s
        .get("/orgs/acme/apps/shop/envs/development", &s.admin)
        .await;
    assert_eq!(env["last_successful_set_id"], d["set_id"]);
    assert_eq!(env["last_successful_deploy_id"], id.as_str());
    assert_eq!(env["convergence"], "converged");

    // Deltas apply to the desired set.
    let (_, patch) = s
        .post(
            "/orgs/acme/apps/shop/deltas",
            &s.admin,
            json!({"modules": {"update": {"web": [
                {"op": "replace", "path": "/spec/containers/web/image", "value": "web:2"}]}}}),
        )
        .await;
    let (status, second) = s
        .deploy(
            &s.admin,
            json!({"delta_id": patch["id"], "expected_generation": generation + 1}),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{second}");
    let (_, set) = s
        .get(
            &format!(
                "/orgs/acme/apps/shop/sets/{}",
                second["set_id"].as_str().unwrap()
            ),
            &s.admin,
        )
        .await;
    assert_eq!(
        set["modules"]["web"]["spec"]["containers"]["web"]["image"],
        "web:2"
    );

    // History is newest first and paginated.
    let (_, page) = s.get(&format!("{DEPLOYS}?limit=1"), &s.admin).await;
    assert_eq!(page["deployments"][0]["id"], second["id"]);
    let next = page["next"].as_i64().unwrap();
    let (_, page) = s
        .get(&format!("{DEPLOYS}?limit=1&before={next}"), &s.admin)
        .await;
    assert_eq!(page["deployments"][0]["id"], id.as_str());
    let (_, latest) = s.get(&format!("{DEPLOYS}/+0"), &s.admin).await;
    assert_eq!(latest["id"], second["id"]);
    let (_, earlier) = s.get(&format!("{DEPLOYS}/+1"), &s.admin).await;
    assert_eq!(earlier["id"], id.as_str());
    let (_, good) = s.get(&format!("{DEPLOYS}/last-successful"), &s.admin).await;
    assert_eq!(good["id"], id.as_str());
    let (status, b) = s.get(&format!("{DEPLOYS}/+2"), &s.admin).await;
    assert_error(status, &b, StatusCode::NOT_FOUND, "API-001");

    // Acceptance is audited.
    let (_, entries) = s
        .get(
            "/orgs/acme/audit-logs?path_prefix=/orgs/acme/apps/shop/envs/development/deploys",
            &s.admin,
        )
        .await;
    assert!(
        entries["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["action"] == "deployment.create"),
        "{entries}"
    );
}

#[tokio::test]
async fn stale_or_concurrent_requests_conflict() {
    let s = server().await;
    let gate = s.deployable().await;
    let generation = s.generation("development").await;
    let first = s.delta("web:1").await;
    let (status, d) = s
        .deploy(
            &s.admin,
            json!({"delta_id": first, "expected_generation": generation}),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{d}");
    let desired = d["set_id"].clone();

    // An old generation, and the current one while a deployment runs.
    let second = s.delta("web:2").await;
    for expected in [generation, generation + 1] {
        let (status, b) = s
            .deploy(
                &s.admin,
                json!({"delta_id": second, "expected_generation": expected}),
                None,
            )
            .await;
        assert_error(status, &b, StatusCode::CONFLICT, "API-004");
    }
    let (_, env) = s
        .get("/orgs/acme/apps/shop/envs/development", &s.admin)
        .await;
    assert_eq!(env["desired_set_id"], desired);
    assert_eq!(env["generation"], generation + 1);
    let (_, delta) = s
        .get(&format!("/orgs/acme/apps/shop/deltas/{second}"), &s.admin)
        .await;
    assert_eq!(delta["frozen"], false);

    // Once it finished, the current generation is accepted.
    gate.add_permits(1);
    s.finished(d["id"].as_str().unwrap()).await;
    let (status, b) = s
        .deploy(
            &s.admin,
            json!({"set_id": "0", "expected_generation": generation + 1}),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{b}");
}

#[tokio::test]
async fn a_lost_response_is_retried_with_the_same_key() {
    let s = server().await;
    let _gate = s.deployable().await;
    let generation = s.generation("development").await;
    let delta = s.delta("web:1").await;
    let body = json!({"delta_id": delta, "expected_generation": generation});
    let (status, first) = s.deploy(&s.admin, body.clone(), Some("ci-run-7")).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{first}");
    // The retry succeeds although acceptance advanced the generation and the
    // deployment still runs.
    let (status, again) = s.deploy(&s.admin, body, Some("ci-run-7")).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{again}");
    assert_eq!(again["id"], first["id"]);
    let (_, page) = s.get(DEPLOYS, &s.admin).await;
    assert_eq!(page["deployments"].as_array().unwrap().len(), 1);

    // Different content under the same key conflicts.
    let (status, b) = s
        .deploy(
            &s.admin,
            json!({"delta_id": delta, "expected_generation": generation, "comment": "x"}),
            Some("ci-run-7"),
        )
        .await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");
    let (status, b) = s
        .deploy(
            &s.admin,
            json!({"set_id": "0", "expected_generation": 0}),
            Some("bad key"),
        )
        .await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
}

#[tokio::test]
async fn deploying_needs_the_deployer_role() {
    let s = server().await;
    let _gate = s.deployable().await;
    s.env_type("production").await;
    let (status, b) = s
        .post(
            "/orgs/acme/apps/shop/envs",
            &s.admin,
            json!({"id": "production", "type": "production"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{b}");
    let viewer = s.member("viewer", "member").await;
    s.grant_app("shop", "viewer", "viewer").await;
    let dev = s.member("dev", "member").await;
    s.grant_app("shop", "dev", "developer").await;
    let body = json!({"set_id": "0", "expected_generation": 0});
    let path = "/orgs/acme/apps/shop/envs/production/deploys";

    for token in [&viewer, &dev] {
        let (status, b) = s.post(path, token, body.clone()).await;
        assert_error(status, &b, StatusCode::FORBIDDEN, "AUTH-002");
    }
    let (_, page) = s.get(path, &s.admin).await;
    assert_eq!(page["deployments"], json!([]));
    // Viewers can read the history.
    let (status, _) = s.get(path, &viewer).await;
    assert_eq!(status, StatusCode::OK);

    s.grant_deployer("production", "dev").await;
    let (status, b) = s.post(path, &dev, body).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{b}");
    assert_eq!(b["actor"], "dev");
}

#[tokio::test]
async fn requests_are_checked_before_acceptance() {
    let s = server().await;
    let _gate = s.deployable().await;
    for (body, needle) in [
        (
            json!({"set_id": "0", "expected_generation": 0, "mode": "incremental"}),
            "incremental",
        ),
        (
            json!({"set_id": "0", "delta_id": "x", "expected_generation": 0}),
            "exactly one",
        ),
        (json!({"expected_generation": 0}), "exactly one"),
        (
            json!({"set_id": "0", "replay": "current", "expected_generation": 0}),
            "replay",
        ),
        (json!({"set_id": "0"}), "expected_generation"),
        (
            json!({"set_id": "0", "expected_generation": 0, "mode": "fast"}),
            "mode",
        ),
    ] {
        let (status, b) = s.deploy(&s.admin, body.clone(), None).await;
        assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
        assert!(
            b["message"].as_str().unwrap().contains(needle),
            "{body}: {b}"
        );
    }

    // A set whose graph cannot be built is rejected without being accepted.
    let (status, b) = s
        .post(
            "/orgs/acme/apps/shop/deltas",
            &s.admin,
            json!({"modules": {"add": {"web": {"externals": {"db": {"type": "postgres"}}}}}}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{b}");
    let (status, b) = s
        .deploy(
            &s.admin,
            json!({"delta_id": b["id"], "expected_generation": 0}),
            None,
        )
        .await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
    assert!(b["message"].as_str().unwrap().contains("postgres"), "{b}");
    assert_eq!(s.generation("development").await, 0);

    let (status, b) = s
        .deploy(
            &s.admin,
            json!({"deploy_id": "last-successful", "expected_generation": 0}),
            None,
        )
        .await;
    assert_error(status, &b, StatusCode::NOT_FOUND, "API-001");
}

#[tokio::test]
async fn redeploys_replay_the_recorded_snapshot_unless_asked_for_current() {
    let s = server().await;
    let gate = s.deployable().await;
    let delta = s.delta("web:1").await;
    let (_, first) = s
        .deploy(
            &s.admin,
            json!({"delta_id": delta, "expected_generation": 0}),
            None,
        )
        .await;
    let first_id = first["id"].as_str().unwrap().to_owned();
    gate.add_permits(1);
    s.finished(&first_id).await;

    // The workload definition changes after the deployment.
    let (status, b) = s
        .call(
            Method::PUT,
            "/orgs/acme/resources/defs/workloads",
            Some(&s.admin),
            Some(json!({"name": "changed", "driver_type": "openporch/echo"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{b}");

    let snapshot = |id: String| {
        let store = s.state.store.clone();
        async move {
            store
                .deployment_snapshot("acme", "shop", "development", &id)
                .await
                .unwrap()
                .unwrap()
        }
    };
    let (status, replay) = s
        .deploy(
            &s.admin,
            json!({"deploy_id": "last-successful", "expected_generation": 1}),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{replay}");
    assert_eq!(replay["set_id"], first["set_id"]);
    assert_eq!(replay["delta_id"], Value::Null);
    let replayed = snapshot(replay["id"].as_str().unwrap().to_owned()).await;
    assert_eq!(replayed["graph"], snapshot(first_id.clone()).await["graph"]);
    assert_eq!(replayed["replayed_from"], first_id.as_str());
    assert_eq!(
        replayed["definitions"]["workloads"]["definition"]["driver_type"],
        "openporch/none"
    );
    gate.add_permits(1);
    s.finished(replay["id"].as_str().unwrap()).await;

    let (status, current) = s
        .deploy(
            &s.admin,
            json!({"deploy_id": "+1", "replay": "current", "expected_generation": 2}),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{current}");
    assert_eq!(current["set_id"], first["set_id"]);
    let current_id = current["id"].as_str().unwrap().to_owned();
    let current = snapshot(current_id.clone()).await;
    assert_eq!(
        current["definitions"]["workloads"]["definition"]["driver_type"],
        "openporch/echo"
    );
    assert_eq!(current["definitions"]["workloads"]["version"], 2);
    gate.add_permits(1);

    // A snapshot is not replayed in another environment.
    s.env_type("staging").await;
    s.post(
        "/orgs/acme/apps/shop/envs",
        &s.admin,
        json!({"id": "staging", "type": "staging"}),
    )
    .await;
    let path = "/orgs/acme/apps/shop/envs/staging/deploys";
    let (status, b) = s
        .post(
            path,
            &s.admin,
            json!({"deploy_id": first_id, "expected_generation": 0}),
        )
        .await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
    let (status, b) = s
        .post(
            path,
            &s.admin,
            json!({"deploy_id": first_id, "replay": "current", "expected_generation": 0}),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{b}");
    assert_eq!(b["set_id"], first["set_id"]);

    // A snapshot whose definition was deleted is not replayed.
    let (status, b) = s
        .delete("/orgs/acme/resources/defs/workloads", &s.admin)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{b}");
    s.finished(&current_id).await;
    let (status, b) = s
        .deploy(
            &s.admin,
            json!({"deploy_id": first_id, "expected_generation": 3}),
            None,
        )
        .await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
    assert!(
        b["message"].as_str().unwrap().contains("no longer exists"),
        "{b}"
    );
}

#[tokio::test]
async fn without_the_engine_deployments_fail_and_free_the_environment() {
    let s = server().await;
    s.deployable().await;
    s.state.runners.set(Arc::new(deploy::Unimplemented));
    let (status, d) = s
        .deploy(
            &s.admin,
            json!({"set_id": "0", "expected_generation": 0}),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{d}");
    let done = s.finished(d["id"].as_str().unwrap()).await;
    assert_eq!(done["status"], "failed");
    assert!(
        done["error"].as_str().unwrap().contains("not implemented"),
        "{done}"
    );
    let (_, env) = s
        .get("/orgs/acme/apps/shop/envs/development", &s.admin)
        .await;
    assert_eq!(env["convergence"], "partial");
    assert_eq!(env["last_successful_set_id"], Value::Null);
}

#[tokio::test]
async fn concurrent_requests_accept_one() {
    let s = server().await;
    let _gate = s.deployable().await;
    let a = s.delta("web:1").await;
    let b = s.delta("web:2").await;
    let (x, y) = tokio::join!(
        s.deploy(
            &s.admin,
            json!({"delta_id": a, "expected_generation": 0}),
            None
        ),
        s.deploy(
            &s.admin,
            json!({"delta_id": b, "expected_generation": 0}),
            None
        ),
    );
    let mut statuses = [x.0, y.0];
    statuses.sort();
    assert_eq!(
        statuses,
        [StatusCode::ACCEPTED, StatusCode::CONFLICT],
        "{x:?} {y:?}"
    );
    let (_, page) = s.get(DEPLOYS, &s.admin).await;
    assert_eq!(page["deployments"].as_array().unwrap().len(), 1);
    // Only the accepted delta is frozen.
    let mut frozen = Vec::new();
    for id in [a, b] {
        let (_, d) = s
            .get(&format!("/orgs/acme/apps/shop/deltas/{id}"), &s.admin)
            .await;
        frozen.push(d["frozen"].clone());
    }
    let (fa, fb) = (&frozen[0], &frozen[1]);
    assert_ne!(fa, fb);
}

#[tokio::test]
async fn retries_and_references_respect_access() {
    let s = server().await;
    let _gate = s.deployable().await;
    let dev = s.member("dev", "member").await;
    s.grant_app("shop", "dev", "developer").await;
    s.grant_deployer("development", "dev").await;
    let body = json!({"set_id": "0", "expected_generation": 0});
    let (status, d) = s.deploy(&dev, body.clone(), Some("k1")).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{d}");
    // Another principal's key is its own.
    let (status, b) = s.deploy(&s.admin, body.clone(), Some("k1")).await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");

    // A retry after the deployer role is gone does not reveal the deployment.
    let (status, b) = s
        .delete("/orgs/acme/env-types/development/users/dev", &s.admin)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{b}");
    let (status, b) = s.deploy(&dev, body, Some("k1")).await;
    assert_error(status, &b, StatusCode::FORBIDDEN, "AUTH-002");

    // Deployments of another application are not found.
    s.app("blog").await;
    let (status, b) = s
        .post(
            "/orgs/acme/apps/blog/envs/development/deploys",
            &s.admin,
            json!({"deploy_id": d["id"], "replay": "current", "expected_generation": 0}),
        )
        .await;
    assert_error(status, &b, StatusCode::NOT_FOUND, "API-001");
    let (status, b) = s
        .get(
            &format!(
                "/orgs/acme/apps/blog/envs/development/deploys/{}",
                d["id"].as_str().unwrap()
            ),
            &s.admin,
        )
        .await;
    assert_error(status, &b, StatusCode::NOT_FOUND, "API-001");
}

#[tokio::test]
async fn a_delta_changed_after_it_was_read_is_not_frozen() {
    let s = server().await;
    let _gate = s.deployable().await;
    let delta = s.delta("web:1").await;
    let set = s
        .state
        .store
        .put_set("acme", "shop", &Default::default())
        .await
        .unwrap();
    let path = "/orgs/acme/apps/shop/envs/development/deploys";
    let err = s
        .state
        .store
        .accept_deployment(openporch_store::DeploymentRequest {
            permits: Default::default(),
            org: "acme".into(),
            app: "shop".into(),
            env: "development".into(),
            actor: "bootstrap".into(),
            expected_generation: 0,
            idempotency_key: None,
            fingerprint: "body".into(),
            kind: openporch_store::DeploymentKind::Deploy,
            set_id: Some(set),
            delta_id: Some(delta.clone()),
            delta_content: Some(json!({"modules": {"remove": ["web"]}})),
            comment: String::new(),
            snapshot: json!({}),
            audit: openporch_store::AuditIntent::accepted(
                "acme",
                "bootstrap",
                "POST",
                path,
                "deploy",
            ),
        })
        .await
        .unwrap_err();
    assert!(err.to_string().contains("changed"), "{err}");
    let (_, d) = s
        .get(&format!("/orgs/acme/apps/shop/deltas/{delta}"), &s.admin)
        .await;
    assert_eq!(d["frozen"], false);
    assert_eq!(s.generation("development").await, 0);
}

#[tokio::test]
async fn a_started_server_provisions_deployments() {
    let s = server().await;
    s.app("shop").await;
    for (id, ty, driver, inputs) in [
        ("env", "base-env", "openporch/none", json!({})),
        ("workloads", "workload", "openporch/none", json!({})),
        (
            "pg",
            "postgres",
            "openporch/echo",
            json!({"values": {"host": "db.internal"}}),
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
    // The OpenTofu driver is registered, with the configured state backend.
    let tofu = s.state.drivers.facts("openporch/opentofu").unwrap();
    assert_eq!(tofu.backend.as_deref(), Some("local state"));

    let (st, delta) = s
        .post(
            "/orgs/acme/apps/shop/deltas",
            &s.admin,
            json!({"modules": {"add": {"web": {"externals": {"db": {"type": "postgres"}},
                "spec": {"containers": {"web": {"image": "web:1"}}}}}}}),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{delta}");
    let (st, d) = s
        .deploy(
            &s.admin,
            json!({"delta_id": delta["id"], "expected_generation": 0}),
            None,
        )
        .await;
    assert_eq!(st, StatusCode::ACCEPTED, "{d}");
    let done = s.finished(d["id"].as_str().unwrap()).await;
    assert_eq!(done["status"], "succeeded", "{done}");

    let (_, env) = s
        .get("/orgs/acme/apps/shop/envs/development", &s.admin)
        .await;
    assert_eq!(env["last_successful_set_id"], d["set_id"]);
    let (st, db) = s
        .get(
            "/orgs/acme/apps/shop/envs/development/resources/postgres/modules.web.externals.db",
            &s.admin,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{db}");
    assert_eq!(db["outputs"]["host"], "db.internal");
    assert_eq!(db["status"], "provisioned", "{db}");
}
