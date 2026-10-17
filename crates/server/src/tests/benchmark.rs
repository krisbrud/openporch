//! The reference capacity benchmark (`control-plane-operations`, reference
//! capacity validation): a 100-node, 1,000-edge graph is previewed at least
//! 100 times while four deployments of it run on a driver that takes time,
//! and status requests arrive at 10 a second. Planning p95 must stay below 2
//! seconds and status p95 below 500 ms; peak memory and disk growth are
//! reported. It is ignored by `cargo test`; `just bench` runs it in release
//! mode on two CPUs, and `docs/benchmark.md` records the results.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use openporch_drivers::{
    DeleteOutcome, Driver, DriverFuture, DriverInfo, DriverRequest, Echo, Observation, Plan,
    PutOutcome,
};
use serde_json::{Value, json};

use super::{Server, server};

/// Chained resources; with the environment, the workload and the workload's
/// own resource the graph has 100 nodes.
const RESOURCES: usize = 97;
/// How many earlier resources each one references, at most.
const FAN_IN: usize = 11;
/// How long the fake driver takes to provision one resource.
const PUT_DELAY: Duration = Duration::from_millis(20);
/// Applications deploying at once, with the default driver concurrency of 4.
const LOAD: usize = 4;
const SAMPLES: usize = 100;
const STATUS_INTERVAL: Duration = Duration::from_millis(100);
const DRIVER: &str = "bench/delayed";

/// Provisionings the delayed driver has started.
static PUTS: AtomicUsize = AtomicUsize::new(0);

/// `bench/delayed`: `openporch/echo`, slowed down. It waits before it
/// provisions anything, and previews, so redeployments need no permits.
#[derive(Debug)]
struct Delayed(Echo, DriverInfo);

impl Delayed {
    fn new() -> Self {
        let echo = Echo::new();
        let mut info = echo.info().clone();
        info.id = DRIVER.to_owned();
        info.capabilities.preview = true;
        Self(echo, info)
    }
}

impl Driver for Delayed {
    fn info(&self) -> &DriverInfo {
        &self.1
    }

    fn plan<'a>(&'a self, req: &'a DriverRequest) -> DriverFuture<'a, Plan> {
        self.0.plan(req)
    }

    fn plan_delete<'a>(&'a self, req: &'a DriverRequest) -> DriverFuture<'a, Plan> {
        self.0.plan_delete(req)
    }

    fn put<'a>(&'a self, req: &'a DriverRequest) -> DriverFuture<'a, PutOutcome> {
        Box::pin(async move {
            PUTS.fetch_add(1, Ordering::Relaxed);
            tokio::time::sleep(PUT_DELAY).await;
            self.0.put(req).await
        })
    }

    fn get<'a>(&'a self, req: &'a DriverRequest) -> DriverFuture<'a, Observation> {
        self.0.get(req)
    }

    fn delete<'a>(&'a self, req: &'a DriverRequest) -> DriverFuture<'a, DeleteOutcome> {
        self.0.delete(req)
    }
}

/// One definition per resource: `n{i}` references up to `FAN_IN` of the
/// resources before it, so the graph is deep as well as dense.
async fn platform(s: &Server) {
    for (id, ty) in [("env", "base-env"), ("workloads", "workload")] {
        let def = json!({"id": id, "type": ty, "driver_type": DRIVER,
                         "driver_inputs": {}, "criteria": [{}]});
        let (st, b) = s.post("/orgs/acme/resources/defs", &s.admin, def).await;
        assert_eq!(st, StatusCode::CREATED, "{b}");
    }
    for i in 0..RESOURCES {
        let refs: serde_json::Map<String, Value> = (i.saturating_sub(FAN_IN)..i)
            .map(|j| {
                (
                    format!("n{j}"),
                    json!(format!("${{resources['config.default#n{j}'].outputs.id}}")),
                )
            })
            .collect();
        let def = json!({"id": format!("n{i}"), "type": "config",
                         "driver_type": DRIVER,
                         "driver_inputs": {"values": {"id": format!("n{i}"), "refs": refs}},
                         "criteria": [{"res_id": format!("n{i}")}]});
        let (st, b) = s.post("/orgs/acme/resources/defs", &s.admin, def).await;
        assert_eq!(st, StatusCode::CREATED, "{b}");
    }
    // The workload's own resource uses the last resources of the chain.
    let refs: serde_json::Map<String, Value> = (RESOURCES - FAN_IN..RESOURCES)
        .map(|j| {
            (
                format!("n{j}"),
                json!(format!("${{resources['config.default#n{j}'].outputs.id}}")),
            )
        })
        .collect();
    let def = json!({"id": "top", "type": "config", "driver_type": DRIVER,
                     "driver_inputs": {"values": {"refs": refs}},
                     "criteria": [{"res_id": "modules.web.externals.top"}]});
    let (st, b) = s.post("/orgs/acme/resources/defs", &s.admin, def).await;
    assert_eq!(st, StatusCode::CREATED, "{b}");
}

/// An application whose one workload has a resource that uses the chain.
async fn app(s: &Server, id: &str) -> Value {
    let (st, b) = s.post("/orgs/acme/apps", &s.admin, json!({"id": id})).await;
    assert_eq!(st, StatusCode::CREATED, "{b}");
    let (st, d) = s
        .post(
            &format!("/orgs/acme/apps/{id}/deltas"),
            &s.admin,
            json!({"modules": {"add": {"web": {
                "spec": {"containers": {"web": {"image": "nginx:1"}}},
                "externals": {"top": {"type": "config"}}}}}}),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{d}");
    d["id"].clone()
}

async fn generation(s: &Server, env: &str) -> Value {
    let (st, e) = s.get(env, &s.admin).await;
    assert_eq!(st, StatusCode::OK, "{e}");
    e["generation"].clone()
}

/// Deploys to `app`'s development environment from `source` (`delta_id` or
/// `deploy_id`), waits for it, and returns the deployment's path.
async fn deploy(s: &Server, app: &str, source: Value) -> String {
    let env = format!("/orgs/acme/apps/{app}/envs/development");
    let mut body = source;
    body["expected_generation"] = generation(s, &env).await;
    let (st, d) = s.post(&format!("{env}/deploys"), &s.admin, body).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{d}");
    let path = format!("{env}/deploys/{}", d["id"].as_str().unwrap());
    loop {
        let (_, done) = s.get(&path, &s.admin).await;
        if done["status"] != "in_progress" {
            assert_eq!(done["status"], "succeeded", "{done}");
            return path;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn p95(samples: &mut [Duration]) -> Duration {
    samples.sort();
    samples[(samples.len() * 95).div_ceil(100) - 1]
}

fn ms(d: Duration) -> f64 {
    (d.as_secs_f64() * 10_000.0).round() / 10.0
}

/// A field of `/proc/self/status`, in KiB.
fn proc_status(field: &str) -> Option<u64> {
    std::fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix(field))?
        .trim()
        .trim_end_matches("kB")
        .trim()
        .parse()
        .ok()
}

fn dir_size(path: &std::path::Path) -> u64 {
    std::fs::read_dir(path)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|e| match e.metadata() {
                    Ok(m) if m.is_dir() => dir_size(&e.path()),
                    Ok(m) => m.len(),
                    Err(_) => 0,
                })
                .sum()
        })
        .unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "the release benchmark; run it with `just bench`"]
async fn reference_capacity_benchmark() {
    let s = server().await;
    let mut registry = openporch_drivers::DriverRegistry::with_builtins();
    registry
        .register(Arc::new(Delayed::new()) as Arc<_>)
        .unwrap();
    crate::state::run_with_drivers(
        &s.state,
        s.state.keyring.clone().unwrap(),
        None,
        Arc::new(registry),
        None,
    );
    // A record, so definitions may name it; the server runs it in-process.
    sqlx::query("INSERT INTO drivers (org_id, id, body, created_at) VALUES ('acme', ?, '{}', 0)")
        .bind(DRIVER)
        .execute(s.state.store.pool())
        .await
        .unwrap();
    platform(&s).await;
    let s = Arc::new(s);

    // The previewed application, and one deployed once to count its graph.
    let bench = app(&s, "bench").await;
    let watched = app(&s, "watched").await;
    deploy(&s, "watched", json!({"delta_id": watched})).await;
    let (st, g) = s
        .get(
            "/orgs/acme/apps/watched/envs/development/resources/graph",
            &s.admin,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{g}");
    let graph = (
        g["nodes"].as_array().unwrap().len(),
        g["edges"].as_array().unwrap().len(),
    );

    let disk_before = dir_size(s.dir.path());
    let stop = Arc::new(AtomicBool::new(false));
    let completed = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let mut load = Vec::new();
    for i in 0..LOAD {
        let id = format!("load{i}");
        let delta = app(&s, &id).await;
        let (s, stop, completed, active) =
            (s.clone(), stop.clone(), completed.clone(), active.clone());
        load.push(tokio::spawn(async move {
            // The first deployment creates everything; later ones redeploy
            // its set, which converges every resource again.
            let mut source = json!({"delta_id": delta});
            while !stop.load(Ordering::Relaxed) {
                active.fetch_add(1, Ordering::Relaxed);
                let path = deploy(&s, &id, source).await;
                active.fetch_sub(1, Ordering::Relaxed);
                completed.fetch_add(1, Ordering::Relaxed);
                source = json!({"deploy_id": path.rsplit('/').next().unwrap(),
                                "replay": "current"});
            }
        }));
    }
    // Let the deployments get going before measuring.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let sent = Arc::new(AtomicUsize::new(0));
    // Status requests read the environments being deployed, in turn, and
    // record how many deployments are running at each tick.
    let status = {
        let (s, stop, active, sent) = (s.clone(), stop.clone(), active.clone(), sent.clone());
        tokio::spawn(async move {
            let mut requests = Vec::new();
            let mut running = Vec::new();
            let mut tick = tokio::time::interval(STATUS_INTERVAL);
            while !stop.load(Ordering::Relaxed) {
                tick.tick().await;
                sent.fetch_add(1, Ordering::Relaxed);
                running.push(active.load(Ordering::Relaxed));
                let path = format!(
                    "/orgs/acme/apps/load{}/envs/development",
                    requests.len() % LOAD
                );
                let s = s.clone();
                requests.push(tokio::spawn(async move {
                    let start = Instant::now();
                    let (st, b) = s.get(&path, &s.admin).await;
                    assert_eq!(st, StatusCode::OK, "{b}");
                    start.elapsed()
                }));
            }
            let mut out = Vec::new();
            for r in requests {
                out.push(r.await.unwrap());
            }
            (out, running)
        })
    };

    let env = "/orgs/acme/apps/bench/envs/development";
    let body = json!({"kind": "deploy", "delta_id": bench,
                      "expected_generation": generation(&s, env).await});
    let mut planning = Vec::with_capacity(SAMPLES);
    let puts_before = PUTS.load(Ordering::Relaxed);
    for _ in 0..SAMPLES {
        let start = Instant::now();
        let (st, plan) = s
            .post(&format!("{env}/plans"), &s.admin, body.clone())
            .await;
        planning.push(start.elapsed());
        assert_eq!(st, StatusCode::CREATED, "{plan}");
        assert_eq!(plan["resources"].as_array().unwrap().len(), graph.0);
    }
    let puts_while_planning = PUTS.load(Ordering::Relaxed) - puts_before;
    // The load runs until there are enough status samples too.
    while sent.load(Ordering::Relaxed) < SAMPLES {
        tokio::time::sleep(STATUS_INTERVAL).await;
    }
    let peak_during = proc_status("VmHWM:");
    stop.store(true, Ordering::Relaxed);
    let (mut status, running) = status.await.unwrap();
    for l in load {
        l.await.unwrap();
    }
    let disk_after = dir_size(s.dir.path());

    let planning_p95 = p95(&mut planning);
    let status_p95 = p95(&mut status);
    let report = json!({
        "config": {
            "cpus": std::thread::available_parallelism().map(|n| n.get()).ok(),
            "runtime_workers": 2,
            "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
            "driver_put_delay_ms": PUT_DELAY.as_millis(),
            "deploying_applications": LOAD,
            "driver_concurrency": s.state.config.limits.driver_concurrency,
            "status_interval_ms": STATUS_INTERVAL.as_millis(),
        },
        "driver_load": {
            "provisionings_while_planning": puts_while_planning,
            "mean_running_deployments": running.iter().sum::<usize>() as f64
                / running.len() as f64,
        },
        "graph": {"nodes": graph.0, "edges": graph.1},
        "deployments_completed_under_load": completed.load(Ordering::Relaxed),
        "planning": {"samples": planning.len(), "p95_ms": ms(planning_p95),
                     "max_ms": ms(*planning.last().unwrap())},
        "status": {"samples": status.len(), "p95_ms": ms(status_p95),
                   "max_ms": ms(*status.last().unwrap())},
        "peak_memory_mib": peak_during.map(|k| k / 1024),
        "subprocess_peak_memory_mib": 0,
        "disk_growth_mib": (disk_after.saturating_sub(disk_before)) as f64 / (1024.0 * 1024.0),
    });
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
    if let Ok(path) = std::env::var("OPENPORCH_BENCH_OUT") {
        std::fs::write(path, serde_json::to_string_pretty(&report).unwrap() + "\n").unwrap();
    }
    assert_eq!(graph.0, 100, "{report}");
    assert!(graph.1 >= 1000, "{report}");
    // The driver load was real: the delayed driver kept provisioning.
    assert!(puts_while_planning >= SAMPLES, "{report}");
    assert!(planning_p95 < Duration::from_secs(2), "{report}");
    assert!(status_p95 < Duration::from_millis(500), "{report}");
}
