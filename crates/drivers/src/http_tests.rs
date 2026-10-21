//! The HTTP adapter against a mock driver: every fixture in `fixtures/http`, then
//! duplicate requests, a polled provisioning and protocol checks.

use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use openporch_core::ids::Guresid;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::*;

const TOKEN: &str = "drv_secret_token";

#[derive(Debug, Clone)]
struct Recorded {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Value,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Canned {
    status: u16,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    body: Option<Value>,
}

/// Answers a request given the earlier ones.
type Responder = Box<dyn Fn(&Recorded, &[Recorded]) -> Canned + Send>;

/// A mock driver that records requests and answers from a queue, or from
/// `respond` when one is set.
#[derive(Default)]
struct Mock {
    recorded: Vec<Recorded>,
    queue: VecDeque<Canned>,
    respond: Option<Responder>,
}

type Shared = Arc<Mutex<Mock>>;

async fn handle(
    State(mock): State<Shared>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let rec = Recorded {
        method: method.to_string(),
        path: uri.path().to_owned(),
        headers: headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or_default().to_owned()))
            .collect(),
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
    };
    let mut mock = mock.lock().unwrap();
    let canned = match &mock.respond {
        Some(f) => f(&rec, &mock.recorded),
        None => mock.queue.pop_front().expect("an unexpected request"),
    };
    mock.recorded.push(rec);
    let mut res = match canned.body {
        Some(b) => axum::Json(b).into_response(),
        None => ().into_response(),
    };
    *res.status_mut() = StatusCode::from_u16(canned.status).unwrap();
    for (k, v) in canned.headers {
        res.headers_mut()
            .insert::<axum::http::HeaderName>(k.parse().unwrap(), v.parse().unwrap());
    }
    res
}

/// Serves a mock driver; returns its target URL.
async fn serve(mock: Shared) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new().fallback(handle).with_state(mock);
    tokio::spawn(async move { axum::serve(listener, app).await });
    format!("http://{addr}/drivers/pg")
}

fn info(caps: Capabilities) -> DriverInfo {
    DriverInfo {
        id: "acme/pg".into(),
        protocol_version: PROTOCOL_VERSION.into(),
        cookie_version: 1,
        input_schema: json!({"type": "object"}),
        output_schema: json!({"type": "object"}),
        capabilities: caps,
    }
}

fn object(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        other => panic!("not an object: {other}"),
    }
}

fn request(cookie: Option<Cookie>) -> DriverRequest {
    DriverRequest {
        guresid: Guresid::new("0123456789abcdef0123456789abcdef").unwrap(),
        operation: Operation {
            id: "op-1".into(),
            fingerprint: "f1".into(),
        },
        resource_type: "postgres".into(),
        resource: object(json!({"name": "shop"})),
        resource_secrets: SecretMap::new(object(json!({"admin_password": "hunter2"}))),
        resource_secret_paths: vec![vec!["admin_password".into()]],
        values: object(json!({"version": "16"})),
        secrets: SecretMap::new(object(json!({"master": "s3cret"}))),
        credentials: None,
        context: DriverContext::default(),
        cookie,
        permit: None,
        pin: None,
        log: LogSink::default(),
    }
}

fn error_json(e: &DriverError) -> Value {
    let mut v = match e {
        DriverError::Invalid { code, message } => {
            json!({"kind": "invalid", "code": code, "message": message})
        }
        DriverError::Conflict(m) => json!({"kind": "conflict", "message": m}),
        DriverError::Transient {
            message,
            retry_after,
        } => json!({"kind": "transient", "message": message,
                    "retry_after_secs": retry_after.map(|d| d.as_secs())}),
        DriverError::Timeout(d) => json!({"kind": "timeout", "secs": d.as_secs()}),
        DriverError::Uncertain(m) => json!({"kind": "uncertain", "message": m}),
        DriverError::Unsupported(m) => json!({"kind": "unsupported", "message": m}),
        DriverError::WithCookie { error, cookie } => {
            let mut v = error_json(error);
            v["cookie"] = serde_json::to_value(cookie).unwrap();
            v
        }
    };
    v["retryable"] = json!(e.is_retryable());
    v
}

fn cookie_json(c: &Option<Cookie>) -> Value {
    serde_json::to_value(c).unwrap()
}

fn outputs_json(o: &DriverOutputs, cookie: &Option<Cookie>) -> Value {
    json!({
        "values": o.values,
        "secrets": o.secrets.expose(),
        "manifests": o.manifests,
        "cookie": cookie_json(cookie),
    })
}

fn summary<T>(r: Result<T, DriverError>, f: impl FnOnce(T) -> Value) -> Value {
    match r {
        Ok(t) => f(t),
        Err(e) => json!({"error": error_json(&e)}),
    }
}

fn observation_json(o: Observation) -> Value {
    match o {
        Observation::Absent { cookie: None } => json!({"absent": null}),
        Observation::Absent { cookie } => json!({"absent": {"cookie": cookie_json(&cookie)}}),
        Observation::InProgress { poll_after, cookie } => {
            json!({"in_progress": {"poll_after_secs": poll_after.as_secs(), "cookie": cookie_json(&cookie)}})
        }
        Observation::Ready { outputs, cookie } => json!({"ready": outputs_json(&outputs, &cookie)}),
        Observation::Failed { error } => json!({"failed": error_json(&error)}),
        Observation::Unknown {
            reason,
            cookie: None,
        } => json!({"unknown": reason}),
        Observation::Unknown { reason, cookie } => {
            json!({"unknown": {"reason": reason, "cookie": cookie_json(&cookie)}})
        }
    }
}

#[derive(Deserialize)]
struct Fixture {
    call: String,
    #[serde(default)]
    cookie: Option<Cookie>,
    #[serde(default)]
    capabilities: Option<Value>,
    request: Expected,
    response: Canned,
    expect: Value,
}

#[derive(Deserialize)]
struct Expected {
    method: String,
    path: String,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    body: Option<Value>,
}

async fn run_fixture(name: &str, f: Fixture) {
    let mock = Shared::default();
    mock.lock().unwrap().queue.push_back(f.response);
    let target = serve(mock.clone()).await;
    let caps = f.capabilities.unwrap_or_default();
    let caps = Capabilities {
        recover: caps["recover"].as_bool().unwrap_or(false),
        cancel: caps["cancel"].as_bool().unwrap_or(false),
        ..Capabilities::default()
    };
    let driver = HttpDriver::new(info(caps), &target, Some(TOKEN.into())).unwrap();
    let req = request(f.cookie);
    let actual = match f.call.as_str() {
        "put" => summary(driver.put(&req).await, |o| match o {
            PutOutcome::Ready { outputs, cookie } => {
                json!({"ready": outputs_json(&outputs, &cookie)})
            }
            PutOutcome::InProgress { poll_after, cookie } => {
                json!({"in_progress": {"poll_after_secs": poll_after.as_secs(), "cookie": cookie_json(&cookie)}})
            }
        }),
        "get" => summary(driver.get(&req).await, observation_json),
        "cancel" => summary(driver.cancel(&req).await, observation_json),
        "delete" => summary(driver.delete(&req).await, |o| match o {
            DeleteOutcome::Deleted => json!({"deleted": null}),
            DeleteOutcome::InProgress { poll_after, cookie } => {
                json!({"in_progress": {"poll_after_secs": poll_after.as_secs(), "cookie": cookie_json(&cookie)}})
            }
        }),
        other => panic!("{name}: unknown call `{other}`"),
    };
    assert_eq!(actual, f.expect, "{name}");

    let recorded = mock.lock().unwrap().recorded.clone();
    assert_eq!(recorded.len(), 1, "{name}");
    let r = &recorded[0];
    assert_eq!(r.method, f.request.method, "{name}");
    assert_eq!(r.path, f.request.path, "{name}");
    // Every call carries the protocol, the operation identity and the token.
    let mut want = BTreeMap::from([
        ("openporch-protocol".to_owned(), PROTOCOL_VERSION.to_owned()),
        ("openporch-operation-id".to_owned(), "op-1".to_owned()),
        ("openporch-input-fingerprint".to_owned(), "f1".to_owned()),
        ("authorization".to_owned(), format!("Bearer {TOKEN}")),
    ]);
    want.extend(f.request.headers);
    for (k, v) in want {
        assert_eq!(r.headers.get(&k), Some(&v), "{name}: header {k}");
    }
    if req.cookie.is_none() {
        assert!(!r.headers.contains_key(COOKIE_HEADER), "{name}");
    }
    if let Some(body) = f.request.body {
        assert_eq!(r.body, body, "{name}");
    }
}

#[tokio::test]
async fn protocol_fixtures() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/http");
    let mut names: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    names.sort();
    assert!(names.len() >= 20);
    for path in names {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let text = std::fs::read_to_string(&path).unwrap();
        let fixture: Fixture =
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
        run_fixture(&name, fixture).await;
    }
}

#[tokio::test]
async fn duplicate_put_recovers_the_same_object() {
    // The Duplicate put scenario: a driver that deduplicates by operation ID.
    let mock = Shared::default();
    mock.lock().unwrap().respond = Some(Box::new(|rec, earlier| {
        let op = &rec.headers["openporch-operation-id"];
        let fp = &rec.headers["openporch-input-fingerprint"];
        match earlier
            .iter()
            .find(|e| &e.headers["openporch-operation-id"] == op)
        {
            // A changed fingerprint under the same operation ID is refused.
            Some(e) if &e.headers["openporch-input-fingerprint"] != fp => Canned {
                status: 409,
                body: Some(json!({"message": "fingerprint changed"})),
                ..Canned::default()
            },
            _ => Canned {
                status: 200,
                body: Some(json!({
                    "id": "0123456789abcdef0123456789abcdef",
                    "resource": {"values": {"arn": format!("arn:db:{op}")}}
                })),
                ..Canned::default()
            },
        }
    }));
    let target = serve(mock.clone()).await;
    let caps = Capabilities {
        recover: true,
        ..Capabilities::default()
    };
    let driver = HttpDriver::new(info(caps), &target, None).unwrap();
    let req = request(None);
    let first = converge(&driver, &req, DEFAULT_TIMEOUT)
        .await
        .result
        .unwrap();
    // The response was lost; the same operation is sent again.
    let second = converge(&driver, &req, DEFAULT_TIMEOUT)
        .await
        .result
        .unwrap();
    assert_eq!(first, second);
    assert_eq!(first.values["arn"], "arn:db:op-1");
    let recorded = mock.lock().unwrap().recorded.clone();
    assert_eq!(recorded.len(), 2);
    assert_eq!(recorded[0].headers, recorded[1].headers);
    assert_eq!(recorded[0].body, recorded[1].body);
    assert!(!recorded[0].headers.contains_key("authorization"));

    let mut changed = req.clone();
    changed.operation.fingerprint = "f2".into();
    let err = converge(&driver, &changed, DEFAULT_TIMEOUT)
        .await
        .result
        .unwrap_err();
    // The conflict is observed, not repeated into.
    assert_eq!(
        err,
        DriverError::Uncertain(
            "the driver reports a conflicting operation (409), so the resource is observed \
             before it is changed again: fingerprint changed"
                .into()
        )
    );
}

#[tokio::test]
async fn an_unreadable_mutation_response_keeps_its_cookie() {
    let new = Cookie::new(1, "op-2").unwrap();
    let cookie = || {
        (
            "set-openporch-driver-cookie".to_owned(),
            "1.b3AtMg".to_owned(),
        )
    };
    let replies = [
        // Larger than a response may be.
        Canned {
            status: 200,
            headers: BTreeMap::from([cookie()]),
            body: Some(json!({"padding": "x".repeat(5 << 20)})),
        },
        // In another protocol.
        Canned {
            status: 200,
            headers: BTreeMap::from([cookie(), ("openporch-protocol".into(), "99".into())]),
            body: Some(json!({})),
        },
    ];
    for (call, reply) in ["put", "delete"]
        .into_iter()
        .flat_map(|call| replies.iter().map(move |r| (call, r)))
    {
        let mock = Shared::default();
        mock.lock().unwrap().queue.push_back(reply.clone());
        let target = serve(mock.clone()).await;
        let driver = HttpDriver::new(info(Capabilities::default()), &target, None).unwrap();
        let req = request(Some(Cookie::new(1, "op-1").unwrap()));
        let settled = match call {
            "put" => {
                let s = converge(&driver, &req, DEFAULT_TIMEOUT).await;
                Settled {
                    result: s.result.map(|_| ()),
                    cookie: s.cookie,
                }
            }
            _ => remove(&driver, &req, DEFAULT_TIMEOUT).await,
        };
        let err = settled.result.unwrap_err();
        assert!(matches!(err, DriverError::Uncertain(_)), "{call}: {err:?}");
        assert_eq!(settled.cookie, Some(new.clone()), "{call}");
    }
}

// Real time: paused time would jump to the deadline while waiting on sockets.
#[tokio::test]
async fn a_failed_poll_keeps_the_cookie_it_returned() {
    let polled = [
        (500, false, Some(json!({"message": "busy"}))),
        (200, false, Some(json!({"unexpected": true}))),
        (422, false, Some(json!({"message": "rejected"}))),
        // The driver's own 404, and one from anything else.
        (404, true, Some(json!({"message": "gone"}))),
        (404, false, None),
    ];
    for (status, spoken, body) in polled {
        let mut headers =
            BTreeMap::from([("set-openporch-driver-cookie".into(), "1.b3AtMg".into())]);
        if spoken {
            headers.insert("openporch-protocol".into(), PROTOCOL_VERSION.into());
        }
        let mock = Shared::default();
        {
            let mut m = mock.lock().unwrap();
            m.queue.push_back(Canned {
                status: 202,
                headers: BTreeMap::from([
                    ("retry-after".into(), "1".into()),
                    ("set-openporch-driver-cookie".into(), "1.b3AtMQ".into()),
                ]),
                body: None,
            });
            m.queue.push_back(Canned {
                status,
                headers,
                body,
            });
        }
        let target = serve(mock.clone()).await;
        let driver = HttpDriver::new(info(Capabilities::default()), &target, None).unwrap();
        let settled = converge(&driver, &request(None), DEFAULT_TIMEOUT).await;
        let err = settled.result.unwrap_err();
        assert!(
            !matches!(err, DriverError::WithCookie { .. }),
            "{status}: {err:?}"
        );
        assert!(!err.to_string().contains("op-2"), "{err}");
        // The poll's cookie, not the 202's, is what recovery observes with.
        assert_eq!(
            settled.cookie,
            Some(Cookie::new(1, "op-2").unwrap()),
            "{status}"
        );
    }
}

// Real time: paused time would jump to the deadline while waiting on sockets.
#[tokio::test]
async fn asynchronous_provisioning_over_http() {
    let mock = Shared::default();
    {
        let mut m = mock.lock().unwrap();
        m.queue.push_back(Canned {
            status: 202,
            headers: BTreeMap::from([
                ("retry-after".into(), "1".into()),
                ("set-openporch-driver-cookie".into(), "1.b3AtMQ".into()),
            ]),
            body: None,
        });
        m.queue.push_back(Canned {
            status: 202,
            headers: BTreeMap::from([("retry-after".into(), "1".into())]),
            body: None,
        });
        m.queue.push_back(Canned {
            status: 200,
            body: Some(json!({"id": "0123456789abcdef0123456789abcdef",
                              "resource": {"values": {"host": "pg"}}})),
            ..Canned::default()
        });
    }
    let target = serve(mock.clone()).await;
    let driver = HttpDriver::new(info(Capabilities::default()), &target, None).unwrap();
    let settled = converge(&driver, &request(None), DEFAULT_TIMEOUT).await;
    assert_eq!(settled.result.unwrap().values["host"], "pg");
    assert_eq!(settled.cookie, Some(Cookie::new(1, "op-1").unwrap()));
    let recorded = mock.lock().unwrap().recorded.clone();
    let calls: Vec<_> = recorded.iter().map(|r| r.method.as_str()).collect();
    assert_eq!(calls, ["PUT", "GET", "GET"]);
    // Polls carry the cookie from the 202.
    assert_eq!(recorded[1].headers[COOKIE_HEADER], "1.b3AtMQ");
}

#[tokio::test]
async fn a_failed_mutation_keeps_the_cookie_it_returned() {
    let old = Cookie::new(1, "op-1").unwrap();
    let new = Cookie::new(1, "op-2").unwrap();
    for (status, call) in [
        (409, "put"),
        (503, "put"),
        (409, "delete"),
        (500, "delete"),
        // A 404 without `Openporch-Protocol` is uncertain, and its cookie is
        // still kept.
        (404, "delete"),
    ] {
        let mock = Shared::default();
        mock.lock().unwrap().queue.push_back(Canned {
            status,
            headers: BTreeMap::from([("set-openporch-driver-cookie".into(), "1.b3AtMg".into())]),
            body: Some(json!({"message": "busy"})),
        });
        let target = serve(mock.clone()).await;
        let driver = HttpDriver::new(info(Capabilities::default()), &target, None).unwrap();
        let req = request(Some(old.clone()));
        let settled = match call {
            "put" => {
                let s = converge(&driver, &req, DEFAULT_TIMEOUT).await;
                Settled {
                    result: s.result.map(|_| ()),
                    cookie: s.cookie,
                }
            }
            _ => remove(&driver, &req, DEFAULT_TIMEOUT).await,
        };
        // The outcome is uncertain, the error carries no cookie of its own,
        // and the new cookie is what recovery observes with.
        let err = settled.result.unwrap_err();
        assert!(
            matches!(err, DriverError::Uncertain(_)),
            "{call} {status}: {err:?}"
        );
        assert_eq!(settled.cookie, Some(new.clone()), "{call} {status}");
        // The cookie's data never shows in the error.
        assert!(!err.to_string().contains("op-2"), "{err}");
    }

    // A cookie in a version the driver does not declare cannot be kept: the
    // outcome is uncertain even for a failure that was definite, and the old
    // cookie stays.
    let mock = Shared::default();
    mock.lock().unwrap().queue.push_back(Canned {
        status: 400,
        headers: BTreeMap::from([("set-openporch-driver-cookie".into(), "2.b3AtMg".into())]),
        body: Some(json!({"message": "bad input"})),
    });
    let target = serve(mock.clone()).await;
    let driver = HttpDriver::new(info(Capabilities::default()), &target, None).unwrap();
    let settled = converge(&driver, &request(Some(old.clone())), DEFAULT_TIMEOUT).await;
    assert_eq!(
        settled.result.unwrap_err(),
        DriverError::Uncertain(
            "the driver returned a version 2 cookie but declares version 1".into()
        )
    );
    assert_eq!(settled.cookie, Some(old));
}

#[test]
fn unsupported_protocol_versions_and_targets_are_refused() {
    let mut legacy = info(Capabilities::default());
    legacy.protocol_version = "humanitec/0.9.3".into();
    assert_eq!(
        HttpDriver::new(legacy, "https://driver.internal/x", None).unwrap_err(),
        HttpDriverError::UnsupportedProtocol {
            id: "acme/pg".into(),
            version: "humanitec/0.9.3".into()
        }
    );
    for bad in [
        "ftp://driver.internal/x",
        "https://user:pw@driver.internal/x",
        "https://driver.internal/x?token=secret1",
        "https://driver.internal/x#frag",
        "not a url",
    ] {
        let err = HttpDriver::new(info(Capabilities::default()), bad, None).unwrap_err();
        assert!(matches!(err, HttpDriverError::InvalidTarget(_)), "{bad}");
        // The error never repeats the URL, which may hold credentials.
        assert!(!err.to_string().contains("driver.internal"), "{err}");
    }
    check_target("HTTPS://driver.internal/x/").unwrap();
}

#[tokio::test]
async fn connection_failures_are_transient_and_tokens_are_not_printed() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let driver = HttpDriver::new(
        info(Capabilities::default()),
        &format!("http://{addr}/x"),
        Some(TOKEN.into()),
    )
    .unwrap();
    // Nothing reached the driver, so even a mutation may be retried.
    let err = driver.put(&request(None)).await.unwrap_err();
    assert!(err.is_retryable(), "{err:?}");
    assert!(!format!("{driver:?}").contains(TOKEN));
}

/// A driver that reads each request and then fails as `mode` says: closing the
/// connection without answering, or answering with an oversized body.
async fn broken(mode: &'static str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = vec![0; 64 * 1024];
                let _ = sock.read(&mut buf).await;
                if mode == "huge" {
                    let size = 5 * 1024 * 1024;
                    let head = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {size}\r\n\r\n"
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(&vec![b' '; size]).await;
                }
                // "drop": close without answering.
            });
        }
    });
    format!("http://{addr}/drivers/pg")
}

#[tokio::test]
async fn lost_or_oversized_responses_to_mutations_are_uncertain() {
    for mode in ["drop", "huge"] {
        let target = broken(mode).await;
        let driver = HttpDriver::new(info(Capabilities::default()), &target, None).unwrap();
        let req = request(None);
        // The driver read the request, so it may have acted.
        let err = driver.put(&req).await.unwrap_err();
        assert!(matches!(err, DriverError::Uncertain(_)), "{mode}: {err:?}");
        let err = driver.delete(&req).await.unwrap_err();
        assert!(matches!(err, DriverError::Uncertain(_)), "{mode}: {err:?}");
        // A read changes nothing, so it may be retried.
        let err = driver.get(&req).await.unwrap_err();
        assert!(err.is_retryable(), "{mode}: {err:?}");
    }
}

#[tokio::test]
async fn cancel_needs_the_capability() {
    let driver =
        HttpDriver::new(info(Capabilities::default()), "http://127.0.0.1:9/x", None).unwrap();
    assert_eq!(
        driver.cancel(&request(None)).await.unwrap_err(),
        DriverError::Unsupported("cancellation".into())
    );
}
