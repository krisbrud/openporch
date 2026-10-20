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

use super::{Server, assert_error, server, server_in};
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

/// Fails each deployment as a cloud operation would.
#[derive(Debug)]
struct Failing;

impl Runner for Failing {
    fn run(&self, state: AppState, job: Job) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(async move {
            deploy::finish(&state, &job, Some("the cloud refused the change".into())).await;
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
    // The developer is told which role is missing.
    let (status, b) = s.post(path, &dev, body.clone()).await;
    assert_error(status, &b, StatusCode::FORBIDDEN, "AUTH-002");
    assert!(b["message"].as_str().unwrap().contains("`deployer`"), "{b}");
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
async fn snapshots_without_recorded_types_are_not_replayed() {
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
    // A snapshot written before types were recorded would replay without
    // checking any driver's outputs, so it is refused.
    sqlx::query(
        "UPDATE deployments SET snapshot = json_set(json_remove(snapshot, '$.types'), \
         '$.version', 4) WHERE id = ?",
    )
    .bind(&first_id)
    .execute(s.state.store.pool())
    .await
    .unwrap();
    let (status, b) = s
        .deploy(
            &s.admin,
            json!({"deploy_id": first_id, "expected_generation": 1}),
            None,
        )
        .await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
    assert!(b.to_string().contains("no replayable snapshot"), "{b}");
    let (status, b) = s
        .deploy(
            &s.admin,
            json!({"deploy_id": first_id, "replay": "current", "expected_generation": 1}),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{b}");
    gate.add_permits(1);
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
async fn without_an_executor_deployments_are_refused() {
    let s = server().await;
    s.deployable().await;
    s.state.runners.set(Arc::new(deploy::Unimplemented));
    let (status, ready) = s.call(Method::GET, "/readyz", None, None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{ready}");
    assert_eq!(ready["reasons"], json!(["executor_unavailable"]));
    let (status, d) = s
        .deploy(
            &s.admin,
            json!({"set_id": "0", "expected_generation": 0}),
            None,
        )
        .await;
    assert_error(status, &d, StatusCode::SERVICE_UNAVAILABLE, "SRV-002");
    assert!(
        d["message"]
            .as_str()
            .unwrap()
            .contains("executor_unavailable"),
        "{d}"
    );
    // Nothing was accepted, and reads still work.
    let (status, page) = s.get(DEPLOYS, &s.admin).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["deployments"], json!([]), "{page}");
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

const DB_PATH: &str =
    "/orgs/acme/apps/shop/envs/development/resources/postgres/modules.web.externals.db";

/// Deploys a postgres database whose `driver` returns no `host`, which must
/// fail naming it without leaking the password, and returns the database.
async fn deploy_without_host(s: &Server, driver: &str) -> Value {
    s.app("shop").await;
    for (id, ty, driver, inputs) in [
        ("env", "base-env", "openporch/none", json!({})),
        ("workloads", "workload", "openporch/none", json!({})),
        (
            "pg",
            "postgres",
            driver,
            json!({"values": {"name": "shop", "port": 5432},
                   "secret_refs": {"username": {"value": "shop"},
                                   "password": {"value": "hunter2-secret"}}}),
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
    assert_eq!(done["status"], "failed", "{done}");
    let error = done["error"].as_str().unwrap();
    assert!(
        error.contains("output `host` of resource type `postgres` is missing"),
        "{error}"
    );
    assert!(!done.to_string().contains("hunter2"), "{done}");
    // The workload that would have read `host` never ran.
    let (_, workload) = s
        .get(
            "/orgs/acme/apps/shop/envs/development/resources/workload/modules.web",
            &s.admin,
        )
        .await;
    assert_ne!(workload["status"], "provisioned", "{workload}");
    s.get(DB_PATH, &s.admin).await.1
}

/// `resource-catalog`: a driver's outputs that miss what the resource type
/// requires fail the resource, naming the output, before anything uses them.
#[tokio::test]
async fn a_missing_required_output_fails_the_resource() {
    let s = server().await;
    // `openporch/echo` plans no changes, so nothing exists to recover.
    let db = deploy_without_host(&s, "openporch/echo").await;
    assert_eq!(db["status"], "failed", "{db}");
}

/// `test/changing`: `openporch/echo`, but it cannot preview, so its calls may
/// change infrastructure.
#[derive(Debug)]
struct Changing(openporch_drivers::Echo, openporch_drivers::DriverInfo);

impl openporch_drivers::Driver for Changing {
    fn info(&self) -> &openporch_drivers::DriverInfo {
        &self.1
    }

    fn plan<'a>(
        &'a self,
        _req: &'a openporch_drivers::DriverRequest,
    ) -> openporch_drivers::DriverFuture<'a, openporch_drivers::Plan> {
        Box::pin(async { Ok(openporch_drivers::Plan::Unknown) })
    }

    fn plan_delete<'a>(
        &'a self,
        req: &'a openporch_drivers::DriverRequest,
    ) -> openporch_drivers::DriverFuture<'a, openporch_drivers::Plan> {
        self.0.plan_delete(req)
    }

    fn put<'a>(
        &'a self,
        req: &'a openporch_drivers::DriverRequest,
    ) -> openporch_drivers::DriverFuture<'a, openporch_drivers::PutOutcome> {
        self.0.put(req)
    }

    fn get<'a>(
        &'a self,
        req: &'a openporch_drivers::DriverRequest,
    ) -> openporch_drivers::DriverFuture<'a, openporch_drivers::Observation> {
        self.0.get(req)
    }

    fn delete<'a>(
        &'a self,
        req: &'a openporch_drivers::DriverRequest,
    ) -> openporch_drivers::DriverFuture<'a, openporch_drivers::DeleteOutcome> {
        self.0.delete(req)
    }
}

/// A driver that may have changed infrastructure leaves outputs its type does
/// not allow for recovery to observe, and recovery leaves them to an
/// administrator rather than recording them.
#[tokio::test]
async fn invalid_outputs_after_a_change_are_left_for_recovery() {
    use openporch_drivers::{Driver, DriverRegistry, Echo};
    let dir = tempfile::tempdir().unwrap();
    let mut config = crate::Config::new("acme", dir.path());
    config.images.pin_other_registries = false;
    let echo = Echo::new();
    let mut info = echo.info().clone();
    info.id = "test/changing".into();
    let mut drivers = DriverRegistry::with_builtins();
    drivers
        .register(Arc::new(Changing(echo, info)) as Arc<_>)
        .unwrap();
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
         VALUES ('acme', 'test/changing', '{}', 0)",
    )
    .execute(s.state.store.pool())
    .await
    .unwrap();
    let db = deploy_without_host(&s, "test/changing").await;
    assert_eq!(db["status"], "unknown", "{db}");
    let (st, recovered) = s
        .post(&format!("{DB_PATH}/recover"), &s.admin, json!({}))
        .await;
    assert_eq!(st, StatusCode::OK, "{recovered}");
    assert_eq!(recovered["outcome"], "recovery_required", "{recovered}");
    assert!(
        recovered["reason"]
            .as_str()
            .unwrap()
            .contains("output `host` of resource type `postgres` is missing"),
        "{recovered}"
    );
    assert!(!recovered.to_string().contains("hunter2"), "{recovered}");
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
            json!({"values": {"host": "db.internal", "name": "shop", "port": 5432},
                   "secret_refs": {"username": {"value": "shop"}, "password": {"value": "change-me"}}}),
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

    // Removing the workload deletes it behind a permit; the database it used
    // stays until it is scheduled for deletion.
    let env_path = "/orgs/acme/apps/shop/envs/development";
    let (st, removal) = s
        .post(
            "/orgs/acme/apps/shop/deltas",
            &s.admin,
            json!({"modules": {"remove": ["web"]}}),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{removal}");
    let generation = s.generation("development").await;
    let body = json!({"delta_id": removal["id"], "expected_generation": generation});
    let (st, b) = s.deploy(&s.admin, body.clone(), None).await;
    assert_error(st, &b, StatusCode::CONFLICT, "DPL-001");
    let permits = s
        .permits_for(env_path, json!({"delta_id": removal["id"]}))
        .await;
    assert!(!permits.is_empty());
    let mut body = body;
    body["permits"] = json!(permits);
    let (st, d) = s.deploy(&s.admin, body, None).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{d}");
    let done = s.finished(d["id"].as_str().unwrap()).await;
    assert_eq!(done["status"], "succeeded", "{done}");
    let (_, left) = s.get(&format!("{env_path}/resources"), &s.admin).await;
    let left: Vec<&str> = left["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["descriptor"].as_str().unwrap())
        .collect();
    assert_eq!(
        left,
        [
            "base-env.default#base-env",
            "postgres.default#modules.web.externals.db"
        ]
    );
}

#[tokio::test]
async fn a_restart_observes_resources_left_with_an_unknown_outcome() {
    let s = server().await;
    s.app("shop").await;
    for (id, ty, driver) in [
        ("env", "base-env", "openporch/none"),
        ("workloads", "workload", "openporch/none"),
        ("pg", "postgres", "openporch/echo"),
    ] {
        let (st, b) = s
            .post(
                "/orgs/acme/resources/defs",
                &s.admin,
                json!({"id": id, "type": ty, "driver_type": driver,
                       "driver_inputs": {"values": {"host": "db.internal", "name": "shop", "port": 5432},
                                         "secret_refs": {"username": {"value": "shop"}, "password": {"value": "change-me"}}},
                       "criteria": [{}]}),
            )
            .await;
        assert_eq!(st, StatusCode::CREATED, "{b}");
    }
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
    let db_path =
        "/orgs/acme/apps/shop/envs/development/resources/postgres/modules.web.externals.db";
    let (_, db) = s.get(db_path, &s.admin).await;

    // The server stops while the database's operation is open. The operation
    // is made an update of a resource that existed before it.
    sqlx::query(
        "UPDATE operations SET state = 'started', finished_at = NULL, \
         recovery = json_set(recovery, '$.existing', json('true')) WHERE guresid = ?",
    )
    .bind(db["guresid"].as_str().unwrap())
    .execute(s.state.store.pool())
    .await
    .unwrap();
    let Server {
        dir, state, router, ..
    } = s;
    state.store.pool().close().await;
    drop((state, router));

    // The restart marks it unknown, then observes it. It exists, which says
    // nothing about whether the update applied, and its driver can repeat
    // it safely, so the next deployment does.
    let s = server_in(dir).await;
    let mut status = Value::Null;
    for _ in 0..200 {
        let (st, db) = s.get(db_path, &s.admin).await;
        assert_eq!(st, StatusCode::OK, "{db}");
        status = db["status"].clone();
        if status == "failed" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(status, "failed");
    let (_, audit) = s.get("/orgs/acme/audit-logs", &s.admin).await;
    assert!(audit.to_string().contains("resource.recover"), "{audit}");
}

#[tokio::test]
async fn cross_application_references_are_hidden() {
    let s = server().await;
    let gate = s.deployable().await;
    let delta = s.delta("web:secret-tag").await;
    let (status, d) = s
        .deploy(
            &s.admin,
            json!({"delta_id": delta, "expected_generation": 0}),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{d}");
    gate.add_permits(1);
    let source = s.finished(d["id"].as_str().unwrap()).await;
    assert_eq!(source["status"], "succeeded", "{source}");

    // A developer and deployer on `blog` who cannot see `shop`.
    s.app("blog").await;
    let dev = s.member("dev", "member").await;
    s.grant_app("blog", "dev", "developer").await;
    s.grant_deployer("development", "dev").await;
    // The developer can deploy `blog` itself.
    let blog = "/orgs/acme/apps/blog/envs/development/deploys";
    let (status, own) = s
        .post(blog, &dev, json!({"set_id": "0", "expected_generation": 0}))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{own}");
    gate.add_permits(1);
    let own = own["id"].as_str().unwrap();
    for _ in 0..200 {
        let (_, d) = s.get(&format!("{blog}/{own}"), &s.admin).await;
        if d["status"] != "in_progress" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let (_, env) = s
        .get("/orgs/acme/apps/blog/envs/development", &s.admin)
        .await;
    let generation = env["generation"].clone();
    let id = d["id"].as_str().unwrap();
    let set = source["set_id"].as_str().unwrap();
    for replay in ["current", "snapshot"] {
        let (status, b) = s
            .post(
                blog,
                &dev,
                json!({"deploy_id": id, "replay": replay, "expected_generation": generation}),
            )
            .await;
        assert_error(status, &b, StatusCode::NOT_FOUND, "API-001");
        let text = b.to_string();
        assert!(
            !text.contains(set) && !text.contains("secret-tag"),
            "{text}"
        );
    }
    let (status, b) = s
        .post(
            "/orgs/acme/apps/blog/envs",
            &dev,
            json!({"id": "copy", "type": "development", "from_deploy_id": id}),
        )
        .await;
    assert_error(status, &b, StatusCode::NOT_FOUND, "API-001");
    assert!(!b.to_string().contains(set), "{b}");
    // Nothing was deployed or created.
    let (_, page) = s.get(blog, &s.admin).await;
    assert_eq!(page["deployments"].as_array().unwrap().len(), 1, "{page}");
    assert_eq!(
        s.get("/orgs/acme/apps/blog/envs/copy", &s.admin).await.0,
        StatusCode::NOT_FOUND
    );
    // Nor can the developer read the source deployment directly.
    let (status, b) = s.get(&format!("{DEPLOYS}/{id}"), &dev).await;
    assert_error(status, &b, StatusCode::NOT_FOUND, "API-001");
}

#[tokio::test]
async fn deployments_are_recorded_with_their_correlated_outcome() {
    let s = server().await;
    s.deployable().await;
    let ci = s.member("ci", "member").await;
    s.grant_app("shop", "ci", "developer").await;
    s.grant_deployer("development", "ci").await;
    // The request is accepted, but its cloud operation fails.
    s.state.runners.set(Arc::new(Failing));
    let (status, d) = s
        .deploy(&ci, json!({"set_id": "0", "expected_generation": 0}), None)
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{d}");
    let id = d["id"].as_str().unwrap();
    assert_eq!(s.finished(id).await["status"], "failed");

    let (_, page) = s
        .get(
            &format!("/orgs/acme/audit-logs?user_id=ci&path_prefix={DEPLOYS}"),
            &s.admin,
        )
        .await;
    let entries = page["entries"].as_array().unwrap();
    let accepted: Vec<_> = entries
        .iter()
        .filter(|e| e["outcome"] == "accepted")
        .collect();
    assert_eq!(accepted.len(), 1, "{page}");
    let accepted = accepted[0];
    assert_eq!(accepted["user_id"], "ci");
    assert_eq!(accepted["method"], "POST");
    assert_eq!(accepted["path"], DEPLOYS);
    assert_eq!(accepted["operation_id"], id);
    assert_eq!(accepted["action"], "deployment.create");

    let failed: Vec<_> = entries
        .iter()
        .filter(|e| e["outcome"] == "failed")
        .collect();
    assert_eq!(failed.len(), 1, "{page}");
    let failed = failed[0];
    assert_eq!(failed["user_id"], "ci");
    assert_eq!(failed["action"], "deployment.finish");
    assert_eq!(failed["operation_id"], id);
    assert_eq!(failed["correlates"], accepted["id"]);
    assert_eq!(failed["reason"], "deployment_failed");
    // Both entries keep their IDs after later writes, on a later page.
    s.app("later").await;
    let (_, newest) = s.get("/orgs/acme/audit-logs?limit=1", &s.admin).await;
    let before = newest["next"].as_i64().unwrap();
    let (_, again) = s
        .get(
            &format!("/orgs/acme/audit-logs?user_id=ci&before={before}"),
            &s.admin,
        )
        .await;
    for entry in [accepted, failed] {
        assert!(
            again["entries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e == entry),
            "{again}"
        );
    }
}

// --- workload deltas --------------------------------------------------------------

const WEB_WORKLOAD: &str = r#"
apiVersion: openporch.dev/v1
kind: Workload
metadata: {name: web}
containers:
  web: {image: ., variables: {PAGE_SIZE: "10"}}
"#;

impl Server {
    /// Drafts a delta for `development` from a workload request.
    async fn workload_delta(&self, body: Value) -> (StatusCode, Value) {
        self.post("/orgs/acme/apps/shop/deltas", &self.admin, body)
            .await
    }

    /// Deploys a drafted delta at its base generation and waits for it.
    async fn deploy_drafted(&self, gate: &Semaphore, delta: &Value) -> Value {
        let (status, d) = self
            .deploy(
                &self.admin,
                json!({"delta_id": delta["id"], "expected_generation": delta["base_generation"]}),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{d}");
        gate.add_permits(1);
        self.finished(d["id"].as_str().unwrap()).await
    }
}

fn web_request(image: &str) -> Value {
    json!({"env": "development", "workload": WEB_WORKLOAD,
           "overrides": {"images": {"web": image}}})
}

/// `Add to an empty environment` and `Redeploy unchanged workload` in
/// `deployment-sets-and-deltas`, through workload conversion requests.
#[tokio::test]
async fn workload_document_becomes_a_delta_against_the_desired_set() {
    let s = server().await;
    let gate = s.deployable().await;
    let generation = s.generation("development").await;

    let (status, first) = s.workload_delta(web_request("web:1")).await;
    assert_eq!(status, StatusCode::CREATED, "{first}");
    assert_eq!(first["env"], "development");
    assert_eq!(first["base_generation"], generation);
    assert_eq!(
        first["modules"]["add"]["web"]["spec"]["containers"]["web"]["image"],
        "web:1"
    );
    let d1 = s.deploy_drafted(&gate, &first).await;
    let (_, set) = s
        .get(
            &format!(
                "/orgs/acme/apps/shop/sets/{}",
                d1["set_id"].as_str().unwrap()
            ),
            &s.admin,
        )
        .await;
    assert_eq!(set["modules"].as_object().unwrap().len(), 1);

    // The same document again is an update that changes nothing.
    let (status, second) = s.workload_delta(web_request("web:1")).await;
    assert_eq!(status, StatusCode::CREATED, "{second}");
    assert_eq!(second["modules"]["update"]["web"], json!([]));
    assert_eq!(second["base_generation"], generation + 1);
    let d2 = s.deploy_drafted(&gate, &second).await;
    assert_eq!(d2["set_id"], d1["set_id"]);

    // A changed image is a patch of the deployed workload.
    let (_, third) = s.workload_delta(web_request("web:2")).await;
    assert_eq!(
        third["modules"]["update"]["web"],
        json!([{"op": "replace", "path": "/spec/containers/web/image", "value": "web:2"}])
    );
}

/// A drafted delta cannot overwrite a change made after it was drafted.
#[tokio::test]
async fn drafted_delta_is_stale_after_another_deployment() {
    let s = server().await;
    let gate = s.deployable().await;
    let (_, stale) = s.workload_delta(web_request("web:1")).await;
    let (_, other) = s.workload_delta(web_request("web:2")).await;
    s.deploy_drafted(&gate, &other).await;

    // Even at the environment's current generation, the delta's base decides.
    let current = s.generation("development").await;
    let (status, body) = s
        .deploy(
            &s.admin,
            json!({"delta_id": stale["id"], "expected_generation": current}),
            None,
        )
        .await;
    assert_error(status, &body, StatusCode::CONFLICT, "API-004");
    assert!(
        body["message"].as_str().unwrap().contains("generation"),
        "{body}"
    );
}

#[tokio::test]
async fn workload_requests_are_checked() {
    let s = server().await;
    s.deployable().await;
    // A value left for deploy time must be supplied.
    let (status, body) = s
        .workload_delta(json!({"env": "development", "workload": WEB_WORKLOAD}))
        .await;
    assert_error(status, &body, StatusCode::BAD_REQUEST, "API-003");
    assert!(
        body["message"].as_str().unwrap().contains("--image web="),
        "{body}"
    );

    let mut unknown = web_request("web:1");
    unknown["env"] = json!("nowhere");
    let (status, _) = s.workload_delta(unknown).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let mut extra = web_request("web:1");
    extra["shared"] = json!([]);
    let (status, body) = s.workload_delta(extra).await;
    assert_error(status, &body, StatusCode::BAD_REQUEST, "API-003");

    // A Score file takes an overlay with the class.
    let score = r#"
apiVersion: score.dev/v1b1
metadata: {name: web}
containers: {web: {image: nginx}}
"#;
    let overlay = "apiVersion: openporch.dev/v1\nkind: WorkloadOverlay\nmetadata: {class: edge}\n";
    let (status, body) = s
        .workload_delta(json!({"env": "development", "workload": score, "overlay": overlay}))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["modules"]["add"]["web"]["class"], "edge");
}

#[tokio::test]
async fn shutdown_refuses_changes_and_waits_for_running_deployments() {
    let s = server().await;
    let gate = s.deployable().await;
    let (status, d) = s
        .deploy(
            &s.admin,
            json!({"set_id": "0", "expected_generation": 0}),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{d}");
    let id = d["id"].as_str().unwrap().to_owned();
    let running = format!("shop/development/{id}");

    // The grace period passes with the deployment still running: it is reported.
    let left = s.state.drain(std::time::Duration::from_millis(100)).await;
    assert_eq!(left, vec![running.clone()]);

    // While draining, changes are refused, and status stays available.
    let (status, ready) = s.call(Method::GET, "/readyz", None, None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{ready}");
    assert_eq!(ready["reasons"], json!(["shutting_down"]));
    let (status, b) = s
        .post("/orgs/acme/apps", &s.admin, json!({"id": "late"}))
        .await;
    assert_error(status, &b, StatusCode::SERVICE_UNAVAILABLE, "SRV-002");
    let (status, b) = s.get(&format!("{DEPLOYS}/{id}"), &s.admin).await;
    assert_eq!(status, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "in_progress", "{b}");
    // Login changes state too; health and version stay available.
    let (status, b) = s
        .call(Method::POST, "/auth/github/device", None, Some(json!({})))
        .await;
    assert_error(status, &b, StatusCode::SERVICE_UNAVAILABLE, "SRV-002");
    let (status, b) = s.call(Method::GET, "/version", None, None).await;
    assert_eq!(status, StatusCode::OK, "{b}");

    // A deployment that finishes within the grace period ends the wait.
    let drain = tokio::spawn({
        let state = s.state.clone();
        async move { state.drain(std::time::Duration::from_secs(30)).await }
    });
    gate.add_permits(1);
    assert_eq!(drain.await.unwrap(), Vec::<String>::new());
    assert_eq!(s.finished(&id).await["status"], "succeeded");
}

#[tokio::test]
async fn shutdown_waits_for_changes_accepted_before_it_began() {
    let s = server().await;
    // A change admitted just before shutdown may still start a deployment.
    let admitted = s.state.admit_change().unwrap();
    let drain = tokio::spawn({
        let state = s.state.clone();
        async move { state.drain(std::time::Duration::from_secs(30)).await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(!drain.is_finished());
    assert_eq!(
        s.state.admit_change().unwrap_err(),
        vec!["shutting_down".to_owned()]
    );
    drop(admitted);
    assert_eq!(drain.await.unwrap(), Vec::<String>::new());
}

/// A server whose limits `set` adjusts, with deployments held until released.
async fn limited(set: impl FnOnce(&mut crate::config::WorkLimits)) -> (Server, Arc<Semaphore>) {
    let dir = tempfile::tempdir().unwrap();
    let mut config = crate::Config::new("acme", dir.path());
    config.images.pin_other_registries = false;
    set(&mut config.limits);
    let s = super::server_with(dir, config).await;
    let gate = s.deployable().await;
    (s, gate)
}

impl Server {
    /// A `development` environment `id` of `shop`.
    async fn dev_env(&self, id: &str) {
        let (status, b) = self
            .post(
                "/orgs/acme/apps/shop/envs",
                &self.admin,
                json!({"id": id, "type": "development"}),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{b}");
    }
}

#[tokio::test]
async fn running_deployments_are_bounded_and_status_stays_available() {
    let (s, gate) = limited(|l| l.running_deployments = 2).await;
    for env in ["staging", "qa"] {
        s.dev_env(env).await;
    }
    let body = json!({"set_id": "0", "expected_generation": 0});
    let path = |env: &str| format!("/orgs/acme/apps/shop/envs/{env}/deploys");
    for env in ["development", "staging"] {
        let (status, d) = s.post(&path(env), &s.admin, body.clone()).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{d}");
    }
    // Full: the next one is refused before anything is stored.
    let (status, b) = s.post(&path("qa"), &s.admin, body.clone()).await;
    assert_error(status, &b, StatusCode::TOO_MANY_REQUESTS, "API-007");
    let (_, page) = s.get(&path("qa"), &s.admin).await;
    assert_eq!(page["deployments"], json!([]), "{page}");
    // So is an environment deletion, which would run too.
    let (status, b) = s.remove_env("/orgs/acme/apps/shop/envs/qa", &s.admin).await;
    assert_error(status, &b, StatusCode::TOO_MANY_REQUESTS, "API-007");

    // Status requests keep answering while the deployments run.
    for _ in 0..10 {
        let started = std::time::Instant::now();
        let (status, b) = s.get(&path("development"), &s.admin).await;
        assert_eq!(status, StatusCode::OK, "{b}");
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    // Once they finish, there is room again.
    gate.add_permits(2);
    let mut accepted = false;
    for _ in 0..200 {
        let (status, _) = s.post(&path("qa"), &s.admin, body.clone()).await;
        if status == StatusCode::ACCEPTED {
            accepted = true;
            break;
        }
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(accepted, "room was not freed");
    gate.add_permits(1);
}

#[tokio::test]
async fn graphs_over_the_limits_are_refused() {
    let (s, _gate) = limited(|l| l.graph_nodes = 1).await;
    // The environment and a workload: two resources.
    let delta = s.delta("nginx:1").await;
    let (status, b) = s
        .deploy(
            &s.admin,
            json!({"delta_id": delta, "expected_generation": 0}),
            None,
        )
        .await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
    assert!(
        b["message"]
            .as_str()
            .unwrap()
            .contains("more than this server's limits of 1 and 5000"),
        "{b}"
    );
    let (_, page) = s.get(DEPLOYS, &s.admin).await;
    assert_eq!(page["deployments"], json!([]), "{page}");
}
