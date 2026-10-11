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
    /// The cookie `get` returns with the outputs.
    ready_cookie: Option<&'static str>,
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
            ready_cookie: None,
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
                    cookie: None,
                });
            }
            Ok(Observation::Ready {
                outputs: DriverOutputs {
                    values: object(json!({"host": "db.internal"})),
                    ..DriverOutputs::default()
                },
                cookie: self.ready_cookie.map(|c| Cookie::new(1, c)).transpose()?,
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
    let settled = converge(&driver, &request(), DEFAULT_TIMEOUT).await;
    let outputs = settled.result.unwrap();
    assert_eq!(outputs.values, object(json!({"host": "db.internal"})));
    // `get` returned no cookie, so the one from `put` stands.
    assert_eq!(settled.cookie, Some(Cookie::new(1, "abc").unwrap()));
    // Three polls, each after the 30 seconds the driver asked for.
    assert_eq!(start.elapsed(), Duration::from_secs(90));
    let calls: Vec<_> = driver.seen().into_iter().map(|(c, _)| c).collect();
    assert_eq!(calls, ["put", "get", "get", "get"]);
}

#[tokio::test(start_paused = true)]
async fn cookie_round_trip() {
    let mut driver = Slow::new(1);
    driver.ready_cookie = Some("def");
    let first = converge(&driver, &request(), DEFAULT_TIMEOUT).await;
    first.result.unwrap();
    // A newer cookie from `get` replaces the one from `put`.
    assert_eq!(first.cookie, Some(Cookie::new(1, "def").unwrap()));

    // The checkpointed cookie goes with the next put and delete for the GUResID.
    let mut next = request();
    next.cookie = first.cookie;
    driver.ready_cookie = None;
    let second = converge(&driver, &next, DEFAULT_TIMEOUT).await;
    second.result.unwrap();
    next.cookie = second.cookie;
    remove(&driver, &next, DEFAULT_TIMEOUT)
        .await
        .result
        .unwrap();
    let c = |s: &str| Some(s.to_owned());
    assert_eq!(
        driver.seen(),
        [
            ("put", None),
            ("get", c("abc")),
            ("get", c("abc")),
            ("put", c("def")),
            ("get", c("abc")),
            ("delete", c("abc")),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn polling_stops_at_the_deadline_and_keeps_the_cookie() {
    let driver = Slow::new(usize::MAX);
    let start = tokio::time::Instant::now();
    let settled = converge(&driver, &request(), Duration::from_secs(100)).await;
    let err = settled.result.unwrap_err();
    assert_eq!(err, DriverError::Timeout(Duration::from_secs(100)));
    assert!(!err.is_retryable());
    // Recovery needs the cookie of the operation that timed out.
    assert_eq!(settled.cookie, Some(Cookie::new(1, "abc").unwrap()));
    // Polls at 30, 60 and 90 seconds, and a last one 5 seconds before the deadline.
    assert_eq!(start.elapsed(), Duration::from_secs(95));
    assert_eq!(driver.seen().len(), 5);
}

/// A driver whose calls never return, and whose `cancel` reports the given
/// observation.
#[derive(Debug)]
struct Hung(DriverInfo, Option<Observation>);

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

    fn cancel<'a>(&'a self, _req: &'a DriverRequest) -> DriverFuture<'a, Observation> {
        let o = self.1.clone();
        Box::pin(async move {
            match o {
                Some(o) => Ok(o),
                None => std::future::pending().await,
            }
        })
    }
}

#[tokio::test(start_paused = true)]
async fn a_driver_that_does_not_answer_times_out() {
    let driver = Hung(Echo::new().info().clone(), None);
    let timeout = Duration::from_secs(5);
    let err = converge(&driver, &request(), timeout)
        .await
        .result
        .unwrap_err();
    assert_eq!(err, DriverError::Timeout(timeout));
    assert_eq!(
        remove(&driver, &request(), timeout)
            .await
            .result
            .unwrap_err(),
        DriverError::Timeout(timeout)
    );
    assert_eq!(err.to_string(), "the driver did not finish within 5s");
    assert_eq!(
        DriverError::Timeout(Duration::from_millis(500)).to_string(),
        "the driver did not finish within 500ms"
    );
}

#[tokio::test(start_paused = true)]
async fn a_timeout_cancels_drivers_that_can() {
    let mut info = Echo::new().info().clone();
    info.capabilities.cancel = true;
    let timeout = Duration::from_secs(5);

    // A confirmed cancellation leaves a plain timeout.
    let stopped = Hung(info.clone(), Some(Observation::Absent));
    let err = converge(&stopped, &request(), timeout)
        .await
        .result
        .unwrap_err();
    assert_eq!(err, DriverError::Timeout(timeout));

    // Otherwise the outcome is uncertain and must be recovered.
    for cancel in [
        Some(Observation::InProgress {
            poll_after: Duration::ZERO,
            cookie: None,
        }),
        Some(Observation::Unknown { reason: "?".into() }),
        None,
    ] {
        let driver = Hung(info.clone(), cancel);
        let err = converge(&driver, &request(), timeout)
            .await
            .result
            .unwrap_err();
        assert!(matches!(err, DriverError::Uncertain(_)), "{err}");
    }
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
        let cookie = self.2.clone();
        Box::pin(async move {
            Ok(DeleteOutcome::InProgress {
                poll_after: Duration::ZERO,
                cookie,
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
        Some(Cookie::new(0, "op-7").unwrap()),
    );
    let r = request();
    let settled = converge(&failed, &r, DEFAULT_TIMEOUT).await;
    assert_eq!(settled.result.unwrap_err(), invalid);
    assert_eq!(settled.cookie, Some(Cookie::new(0, "op-7").unwrap()));
    assert_eq!(invalid.to_string(), "PG-1: bad version");

    let absent = Then(info.clone(), Observation::Absent, None);
    assert!(matches!(
        converge(&absent, &r, DEFAULT_TIMEOUT).await.result,
        Err(DriverError::Uncertain(_))
    ));
    // For a deletion, absent is done.
    remove(&absent, &r, DEFAULT_TIMEOUT).await.result.unwrap();

    let unknown = Then(
        info,
        Observation::Unknown {
            reason: "lost the operation".into(),
        },
        Some(Cookie::new(0, "op-8").unwrap()),
    );
    assert_eq!(
        converge(&unknown, &r, DEFAULT_TIMEOUT)
            .await
            .result
            .unwrap_err(),
        DriverError::Uncertain("lost the operation".into())
    );
    let settled = remove(&unknown, &r, DEFAULT_TIMEOUT).await;
    assert_eq!(
        settled.result.unwrap_err(),
        DriverError::Uncertain("lost the operation".into())
    );
    assert_eq!(settled.cookie, Some(Cookie::new(0, "op-8").unwrap()));
}

/// A driver that answers every call at once with `poll_after` and never finishes.
#[derive(Debug)]
struct Busy(DriverInfo, Duration);

impl Driver for Busy {
    fn info(&self) -> &DriverInfo {
        &self.0
    }

    fn put<'a>(&'a self, _req: &'a DriverRequest) -> DriverFuture<'a, PutOutcome> {
        let poll_after = self.1;
        Box::pin(async move {
            Ok(PutOutcome::InProgress {
                poll_after,
                cookie: None,
            })
        })
    }

    fn get<'a>(&'a self, _req: &'a DriverRequest) -> DriverFuture<'a, Observation> {
        let poll_after = self.1;
        Box::pin(async move {
            Ok(Observation::InProgress {
                poll_after,
                cookie: None,
            })
        })
    }

    fn delete<'a>(&'a self, _req: &'a DriverRequest) -> DriverFuture<'a, DeleteOutcome> {
        Box::pin(async { Ok(DeleteOutcome::Deleted) })
    }
}

#[tokio::test(start_paused = true)]
async fn extreme_waits_and_timeouts_do_not_overflow() {
    let info = Echo::new().info().clone();
    let start = tokio::time::Instant::now();
    let driver = Busy(info.clone(), Duration::MAX);
    let err = converge(&driver, &request(), Duration::MAX)
        .await
        .result
        .unwrap_err();
    assert_eq!(err, DriverError::Timeout(MAX_TIMEOUT));
    assert!(start.elapsed() <= MAX_TIMEOUT);

    // A zero wait is raised to the minimum poll interval.
    let start = tokio::time::Instant::now();
    let driver = Busy(info, Duration::ZERO);
    converge(&driver, &request(), Duration::from_secs(10))
        .await
        .result
        .unwrap_err();
    assert!(start.elapsed() >= Duration::from_secs(9));
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
    let settled = converge(&driver, &request(), DEFAULT_TIMEOUT).await;
    // An operation may be running, so the outcome is uncertain rather than invalid.
    assert!(matches!(settled.result, Err(DriverError::Uncertain(_))));
    assert_eq!(settled.cookie, None);
    let old = Cookie::new(1, "abc").unwrap();
    let driver = Then(info, Observation::Absent, Some(old));
    let err = converge(&driver, &request(), DEFAULT_TIMEOUT)
        .await
        .result
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
    assert_eq!(t(json!(90.0), None).unwrap(), Duration::from_secs(90));
    assert_eq!(t(json!("90"), None).unwrap(), Duration::from_secs(90));
    assert_eq!(t(json!("90s"), None).unwrap(), Duration::from_secs(90));
    assert_eq!(t(json!("15m"), None).unwrap(), Duration::from_secs(900));
    assert_eq!(t(json!("1h"), None).unwrap(), Duration::from_secs(3600));
    assert_eq!(t(json!("24h"), None).unwrap(), MAX_TIMEOUT);
    // Bounded by the deployment's remaining deadline.
    assert_eq!(t(json!("1h"), Some(60)).unwrap(), Duration::from_secs(60));
    assert_eq!(
        call_timeout(&Map::new(), Some(Duration::from_secs(10))).unwrap(),
        Duration::from_secs(10)
    );
    // A spent deadline is not a timeout: nothing was called.
    assert!(matches!(
        call_timeout(&Map::new(), Some(Duration::ZERO)),
        Err(DriverError::Invalid { .. })
    ));
    for bad in [
        json!(0),
        json!(-5),
        json!(1.5),
        json!(u64::MAX),
        json!(1e300),
        json!("0s"),
        json!("25h"),
        json!("5d"),
        json!("m"),
        json!(true),
        json!("99999999999999999999h"),
    ] {
        assert!(t(bad.clone(), None).is_err(), "{bad}");
    }
}

#[test]
fn cookies_and_manifests_are_not_printed() {
    let mut req = request();
    req.cookie = Some(Cookie::new(1, "generated-password").unwrap());
    let debug = format!("{req:?}");
    assert!(!debug.contains("generated-password"), "{debug}");
    assert!(debug.contains("bytes: 18"), "{debug}");
    let outputs = DriverOutputs {
        manifests: vec![Manifest {
            location: "namespace".into(),
            data: json!({"kind": "Secret", "data": {"password": "aHVudGVyMg=="}}),
        }],
        ..DriverOutputs::default()
    };
    let debug = format!("{outputs:?}");
    assert!(!debug.contains("aHVudGVyMg"), "{debug}");
    assert!(debug.contains("Secret"), "{debug}");
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
    let settled = converge(&echo, &req, DEFAULT_TIMEOUT).await;
    let outputs = settled.result.unwrap();
    assert_eq!(outputs.values, req.values);
    assert_eq!(outputs.secrets, req.secrets);
    assert!(outputs.manifests.is_empty());
    assert_eq!(settled.cookie, None);
    assert_eq!(echo.plan(&req).await.unwrap(), Plan::NoEffects);
    remove(&echo, &req, DEFAULT_TIMEOUT).await.result.unwrap();
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
    let outputs = converge(&none, &req, DEFAULT_TIMEOUT).await.result.unwrap();
    assert_eq!(outputs.values, req.resource);
    assert_eq!(outputs.secrets, req.resource_secrets);
    assert_eq!(none.plan(&req).await.unwrap(), Plan::NoEffects);
}

#[test]
fn registry_holds_builtins_and_reserves_their_namespace() {
    let mut registry = DriverRegistry::with_builtins();
    assert_eq!(
        registry.ids().collect::<Vec<_>>(),
        ["openporch/echo", "openporch/none", "openporch/template"]
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

fn template_request(templates: Value) -> DriverRequest {
    let mut req = request();
    req.values = object(json!({"templates": templates, "port": 5432}));
    req.secrets = SecretMap::new(object(json!({"admin": "s3cret"})));
    req.resource = object(json!({"name": "shop"}));
    req
}

#[tokio::test]
async fn template_keeps_a_generated_password_in_its_cookie() {
    // The `Deterministic password` scenario.
    let templates = json!({
        "init": "password: {{ cookie.password | default(random_alphanumeric(24)) | tojson }}\n",
        "outputs": "host: {{ resource.name }}.db.internal\nport: {{ driver.values.port }}\nid: {{ guresid }}\n",
        "secrets": "password: {{ init.password | tojson }}\nadmin: {{ driver.secrets.admin | tojson }}\n",
        "cookie": "password: {{ init.password | tojson }}\n",
    });
    let driver = Template::new();
    let req = template_request(templates);
    let first = converge(&driver, &req, DEFAULT_TIMEOUT).await;
    let outputs = first.result.unwrap();
    assert_eq!(
        outputs.values,
        object(json!({
            "host": "shop.db.internal",
            "port": 5432,
            "id": "0123456789abcdef0123456789abcdef",
        }))
    );
    let password = outputs.secrets.expose()["password"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(password.len(), 24);
    assert_eq!(outputs.secrets.expose()["admin"], "s3cret");

    // Without the cookie, a new password is generated.
    let fresh = converge(&driver, &req, DEFAULT_TIMEOUT)
        .await
        .result
        .unwrap();
    assert_ne!(fresh.secrets.expose()["password"], password.as_str());

    // With it, the second deployment returns the same password.
    let mut second = req.clone();
    second.cookie = first.cookie;
    let again = converge(&driver, &second, DEFAULT_TIMEOUT).await;
    assert_eq!(
        again.result.unwrap().secrets.expose()["password"],
        password.as_str()
    );
    assert_eq!(again.cookie, second.cookie);
}

#[tokio::test]
async fn template_emits_manifests() {
    let templates = json!({
        "manifests": r#"
deployment.yaml:
  location: namespace
  data:
    apiVersion: apps/v1
    kind: Deployment
    metadata:
      name: {{ resource.name }}
    spec:
      replicas: {{ driver.values.port // 1000 }}
secret.yaml:
  location: cluster
  data:
    apiVersion: v1
    kind: Secret
    metadata:
      name: {{ resource.name }}-admin
    data:
      admin: {{ driver.secrets.admin | b64encode }}
"#,
    });
    let driver = Template::new();
    let outputs = converge(&driver, &template_request(templates), DEFAULT_TIMEOUT)
        .await
        .result
        .unwrap();
    assert_eq!(outputs.manifests.len(), 2);
    let deployment = &outputs.manifests[0];
    assert_eq!(deployment.location, "namespace");
    assert_eq!(deployment.data["metadata"]["name"], "shop");
    assert_eq!(deployment.data["spec"]["replicas"], 5);
    assert_eq!(outputs.manifests[1].data["data"]["admin"], "czNjcmV0");
    assert_eq!(outputs.manifests[1].location, "cluster");
    assert!(outputs.values.is_empty());
    assert!(!format!("{outputs:?}").contains("czNjcmV0"));
}

#[tokio::test]
async fn template_errors_name_the_template_but_not_secrets() {
    let driver = Template::new();
    let err = |templates: Value| {
        let req = template_request(templates);
        let driver = &driver;
        async move { driver.put(&req).await.unwrap_err().to_string() }
    };

    let e = err(json!({"outputs": "a: {{ driver.values.missing }}"})).await;
    assert_eq!(
        e,
        "TPL-001: the `outputs` template uses an undefined value at line 1"
    );
    // YAML errors give a position, not the rendered text.
    let e = err(json!({"secrets": "admin: [{{ driver.secrets.admin }}"})).await;
    assert!(
        e.starts_with("TPL-001: the `secrets` template rendered invalid YAML"),
        "{e}"
    );
    assert!(!e.contains("s3cret"), "{e}");
    let e = err(json!({"secrets": "{{ driver.secrets.admin }}"})).await;
    assert_eq!(e, "TPL-001: the `secrets` template must render a YAML map");
    let e = err(json!({"outputs": "a: {{ 1 + driver.secrets.admin }}"})).await;
    assert!(!e.contains("s3cret"), "{e}");
    let e = err(json!({"outputs": "{% for i in range(100000) %}{% for j in range(100000) %}{% endfor %}{% endfor %}"})).await;
    assert!(e.contains("ran too long"), "{e}");
    let e = err(json!({"outputs": "a: {{ random_alphanumeric(0) }}"})).await;
    assert!(
        e.contains("random_alphanumeric takes a length from 1 to 1024"),
        "{e}"
    );
    let e = err(json!({"output": "a: 1"})).await;
    assert!(e.contains("unknown template `output`"), "{e}");
    // Manifest names are rendered text too, so errors give positions.
    let e = err(json!({"manifests": "ok:\n  location: volume\n  data: {}\n{{ driver.secrets.admin }}:\n  location: everywhere\n  data: {}"})).await;
    assert!(e.contains("manifest 2 needs a `location`"), "{e}");
    assert!(!e.contains("s3cret"), "{e}");
    // Output is bounded, so a template cannot grow the response without limit.
    let e = err(json!({"outputs": "{% for i in range(5000) %}{{ 'x' * 1000 }}{% endfor %}"})).await;
    assert!(e.contains("rendered more than 1048576 bytes"), "{e}");
    let e = err(json!({"outputs": "a: {{ 'not base64!' | b64decode }}"})).await;
    assert!(e.contains("b64decode: not base64"), "{e}");
    let mut req = template_request(json!({"outputs": "a: 1"}));
    req.cookie = Some(Cookie::new(1, "not json").unwrap());
    let e = driver.put(&req).await.unwrap_err().to_string();
    assert!(e.contains("cannot read the version 1 cookie"), "{e}");
    req.cookie = Some(Cookie::new(2, "{}").unwrap());
    let e = driver.put(&req).await.unwrap_err().to_string();
    assert!(e.contains("cannot read the version 2 cookie"), "{e}");
    let mut req = request();
    req.values = Map::new();
    assert!(driver.put(&req).await.is_err());
}

#[tokio::test]
async fn template_docs_example_keeps_its_password() {
    let doc = include_str!("../../../docs/drivers/template.md");
    let example = doc
        .split("```yaml\n")
        .filter_map(|b| b.split_once("```").map(|(b, _)| b))
        .find(|b| b.contains("driver_type: openporch/template"))
        .unwrap();
    let def: Value = serde_yaml_ng::from_str(example).unwrap();
    let mut req = template_request(def["driver_inputs"]["values"]["templates"].clone());
    req.resource_type = "postgres".into();
    let driver = Template::new();
    let first = converge(&driver, &req, DEFAULT_TIMEOUT).await;
    let password = first.result.unwrap().secrets.expose()["password"].clone();
    req.cookie = first.cookie;
    let second = converge(&driver, &req, DEFAULT_TIMEOUT)
        .await
        .result
        .unwrap();
    assert_eq!(second.secrets.expose()["password"], password);
    assert_eq!(
        second.values,
        object(json!({"host": "shop.db.internal", "port": 5432}))
    );
}
