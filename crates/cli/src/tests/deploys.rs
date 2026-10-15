//! Deployment commands against a dev server: `Deploy and wait`, `Score alias`,
//! `Follow logs`, `Raw get` and `Response lost after acceptance` in `cli`, and
//! the workload deploy scenarios of `deployment-sets-and-deltas`.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::extract::Request;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use openporch_server::deploy::{Job, Runner};
use openporch_server::state::AppState;
use serde_json::{Value, json};
use tokio::sync::Semaphore;

use super::objects::{admin_client, admin_machine};
use super::{DevServer, Machine};
use openporch_client::Method;

const WEB: &str = r#"
apiVersion: openporch.dev/v1
kind: Workload
metadata: {name: web}
containers:
  web:
    image: .
    variables: {PAGE_SIZE: "10"}
"#;

const SCORE: &str = r#"
apiVersion: score.dev/v1b1
metadata: {name: web}
containers:
  web:
    image: nginx:1
"#;

/// The openporch document equivalent to [`SCORE`].
const SCORE_AS_WORKLOAD: &str = r#"
apiVersion: openporch.dev/v1
kind: Workload
metadata: {name: web}
containers:
  web:
    image: nginx:1
"#;

/// `shop` with definitions that deploy workloads without a cloud, and a
/// machine logged in as the administrator with `--app shop --env development`
/// in its environment.
pub(super) async fn deployable(server: &DevServer, workload_driver: Value) -> Machine {
    server.post("/orgs/acme/apps", json!({"id": "shop"})).await;
    server
        .post(
            "/orgs/acme/resources/defs",
            json!({"id": "env", "type": "base-env", "driver_type": "openporch/none", "criteria": [{}]}),
        )
        .await;
    let mut def = json!({"id": "workloads", "type": "workload", "criteria": [{}]});
    def.as_object_mut()
        .unwrap()
        .extend(workload_driver.as_object().unwrap().clone());
    server.post("/orgs/acme/resources/defs", def).await;
    let mut m = admin_machine(server);
    m.set("OPENPORCH_APP", "shop")
        .set("OPENPORCH_ENV", "development");
    m
}

pub(super) fn file(m: &Machine, name: &str, content: &str) -> String {
    let path = m.home.path().join(name);
    std::fs::write(&path, content).unwrap();
    path.to_string_lossy().into_owned()
}

pub(super) async fn deploys(server: &DevServer) -> Vec<Value> {
    let page = admin_client(server)
        .request(
            Method::GET,
            "/orgs/acme/apps/shop/envs/development/deploys",
            None,
        )
        .await
        .unwrap();
    page["deployments"].as_array().unwrap().clone()
}

/// `Deploy and wait` in `cli`, with an image supplied at deploy time.
#[tokio::test]
async fn deploy_and_wait() {
    let server = DevServer::start(None).await;
    let m = deployable(&server, json!({"driver_type": "openporch/none"})).await;
    let f = file(&m, "workload.yaml", WEB);

    let r = m
        .porch(
            &[
                "workload",
                "deploy",
                "-f",
                &f,
                "--image",
                "web=nginx:1",
                "--comment",
                "first",
                "--wait",
            ],
            "",
        )
        .await;
    assert_eq!(r.code, 0, "{r:?}");
    let d = &deploys(&server).await[0];
    let id = d["id"].as_str().unwrap();
    assert_eq!(d["status"], "succeeded");
    assert_eq!(d["comment"], "first");
    let mut lines = r.out.lines();
    assert_eq!(
        lines.next(),
        Some(format!("Deployment {id} started.").as_str())
    );
    assert!(
        r.out.contains("workload.default#modules.web: "),
        "no status line for the workload: {}",
        r.out
    );
    assert_eq!(
        r.out.lines().last(),
        Some(format!("Deployment {id} succeeded.").as_str())
    );

    // The deployed set has the image from the command line.
    let set = admin_client(&server)
        .request(
            Method::GET,
            &format!(
                "/orgs/acme/apps/shop/sets/{}",
                d["set_id"].as_str().unwrap()
            ),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        set["modules"]["web"]["spec"]["containers"]["web"]["image"],
        "nginx:1"
    );

    // Without the image, the command fails naming it, and nothing is deployed.
    let r = m.porch(&["workload", "deploy", "-f", &f], "").await;
    assert_eq!(r.code, 1);
    assert!(r.err.contains("--image web="), "{r:?}");
    assert_eq!(deploys(&server).await.len(), 1);
}

/// `--wait` exits non-zero when the deployment fails.
#[tokio::test]
async fn waiting_on_a_failed_deployment_fails() {
    let server = DevServer::start(None).await;
    let m = deployable(
        &server,
        json!({"driver_type": "openporch/template",
               "driver_inputs": {"values": {"templates": {"outputs": "{{ nope() }}"}}}}),
    )
    .await;
    let f = file(&m, "workload.yaml", SCORE_AS_WORKLOAD);
    let r = m
        .porch(&["workload", "deploy", "-f", &f, "--wait"], "")
        .await;
    assert_eq!(r.code, 1, "{r:?}");
    let d = &deploys(&server).await[0];
    assert_eq!(d["status"], "failed");
    assert!(
        r.err
            .contains(&format!("deployment {} failed", d["id"].as_str().unwrap())),
        "{r:?}"
    );
}

/// `Score alias` in `cli`, `Redeploy unchanged workload` and `Diff last two
/// deployments` in `deployment-sets-and-deltas`.
#[tokio::test]
async fn score_alias_redeploys_and_diffs() {
    let server = DevServer::start(None).await;
    let m = deployable(&server, json!({"driver_type": "openporch/none"})).await;
    let score = file(&m, "score.yaml", SCORE);
    let workload = file(&m, "workload.yaml", SCORE_AS_WORKLOAD);

    let r = m
        .porch(&["score", "deploy", "-f", &score, "--wait"], "")
        .await;
    assert_eq!(r.code, 0, "{r:?}");
    // The same workload as an openporch document changes nothing.
    let r = m
        .porch(&["workload", "deploy", "-f", &workload, "--wait"], "")
        .await;
    assert_eq!(r.code, 0, "{r:?}");
    let list = deploys(&server).await;
    assert_eq!(list.len(), 2);
    assert_eq!(list[0]["set_id"], list[1]["set_id"]);

    // `score deploy` takes only Score files.
    let r = m.porch(&["score", "deploy", "-f", &workload], "").await;
    assert_eq!(r.code, 1);
    assert!(r.err.contains("not a Score file"), "{r:?}");

    let r = m
        .porch(
            &[
                "workload",
                "deploy",
                "-f",
                &workload,
                "--property",
                "containers.web.variables.PAGE_SIZE=20",
                "--wait",
            ],
            "",
        )
        .await;
    assert_eq!(r.code, 0, "{r:?}");
    let r = m
        .porch(&["diff", "sets", "deploy/.", "deploy/+1", "-o", "json"], "")
        .await;
    assert_eq!(r.code, 0, "{r:?}");
    let delta: Value = serde_json::from_str(&r.out).unwrap();
    assert_eq!(
        delta["modules"]["update"]["web"],
        json!([{"op": "remove", "path": "/spec/containers/web/variables"}])
    );
    let r = m
        .porch(
            &[
                "diff",
                "sets",
                "deploy/last-successful",
                "deploy/+1",
                "-o",
                "json",
            ],
            "",
        )
        .await;
    assert_eq!(
        serde_json::from_str::<Value>(&r.out).unwrap(),
        delta,
        "{r:?}"
    );
}

/// `deploy deploy` replays and `deploy delta`.
#[tokio::test]
async fn logs_replays_and_stored_deltas() {
    let server = DevServer::start(None).await;
    let m = deployable(&server, json!({"driver_type": "openporch/none"})).await;
    let f = file(&m, "workload.yaml", SCORE_AS_WORKLOAD);
    let r = m.porch(&["workload", "deploy", "-f", &f], "").await;
    assert_eq!(r.code, 0, "{r:?}");
    let id = deploys(&server).await[0]["id"].as_str().unwrap().to_owned();

    let r = m.porch(&["logs", "deploy", &id], "").await;
    assert_eq!(r.code, 0, "{r:?}");
    assert_eq!(deploys(&server).await[0]["status"], "succeeded");

    for replay in ["snapshot", "current"] {
        let r = m
            .porch(
                &[
                    "deploy",
                    "deploy",
                    "last-successful",
                    "--replay",
                    replay,
                    "--wait",
                ],
                "",
            )
            .await;
        assert_eq!(r.code, 0, "{r:?}");
    }
    let list = deploys(&server).await;
    assert_eq!(list.len(), 3);
    assert_eq!(list[0]["set_id"], list[2]["set_id"]);

    let delta = admin_client(&server)
        .request(
            Method::POST,
            "/orgs/acme/apps/shop/deltas",
            Some(&json!({"modules": {"remove": ["web"]}})),
        )
        .await
        .unwrap();
    let r = m
        .porch(
            &[
                "deploy",
                "delta",
                delta["id"].as_str().unwrap(),
                "development",
                "--wait",
            ],
            "",
        )
        .await;
    // Removing a workload deletes it, which needs a permit.
    assert_eq!(r.code, 1, "{r:?}");
    assert!(r.err.contains("DPL-"), "{r:?}");
}

/// `Raw get` in `cli`.
#[tokio::test]
async fn raw_api_requests() {
    let server = DevServer::start(None).await;
    let m = admin_machine(&server);
    let r = m
        .porch(&["api", "get", "/orgs/acme/resources/types"], "")
        .await;
    assert_eq!(r.code, 0, "{r:?}");
    let types: Value = serde_json::from_str(&r.out).unwrap();
    assert!(
        types
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["id"] == "workload")
    );

    let r = m
        .porch(
            &["api", "post", "/orgs/acme/apps", "-d", r#"{"id": "blog"}"#],
            "",
        )
        .await;
    assert_eq!(r.code, 0, "{r:?}");
    let r = m
        .porch(
            &["api", "post", "/orgs/acme/env-types", "-d", "@-"],
            r#"{"id": "qa"}"#,
        )
        .await;
    assert_eq!(r.code, 0, "{r:?}");
    let r = m
        .porch(&["api", "delete", "/orgs/acme/env-types/qa"], "")
        .await;
    assert_eq!(r.code, 0, "{r:?}");
    let r = m.porch(&["api", "get", "/orgs/acme/apps/nope"], "").await;
    assert_eq!(r.code, 1);
    assert!(r.err.contains("HTTP 404"), "{r:?}");
}

/// Serves `router` on a loopback port.
pub(super) async fn serve(router: axum::Router, data: tempfile::TempDir) -> DevServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
    });
    DevServer {
        url: format!("http://{addr}"),
        admin: std::fs::read_to_string(data.path().join("bootstrap-token"))
            .unwrap()
            .trim()
            .to_owned(),
        _data: data,
    }
}

/// Holds each deployment until the test releases it, then runs it.
#[derive(Debug)]
struct Gated {
    gate: Arc<Semaphore>,
    inner: Arc<dyn Runner>,
}

impl Runner for Gated {
    fn run(&self, state: AppState, job: Job) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        let (gate, inner) = (self.gate.clone(), self.inner.clone());
        Box::pin(async move {
            if let Ok(permit) = gate.acquire().await {
                permit.forget();
            }
            inner.run(state, job).await;
        })
    }
}

/// `Follow logs` in `cli`: the log is followed while the deployment is in
/// progress, until it finishes.
#[tokio::test]
async fn follow_logs_of_a_running_deployment() {
    let data = tempfile::tempdir().unwrap();
    let state = openporch_server::start(openporch_server::Config::new("acme", data.path()), None)
        .await
        .unwrap();
    let gate = Arc::new(Semaphore::new(0));
    state.runners.set(Arc::new(Gated {
        gate: gate.clone(),
        inner: state.runners.current(),
    }));
    let (router, _) = openporch_server::router(state);
    let server = serve(router, data).await;
    let m = deployable(&server, json!({"driver_type": "openporch/none"})).await;
    let f = file(&m, "workload.yaml", SCORE_AS_WORKLOAD);
    let r = m.porch(&["workload", "deploy", "-f", &f], "").await;
    assert_eq!(r.code, 0, "{r:?}");

    let release = async {
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        let status = deploys(&server).await[0]["status"].clone();
        gate.add_permits(1);
        status
    };
    let (r, status_at_release) = tokio::join!(m.porch(&["logs", "deploy", "+0"], ""), release);
    assert_eq!(status_at_release, "in_progress");
    assert_eq!(r.code, 0, "{r:?}");
    assert!(r.out.contains("workload.default#modules.web: "), "{r:?}");
    assert_eq!(deploys(&server).await[0]["status"], "succeeded");
}

/// Answers the first deployment request with a response cut off after the
/// server accepted it.
async fn lose_first_deploy(lost: Arc<AtomicBool>, req: Request, next: Next) -> Response {
    let deploy = req.method() == axum::http::Method::POST && req.uri().path().ends_with("/deploys");
    let res = next.run(req).await;
    if deploy && !lost.swap(true, Ordering::SeqCst) {
        // The body is shorter than announced, so the client never reads a response.
        return (
            res.status(),
            [(axum::http::header::CONTENT_LENGTH, "1000")],
            "{",
        )
            .into_response();
    }
    res
}

/// `Response lost after acceptance` in `cli`.
#[tokio::test]
async fn lost_acceptance_response_returns_the_original_deployment() {
    let data = tempfile::tempdir().unwrap();
    let config = openporch_server::Config::new("acme", data.path());
    let state = openporch_server::start(config, None).await.unwrap();
    let (router, _) = openporch_server::router(state);
    let lost = Arc::new(AtomicBool::new(false));
    let router = router.layer(middleware::from_fn({
        let lost = lost.clone();
        move |req, next| lose_first_deploy(lost.clone(), req, next)
    }));
    let server = serve(router, data).await;
    let m = deployable(&server, json!({"driver_type": "openporch/none"})).await;
    let f = file(&m, "workload.yaml", SCORE_AS_WORKLOAD);
    let r = m
        .porch(&["workload", "deploy", "-f", &f, "--wait"], "")
        .await;
    assert_eq!(r.code, 0, "{r:?}");
    assert!(lost.load(Ordering::SeqCst));
    assert!(r.err.contains("sending the same request again"), "{r:?}");
    let list = deploys(&server).await;
    assert_eq!(list.len(), 1, "{list:?}");
    assert!(r.out.starts_with(&format!(
        "Deployment {} started.",
        list[0]["id"].as_str().unwrap()
    )));
}

/// `--timeout` stops waiting on a deployment that is still running, which
/// carries on.
#[tokio::test]
async fn waiting_stops_at_the_timeout() {
    let data = tempfile::tempdir().unwrap();
    let state = openporch_server::start(openporch_server::Config::new("acme", data.path()), None)
        .await
        .unwrap();
    state.runners.set(Arc::new(Gated {
        gate: Arc::new(Semaphore::new(0)),
        inner: state.runners.current(),
    }));
    let (router, _) = openporch_server::router(state);
    let server = serve(router, data).await;
    let m = deployable(&server, json!({"driver_type": "openporch/none"})).await;
    let f = file(&m, "workload.yaml", SCORE_AS_WORKLOAD);
    let r = m
        .porch(
            &["workload", "deploy", "-f", &f, "--wait", "--timeout", "1s"],
            "",
        )
        .await;
    assert_eq!(r.code, 1, "{r:?}");
    assert!(r.err.contains("still running after 1s"), "{r:?}");
    assert!(r.err.contains("porch logs deploy"), "{r:?}");
    assert_eq!(deploys(&server).await[0]["status"], "in_progress");

    // A timeout needs `--wait`.
    let r = m
        .porch(&["workload", "deploy", "-f", &f, "--timeout", "1s"], "")
        .await;
    assert_eq!(r.code, 2, "{r:?}");
}
