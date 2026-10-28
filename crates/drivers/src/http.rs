//! The external HTTP driver adapter: maps the [`Driver`] trait onto the
//! `openporch-http/1` profile, documented in `docs/drivers/http.md`. The request
//! and response bodies follow the shapes of Humanitec's Driver API 0.9.3; cookies,
//! operation identity and cancellation use openporch headers and endpoints.

use std::fmt;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use openporch_core::redact::Redactor;
use reqwest::header::{AUTHORIZATION, HeaderMap, RETRY_AFTER};
use reqwest::{Client, Method, StatusCode, Url};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::{
    Cookie, DeleteOutcome, Driver, DriverError, DriverFuture, DriverInfo, DriverOutputs,
    DriverRequest, Manifest, Observation, PutOutcome, SecretMap,
};

/// The protocol profile this adapter speaks.
pub const PROTOCOL_VERSION: &str = "openporch-http/1";
/// Sent with every request, and checked on responses that carry it.
pub const PROTOCOL_HEADER: &str = "openporch-protocol";
/// The durable operation ID of the call.
pub const OPERATION_HEADER: &str = "openporch-operation-id";
/// The fingerprint of the call's inputs.
pub const FINGERPRINT_HEADER: &str = "openporch-input-fingerprint";
/// The cookie from the previous call, as `<version>.<base64url data>`.
pub const COOKIE_HEADER: &str = "openporch-driver-cookie";
/// A new cookie in a response, in the same format.
pub const SET_COOKIE_HEADER: &str = "set-openporch-driver-cookie";

/// The poll interval when a `202` names none.
const DEFAULT_POLL: Duration = Duration::from_secs(10);
/// The longest `Retry-After` honored.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(3600);
/// The largest response body read.
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;
/// The longest driver error message passed on, in characters.
const MAX_MESSAGE_CHARS: usize = 1000;
/// How long to wait for a connection; a driver that cannot be reached in this
/// time was not called, so the call may be retried.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Checks an external driver's target URL: `http` or `https` with a host, and no
/// credentials, query or fragment, since the GUResID is appended to its path.
/// The error never repeats the URL, which may hold credentials.
pub fn check_target(target: &str) -> Result<Url, HttpDriverError> {
    let url = Url::parse(target).map_err(|_| HttpDriverError::InvalidTarget("it is not a URL"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(HttpDriverError::InvalidTarget(
            "it must be an http or https URL with a host",
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(HttpDriverError::InvalidTarget(
            "it must not contain credentials; use `token`",
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(HttpDriverError::InvalidTarget(
            "it must not contain a query or fragment",
        ));
    }
    Ok(url)
}

/// The client [`HttpDriver::new`] builds: redirects are not followed, and,
/// when `https_only`, an `https` target is never left for plain HTTP. One
/// client can serve many drivers, and keeps their connections.
pub fn default_client(https_only: bool) -> Result<Client, HttpDriverError> {
    // `ring` is the crypto provider; installing it twice is harmless.
    let _ = rustls::crypto::ring::default_provider().install_default();
    Client::builder()
        .user_agent(concat!("openporch/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(CONNECT_TIMEOUT)
        .https_only(https_only)
        .build()
        .map_err(|e| HttpDriverError::Client(e.to_string()))
}

/// Why an external driver cannot be set up.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HttpDriverError {
    #[error("driver `{id}` uses protocol `{version}`; this server supports `{PROTOCOL_VERSION}`")]
    UnsupportedProtocol { id: String, version: String },
    #[error("the driver target is invalid: {0}")]
    InvalidTarget(&'static str),
    #[error("building the HTTP client: {0}")]
    Client(String),
}

/// A driver reached over HTTP.
pub struct HttpDriver {
    info: DriverInfo,
    target: Url,
    token: Option<String>,
    client: Client,
}

impl fmt::Debug for HttpDriver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpDriver")
            .field("info", &self.info)
            .field("target", &self.target.as_str())
            .field("token", &self.token.as_ref().map(|_| "[REDACTED]"))
            .finish_non_exhaustive()
    }
}

impl HttpDriver {
    /// An adapter for the driver at `target`, authenticating with `token` when
    /// set. Redirects are not followed, and an `https` target is never left for
    /// plain HTTP.
    pub fn new(
        info: DriverInfo,
        target: &str,
        token: Option<String>,
    ) -> Result<Self, HttpDriverError> {
        let https = check_target(target)?.scheme() == "https";
        Self::with_client(info, target, token, default_client(https)?)
    }

    /// Like [`HttpDriver::new`], with a caller-built client. The caller decides
    /// its redirect, HTTPS and timeout settings.
    pub fn with_client(
        info: DriverInfo,
        target: &str,
        token: Option<String>,
        client: Client,
    ) -> Result<Self, HttpDriverError> {
        if info.protocol_version != PROTOCOL_VERSION {
            return Err(HttpDriverError::UnsupportedProtocol {
                id: info.id.clone(),
                version: info.protocol_version.clone(),
            });
        }
        let target = check_target(target)?;
        Ok(Self {
            info,
            target,
            token,
            client,
        })
    }

    fn url(&self, req: &DriverRequest, suffix: &str) -> Result<Url, DriverError> {
        let base = self.target.as_str().trim_end_matches('/');
        Url::parse(&format!("{base}/{}{suffix}", req.guresid.as_str())).map_err(|_| {
            DriverError::Invalid {
                code: None,
                message: "the driver target is not a valid base URL".into(),
            }
        })
    }

    /// Sends one call. `mutation` says whether a lost response leaves the
    /// outcome uncertain.
    async fn call(
        &self,
        method: Method,
        req: &DriverRequest,
        suffix: &str,
        body: Option<Value>,
        mutation: bool,
    ) -> Result<Reply, DriverError> {
        let mut rb = self
            .client
            .request(method, self.url(req, suffix)?)
            .header(PROTOCOL_HEADER, PROTOCOL_VERSION)
            .header(OPERATION_HEADER, &req.operation.id)
            .header(FINGERPRINT_HEADER, &req.operation.fingerprint);
        if let Some(token) = &self.token {
            rb = rb.header(AUTHORIZATION, format!("Bearer {token}"));
        }
        if let Some(cookie) = &req.cookie {
            rb = rb.header(
                COOKIE_HEADER,
                format!(
                    "{}.{}",
                    cookie.version,
                    URL_SAFE_NO_PAD.encode(&cookie.data)
                ),
            );
        }
        if let Some(body) = body {
            rb = rb.json(&body);
        }
        let lost = |what: &str| {
            if mutation {
                DriverError::Uncertain(format!("{what}; the driver may have acted"))
            } else {
                DriverError::Transient {
                    message: what.to_owned(),
                    retry_after: None,
                }
            }
        };
        let mut res = match rb.send().await {
            Ok(res) => res,
            // Nothing reached the driver.
            Err(e) if e.is_connect() => {
                return Err(DriverError::Transient {
                    message: "cannot connect to the driver".into(),
                    retry_after: None,
                });
            }
            Err(_) => return Err(lost("the request to the driver failed")),
        };
        let status = res.status();
        let headers = res.headers().clone();
        let cookie = headers
            .get(SET_COOKIE_HEADER)
            .map(|v| parse_cookie(v.as_bytes()));
        // A cookie that arrived is kept even with a response rejected below.
        let arrived = cookie.clone().flatten();
        if let Some(version) = headers.get(PROTOCOL_HEADER)
            && version.as_bytes() != PROTOCOL_VERSION.as_bytes()
        {
            let message = format!(
                "the driver answered with protocol `{}`; expected `{PROTOCOL_VERSION}`",
                String::from_utf8_lossy(version.as_bytes())
            );
            return Err(if mutation {
                DriverError::Uncertain(message)
            } else {
                DriverError::Invalid {
                    code: None,
                    message,
                }
            }
            .with_cookie(arrived));
        }
        let cookie = match cookie {
            None => None,
            Some(parsed) => Some(parsed.ok_or_else(|| {
                DriverError::Uncertain("the driver returned a malformed cookie".into())
            })?),
        };
        // The headers arrived, so a cookie in them is kept even when the body
        // does not.
        let mut body = Vec::new();
        loop {
            match res.chunk().await {
                Ok(Some(chunk)) => {
                    if body.len() + chunk.len() > MAX_BODY_BYTES {
                        return Err(
                            lost("the driver's response is larger than 4 MiB").with_cookie(cookie)
                        );
                    }
                    body.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(_) => {
                    return Err(lost("reading the driver's response failed").with_cookie(cookie));
                }
            }
        }
        Ok(Reply {
            status,
            spoken: headers.contains_key(PROTOCOL_HEADER),
            retry_after: retry_after(&headers),
            cookie,
            body,
            mutation,
        })
    }

    /// The outputs of a `200`, whose `id` must be the GUResID.
    fn outputs(&self, req: &DriverRequest, reply: &Reply) -> Result<DriverOutputs, DriverError> {
        let bad = |message: String| {
            if reply.mutation {
                DriverError::Uncertain(message)
            } else {
                DriverError::Invalid {
                    code: None,
                    message,
                }
            }
        };
        let body: ResourceBody = serde_json::from_slice(&reply.body)
            // serde messages can quote values, so only the position is given.
            .map_err(|e| {
                bad(format!(
                    "the driver's response is not a resource (line {}, column {})",
                    e.line(),
                    e.column()
                ))
            })?;
        if body.id != req.guresid.as_str() {
            return Err(bad(
                "the driver's response names a different resource ID than the GUResID".into(),
            ));
        }
        Ok(DriverOutputs {
            values: body.resource.values,
            secrets: SecretMap::new(body.resource.secrets),
            manifests: body.manifests,
            footprint: Default::default(),
        })
    }

    /// A failure status as a driver error. A server error after a mutation is
    /// uncertain unless the driver deduplicates retried operations.
    /// Secret values the driver was sent never come back in its messages.
    fn failure(&self, req: &DriverRequest, reply: &Reply) -> DriverError {
        let status = reply.status.as_u16();
        let body: ErrorBody = serde_json::from_slice(&reply.body).unwrap_or_default();
        // Redacted before truncating, so a cut cannot leave part of a secret.
        let secrets = secret_redactor(req);
        let message = body
            .message
            .map(|m| {
                secrets
                    .redact(&m)
                    .chars()
                    .take(MAX_MESSAGE_CHARS)
                    .collect::<String>()
            })
            .unwrap_or_else(|| format!("the driver answered {status}"));
        match status {
            400 | 422 => DriverError::Invalid {
                code: body
                    .error
                    .map(|c| secrets.redact(&c).chars().take(64).collect::<String>()),
                message,
            },
            401 | 403 => DriverError::Invalid {
                code: None,
                message: format!("the driver refused the credentials ({status})"),
            },
            // A conflicting operation may be this one, retried after its
            // response was lost: the resource is observed before anything is
            // changed again, never mutated blindly.
            409 if reply.mutation => DriverError::Uncertain(format!(
                "the driver reports a conflicting operation (409), so the resource is \
                 observed before it is changed again: {message}"
            )),
            409 => DriverError::Conflict(message),
            429 => DriverError::Transient {
                message,
                retry_after: reply.retry_after,
            },
            500..=599 if !reply.mutation || self.info.capabilities.recover => {
                DriverError::Transient {
                    message,
                    retry_after: reply.retry_after,
                }
            }
            _ if reply.mutation => DriverError::Uncertain(format!(
                "the driver answered {status} to a mutation: {message}"
            )),
            _ => DriverError::Invalid {
                code: None,
                message: format!("the driver answered {status}: {message}"),
            },
        }
    }

    fn observe(&self, req: &DriverRequest, reply: Reply) -> Result<Observation, DriverError> {
        match reply.status {
            // As for a mutation, a failure's cookie is kept: a poll may return
            // the driver's newest state with any response.
            StatusCode::OK => Ok(Observation::Ready {
                outputs: self
                    .outputs(req, &reply)
                    .map_err(|e| e.with_cookie(reply.cookie.clone()))?,
                cookie: reply.cookie,
            }),
            StatusCode::ACCEPTED => Ok(Observation::InProgress {
                poll_after: reply.poll_after(),
                cookie: reply.cookie,
            }),
            // Only a driver's own 404 means absent: a proxy or a wrong path
            // must not make openporch forget a live resource.
            StatusCode::NOT_FOUND if reply.spoken => Ok(Observation::Absent {
                cookie: reply.cookie,
            }),
            StatusCode::NOT_FOUND => Ok(Observation::Unknown {
                reason: NOT_A_DRIVER_404.into(),
                cookie: reply.cookie,
            }),
            StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => Ok(Observation::Failed {
                error: self.failure(req, &reply).with_cookie(reply.cookie),
            }),
            _ => Err(self.failure(req, &reply).with_cookie(reply.cookie)),
        }
    }
}

const NOT_A_DRIVER_404: &str = "the target answered 404 without `Openporch-Protocol`, so it \
     may not be the driver; check the driver's target";

/// A redactor for the string secret values of a request, at any depth.
fn secret_redactor(req: &DriverRequest) -> Redactor {
    fn walk(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::String(s) => out.push(s.clone()),
            Value::Array(a) => a.iter().for_each(|v| walk(v, out)),
            Value::Object(m) => m.values().for_each(|v| walk(v, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    for map in [&req.secrets, &req.resource_secrets]
        .into_iter()
        .chain(req.credentials.as_ref())
    {
        map.expose().values().for_each(|v| walk(v, &mut out));
    }
    Redactor::new(out)
}

/// The request body: `{type, resource, driver: {values, secrets}}`. Resource
/// inputs that resolved from secrets are merged into `resource`, replacing a
/// plain input of the same name.
fn body(req: &DriverRequest) -> Value {
    let mut resource = req.resource.clone();
    resource.extend(req.resource_secrets.expose().clone());
    json!({
        "type": req.resource_type,
        "resource": resource,
        "driver": {"values": req.values, "secrets": req.secrets.expose()},
    })
}

struct Reply {
    status: StatusCode,
    /// Whether the response carried `Openporch-Protocol`, so it came from a
    /// driver rather than, say, a proxy or a wrong path.
    spoken: bool,
    retry_after: Option<Duration>,
    cookie: Option<Cookie>,
    body: Vec<u8>,
    mutation: bool,
}

impl Reply {
    fn poll_after(&self) -> Duration {
        self.retry_after.unwrap_or(DEFAULT_POLL)
    }
}

#[derive(Deserialize)]
struct ResourceBody {
    id: String,
    #[serde(default)]
    resource: ResourceOutputs,
    #[serde(default)]
    manifests: Vec<Manifest>,
}

#[derive(Default, Deserialize)]
struct ResourceOutputs {
    #[serde(default)]
    values: Map<String, Value>,
    #[serde(default)]
    secrets: Map<String, Value>,
}

#[derive(Default, Deserialize)]
struct ErrorBody {
    error: Option<String>,
    message: Option<String>,
}

fn parse_cookie(v: &[u8]) -> Option<Cookie> {
    let v = std::str::from_utf8(v).ok()?;
    let (version, data) = v.trim().split_once('.')?;
    let data = String::from_utf8(URL_SAFE_NO_PAD.decode(data).ok()?).ok()?;
    Cookie::new(version.parse().ok()?, data).ok()
}

/// `Retry-After` in seconds; dates are ignored.
fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let secs: u64 = headers
        .get(RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(Duration::from_secs(secs).min(MAX_RETRY_AFTER))
}

impl Driver for HttpDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    fn put<'a>(&'a self, req: &'a DriverRequest) -> DriverFuture<'a, PutOutcome> {
        Box::pin(async move {
            let reply = self
                .call(Method::PUT, req, "", Some(body(req)), true)
                .await?;
            // A driver may return a cookie with any response, a failure
            // included; it is kept for the observation that recovers an
            // uncertain outcome.
            match reply.status {
                StatusCode::OK => Ok(PutOutcome::Ready {
                    outputs: self
                        .outputs(req, &reply)
                        .map_err(|e| e.with_cookie(reply.cookie.clone()))?,
                    cookie: reply.cookie,
                }),
                StatusCode::ACCEPTED => Ok(PutOutcome::InProgress {
                    poll_after: reply.poll_after(),
                    cookie: reply.cookie,
                }),
                StatusCode::NO_CONTENT => Ok(PutOutcome::Ready {
                    outputs: DriverOutputs::default(),
                    cookie: reply.cookie,
                }),
                _ => Err(self.failure(req, &reply).with_cookie(reply.cookie)),
            }
        })
    }

    fn get<'a>(&'a self, req: &'a DriverRequest) -> DriverFuture<'a, Observation> {
        Box::pin(async move {
            let reply = self.call(Method::GET, req, "", None, false).await?;
            self.observe(req, reply)
        })
    }

    fn delete<'a>(&'a self, req: &'a DriverRequest) -> DriverFuture<'a, DeleteOutcome> {
        Box::pin(async move {
            let reply = self.call(Method::DELETE, req, "", None, true).await?;
            match reply.status {
                StatusCode::OK | StatusCode::NO_CONTENT => Ok(DeleteOutcome::Deleted),
                // The driver's own 404 means it is already gone.
                StatusCode::NOT_FOUND if reply.spoken => Ok(DeleteOutcome::Deleted),
                // Whatever answered, a cookie it returned is kept.
                StatusCode::NOT_FOUND => {
                    Err(DriverError::Uncertain(NOT_A_DRIVER_404.into()).with_cookie(reply.cookie))
                }
                StatusCode::ACCEPTED => Ok(DeleteOutcome::InProgress {
                    poll_after: reply.poll_after(),
                    cookie: reply.cookie,
                }),
                // As for `put`, a failure's cookie is kept.
                _ => Err(self.failure(req, &reply).with_cookie(reply.cookie)),
            }
        })
    }

    fn cancel<'a>(&'a self, req: &'a DriverRequest) -> DriverFuture<'a, Observation> {
        Box::pin(async move {
            if !self.info.capabilities.cancel {
                return Err(DriverError::Unsupported("cancellation".into()));
            }
            let reply = self.call(Method::POST, req, "/cancel", None, false).await?;
            self.observe(req, reply)
        })
    }
}
