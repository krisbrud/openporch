use std::sync::{Arc, Mutex};
use std::time::Duration;

use openporch_core::ids::Guresid;
use serde_json::{Map, Value, json};

use super::*;

fn object(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        other => panic!("not an object: {other}"),
    }
}

fn request() -> DriverRequest {
    DriverRequest {
        guresid: Guresid::new("0123456789abcdef0123456789abcdef").unwrap(),
        operation: Operation {
            id: "op-1".into(),
            fingerprint: "f1".into(),
        },
        resource_type: "postgres".into(),
        resource: Map::new(),
        resource_secrets: SecretMap::default(),
        values: Map::new(),
        secrets: SecretMap::default(),
        credentials: None,
        context: DriverContext::default(),
        cookie: None,
    }
}

/// A driver whose `put` starts an operation that `get` reports ready after
/// `polls` in-progress observations. It records the cookie each call received.
#[derive(Debug)]
struct Slow {
    info: DriverInfo,
    polls: usize,
    seen: Mutex<Vec<(&'static str, Option<String>)>>,
}

impl Slow {
    fn new(polls: usize) -> Self {
        Self {
            info: DriverInfo {
                id: "test/slow".into(),
                protocol_version: "test".into(),
                cookie_version: 1,
                input_schema: any_object(),
                output_schema: any_object(),
                capabilities: Capabilities::default(),
            },
            polls,
            seen: Mutex::new(Vec::new()),
        }
    }

    fn record(&self, call: &'static str, req: &DriverRequest) -> usize {
        let mut seen = self.seen.lock().unwrap();
        seen.push((call, req.cookie.as_ref().map(|c| c.data.clone())));
        seen.iter().filter(|(c, _)| *c == call).count()
    }

    fn seen(&self) -> Vec<(&'static str, Option<String>)> {
        self.seen.lock().unwrap().clone()
    }
}

impl Driver for Slow {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    fn put<'a>(&'a self, req: &'a DriverRequest) -> DriverFuture<'a, PutOutcome> {
        Box::pin(async move {
            self.record("put", req);
            Ok(PutOutcome::InProgress {
                poll_after: Duration::from_secs(30),
                cookie: Some(Cookie::new(1, "abc")?),
            })
        })
    }

    fn get<'a>(&'a self, req: &'a DriverRequest) -> DriverFuture<'a, Observation> {
        Box::pin(async move {
            let n = self.record("get", req);
            if n <= self.polls {
                return Ok(Observation::InProgress {
                    poll_after: Duration::from_secs(30),
                });
            }
            Ok(Observation::Ready {
                outputs: DriverOutputs {
                    values: object(json!({"host": "db.internal"})),
                    ..DriverOutputs::default()
                },
                cookie: None,
            })
        })
    }

    fn delete<'a>(&'a self, req: &'a DriverRequest) -> DriverFuture<'a, DeleteOutcome> {
        Box::pin(async move {
            self.record("delete", req);
            Ok(DeleteOutcome::Deleted)
        })
    }
}

#[tokio::test(start_paused = true)]
async fn asynchronous_provisioning_polls_get_until_ready() {
    let driver = Slow::new(2);
    let start = tokio::time::Instant::now();
    let (outputs, cookie) = converge(&driver, &request(), DEFAULT_TIMEOUT)
        .await
        .unwrap();
    assert_eq!(outputs.values, object(json!({"host": "db.internal"})));
    assert_eq!(cookie, Some(Cookie::new(1, "abc").unwrap()));
    // Three polls, each after the 30 seconds the driver asked for.
    assert_eq!(start.elapsed(), Duration::from_secs(90));
    let calls: Vec<_> = driver.seen().into_iter().map(|(c, _)| c).collect();
    assert_eq!(calls, ["put", "get", "get", "get"]);
}

#[tokio::test(start_paused = true)]
async fn cookie_round_trip() {
    let driver = Slow::new(0);
    let (_, cookie) = converge(&driver, &request(), DEFAULT_TIMEOUT)
        .await
        .unwrap();
    // The checkpointed cookie goes with the next put and delete for the GUResID.
    let mut next = request();
    next.cookie = cookie;
    let _ = driver.put(&next).await.unwrap();
    remove(&driver, &next, DEFAULT_TIMEOUT).await.unwrap();
    let abc = Some("abc".to_owned());
    assert_eq!(
        driver.seen(),
        [
            ("put", None),
            ("get", abc.clone()),
            ("put", abc.clone()),
            ("delete", abc),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn polling_stops_at_the_deadline() {
    let driver = Slow::new(usize::MAX);
    let start = tokio::time::Instant::now();
    let err = converge(&driver, &request(), Duration::from_secs(100))
        .await
        .unwrap_err();
    assert_eq!(err, DriverError::Timeout(Duration::from_secs(100)));
    assert!(!err.is_retryable());
    assert_eq!(start.elapsed(), Duration::from_secs(100));
}

/// A driver whose calls never return.
#[derive(Debug)]
struct Hung(DriverInfo);

impl Driver for Hung {
    fn info(&self) -> &DriverInfo {
        &self.0
    }

    fn put<'a>(&'a self, _req: &'a DriverRequest) -> DriverFuture<'a, PutOutcome> {
        Box::pin(std::future::pending())
    }

    fn get<'a>(&'a self, _req: &'a DriverRequest) -> DriverFuture<'a, Observation> {
        Box::pin(std::future::pending())
    }

    fn delete<'a>(&'a self, _req: &'a DriverRequest) -> DriverFuture<'a, DeleteOutcome> {
        Box::pin(std::future::pending())
    }
}

#[tokio::test(start_paused = true)]
async fn a_driver_that_does_not_answer_times_out() {
    let driver = Hung(Echo::new().info().clone());
    let timeout = Duration::from_secs(5);
    let err = converge(&driver, &request(), timeout).await.unwrap_err();
    assert_eq!(err, DriverError::Timeout(timeout));
    assert_eq!(
        remove(&driver, &request(), timeout).await.unwrap_err(),
        DriverError::Timeout(timeout)
    );
    assert_eq!(err.to_string(), "the driver did not answer within 5s");
}

/// A driver that reports `in_progress` and then the given observation.
#[derive(Debug)]
struct Then(DriverInfo, Observation, Option<Cookie>);

impl Driver for Then {
    fn info(&self) -> &DriverInfo {
        &self.0
    }

    fn put<'a>(&'a self, _req: &'a DriverRequest) -> DriverFuture<'a, PutOutcome> {
        let cookie = self.2.clone();
        Box::pin(async move {
            Ok(PutOutcome::InProgress {
                poll_after: Duration::ZERO,
                cookie,
            })
        })
    }

    fn get<'a>(&'a self, _req: &'a DriverRequest) -> DriverFuture<'a, Observation> {
        let o = self.1.clone();
        Box::pin(async move { Ok(o) })
    }

    fn delete<'a>(&'a self, _req: &'a DriverRequest) -> DriverFuture<'a, DeleteOutcome> {
        Box::pin(async {
            Ok(DeleteOutcome::InProgress {
                poll_after: Duration::ZERO,
            })
        })
    }
}

#[tokio::test(start_paused = true)]
async fn failed_absent_and_unknown_observations() {
    let info = Echo::new().info().clone();
    let invalid = DriverError::Invalid {
        code: Some("PG-1".into()),
        message: "bad version".into(),
    };
    let failed = Then(
        info.clone(),
        Observation::Failed {
            error: invalid.clone(),
        },
        None,
    );
    let r = request();
    assert_eq!(
        converge(&failed, &r, DEFAULT_TIMEOUT).await.unwrap_err(),
        invalid
    );
    assert_eq!(invalid.to_string(), "PG-1: bad version");

    let absent = Then(info.clone(), Observation::Absent, None);
    assert!(matches!(
        converge(&absent, &r, DEFAULT_TIMEOUT).await,
        Err(DriverError::Uncertain(_))
    ));
    // For a deletion, absent is done.
    remove(&absent, &r, DEFAULT_TIMEOUT).await.unwrap();

    let unknown = Then(
        info,
        Observation::Unknown {
            reason: "lost the operation".into(),
        },
        None,
    );
    assert_eq!(
        converge(&unknown, &r, DEFAULT_TIMEOUT).await.unwrap_err(),
        DriverError::Uncertain("lost the operation".into())
    );
    assert_eq!(
        remove(&unknown, &r, DEFAULT_TIMEOUT).await.unwrap_err(),
        DriverError::Uncertain("lost the operation".into())
    );
}

#[tokio::test(start_paused = true)]
async fn returned_cookies_are_checked() {
    let mut info = Echo::new().info().clone();
    info.cookie_version = 2;
    let big = Cookie {
        version: 2,
        data: "x".repeat(MAX_COOKIE_BYTES + 1),
    };
    let driver = Then(info.clone(), Observation::Absent, Some(big));
    assert!(matches!(
        converge(&driver, &request(), DEFAULT_TIMEOUT).await,
        Err(DriverError::Invalid { .. })
    ));
    let old = Cookie::new(1, "abc").unwrap();
    let driver = Then(info, Observation::Absent, Some(old));
    let err = converge(&driver, &request(), DEFAULT_TIMEOUT)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("version 1 cookie"), "{err}");
}

#[test]
fn cookie_size_limit() {
    Cookie::new(1, "x".repeat(MAX_COOKIE_BYTES)).unwrap();
    let err = Cookie::new(1, "x".repeat(MAX_COOKIE_BYTES + 1)).unwrap_err();
    assert!(err.to_string().contains("limit is 10240"), "{err}");
}

#[test]
fn call_timeouts() {
    let t = |v: Value, remaining: Option<u64>| {
        call_timeout(
            &object(json!({"timeout": v})),
            remaining.map(Duration::from_secs),
        )
    };
    assert_eq!(call_timeout(&Map::new(), None).unwrap(), DEFAULT_TIMEOUT);
    assert_eq!(t(Value::Null, None).unwrap(), DEFAULT_TIMEOUT);
    assert_eq!(t(json!(90), None).unwrap(), Duration::from_secs(90));
    assert_eq!(t(json!("90"), None).unwrap(), Duration::from_secs(90));
    assert_eq!(t(json!("90s"), None).unwrap(), Duration::from_secs(90));
    assert_eq!(t(json!("15m"), None).unwrap(), Duration::from_secs(900));
    assert_eq!(t(json!("1h"), None).unwrap(), Duration::from_secs(3600));
    // Bounded by the deployment's remaining deadline.
    assert_eq!(t(json!("1h"), Some(60)).unwrap(), Duration::from_secs(60));
    assert_eq!(
        call_timeout(&Map::new(), Some(Duration::from_secs(10))).unwrap(),
        Duration::from_secs(10)
    );
    for bad in [
        json!(0),
        json!(-5),
        json!(1.5),
        json!("0s"),
        json!("5d"),
        json!("m"),
        json!(true),
        json!("99999999999999999999h"),
    ] {
        assert!(t(bad.clone(), None).is_err(), "{bad}");
    }
}

#[test]
fn secret_maps_redact_their_values() {
    let secrets = SecretMap::new(object(json!({"password": "hunter2"})));
    let debug = format!("{secrets:?}");
    assert_eq!(debug, r#"{"password": "[REDACTED]"}"#);
    let mut req = request();
    req.secrets = secrets.clone();
    req.resource_secrets = secrets.clone();
    req.credentials = Some(secrets);
    assert!(!format!("{req:?}").contains("hunter2"));
}

#[tokio::test]
async fn echo_returns_driver_inputs() {
    // The `Static database instance` scenario.
    let mut req = request();
    req.resource_type = "postgres-instance".into();
    req.values = object(json!({"host": "db.example.com", "port": 5432, "name": "shop"}));
    req.secrets = SecretMap::new(object(json!({"username": "shop", "password": "hunter2"})));
    let echo = Echo::new();
    let (outputs, cookie) = converge(&echo, &req, DEFAULT_TIMEOUT).await.unwrap();
    assert_eq!(outputs.values, req.values);
    assert_eq!(outputs.secrets, req.secrets);
    assert!(outputs.manifests.is_empty());
    assert_eq!(cookie, None);
    assert_eq!(echo.plan(&req).await.unwrap(), Plan::NoEffects);
    remove(&echo, &req, DEFAULT_TIMEOUT).await.unwrap();
}

#[tokio::test]
async fn none_returns_resolved_resource_inputs() {
    // The `Workload without a runtime` scenario.
    let mut req = request();
    req.resource_type = "workload".into();
    req.resource = object(json!({
        "containers": {"main": {"image": "ghcr.io/acme/api:1.2.0", "variables": {"PORT": "8080"}}}
    }));
    req.resource_secrets = SecretMap::new(object(json!({"DB_PASS": "hunter2"})));
    req.values = object(json!({"ignored": true}));
    let none = NoneDriver::new();
    let (outputs, _) = converge(&none, &req, DEFAULT_TIMEOUT).await.unwrap();
    assert_eq!(outputs.values, req.resource);
    assert_eq!(outputs.secrets, req.resource_secrets);
    assert_eq!(none.plan(&req).await.unwrap(), Plan::NoEffects);
}

#[test]
fn registry_holds_builtins_and_reserves_their_namespace() {
    let mut registry = DriverRegistry::with_builtins();
    assert_eq!(
        registry.ids().collect::<Vec<_>>(),
        ["openporch/echo", "openporch/none"]
    );
    assert!(registry.get("openporch/echo").is_some());
    assert!(registry.get("acme/slow").is_none());

    let with_id = |id: &str| {
        let mut d = Slow::new(0);
        d.info.id = id.into();
        Arc::new(d) as Arc<dyn Driver>
    };
    registry.register(with_id("acme/slow")).unwrap();
    assert!(registry.get("acme/slow").is_some());
    assert_eq!(
        registry.register(with_id("acme/slow")),
        Err(RegistryError::Duplicate("acme/slow".into()))
    );
    assert_eq!(
        registry.register(with_id("openporch/slow")),
        Err(RegistryError::Reserved("openporch/slow".into()))
    );
    for bad in ["slow", "acme/", "Acme/slow", "acme/slow/x", "/slow"] {
        assert_eq!(
            registry.register(with_id(bad)),
            Err(RegistryError::InvalidId(bad.into())),
            "{bad}"
        );
    }
}
