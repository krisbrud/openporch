//! The `Driver` trait and the built-in openporch drivers.
//!
//! A driver turns a resource definition's inputs into a provisioned resource and its
//! outputs. Calls are keyed by the resource's GUResID and carry a durable operation
//! identity, so a retried call with the same identity converges on the same effect.
//! The executor resolves secrets before a call; drivers see secret values only in
//! [`SecretMap`]s, whose `Debug` output is redacted.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use openporch_core::ids::Guresid;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

mod echo;
mod none;
mod registry;
mod run;
mod template;

pub use echo::Echo;
pub use none::NoneDriver;
pub use registry::{DriverRegistry, RegistryError};
pub use run::{
    DEFAULT_TIMEOUT, MAX_TIMEOUT, Settled, call_timeout, converge, remove, with_timeout,
};
pub use template::Template;

/// The largest cookie a driver may return, in bytes.
pub const MAX_COOKIE_BYTES: usize = 10 * 1024;

/// A boxed driver call, so drivers can live in a registry as trait objects.
pub type DriverFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, DriverError>> + Send + 'a>>;

/// Secret values keyed like the JSON object they came from. `Debug` never shows
/// them; only the executor and drivers call [`SecretMap::expose`].
#[derive(Clone, Default, PartialEq)]
pub struct SecretMap(Map<String, Value>);

impl SecretMap {
    /// Wraps resolved secret values.
    pub fn new(values: Map<String, Value>) -> Self {
        Self(values)
    }

    /// The values.
    pub fn expose(&self) -> &Map<String, Value> {
        &self.0
    }

    /// Whether there are no secrets.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The keys, which are not secret.
    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.0.keys()
    }
}

impl fmt::Debug for SecretMap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map()
            .entries(self.0.keys().map(|k| (k, "[REDACTED]")))
            .finish()
    }
}

/// An opaque, versioned driver state blob, returned by one call and passed to the
/// next call for the same GUResID. It may hold secrets, such as a generated
/// password, so `Debug` shows only its version and size.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cookie {
    /// The cookie format version the driver wrote.
    pub version: u32,
    /// The driver's data.
    pub data: String,
}

impl Cookie {
    /// A cookie, refused when larger than [`MAX_COOKIE_BYTES`].
    pub fn new(version: u32, data: impl Into<String>) -> Result<Self, DriverError> {
        let cookie = Self {
            version,
            data: data.into(),
        };
        cookie.check()?;
        Ok(cookie)
    }

    /// Refuses a cookie larger than [`MAX_COOKIE_BYTES`].
    pub fn check(&self) -> Result<(), DriverError> {
        if self.data.len() > MAX_COOKIE_BYTES {
            return Err(DriverError::Invalid {
                code: None,
                message: format!(
                    "the driver returned a {} byte cookie; the limit is {MAX_COOKIE_BYTES}",
                    self.data.len()
                ),
            });
        }
        Ok(())
    }
}

impl fmt::Debug for Cookie {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cookie")
            .field("version", &self.version)
            .field("bytes", &self.data.len())
            .finish()
    }
}

/// Where a call comes from. Drivers may use it to name things; it is not secret.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DriverContext {
    pub org: String,
    pub app: String,
    pub env: String,
    pub env_type: String,
    /// The resource ID in the graph, such as `modules.api.externals.db`.
    pub res_id: String,
    pub class: String,
}

/// The durable identity of one mutation. A retry with the same identity must
/// converge on the same effect; a changed fingerprint under the same ID is refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Operation {
    pub id: String,
    /// A hash of the call's inputs.
    pub fingerprint: String,
}

/// A driver call. Values are resolved; secrets are resolved values the executor
/// read immediately before the call.
#[derive(Debug, Clone, PartialEq)]
pub struct DriverRequest {
    pub guresid: Guresid,
    pub operation: Operation,
    /// The resource type, such as `postgres`.
    pub resource_type: String,
    /// The resource's resolved inputs.
    pub resource: Map<String, Value>,
    /// Resource inputs that resolved from secrets.
    pub resource_secrets: SecretMap,
    /// The definition's resolved `driver_inputs.values`.
    pub values: Map<String, Value>,
    /// The definition's resolved `driver_inputs.secrets` and `secret_refs`.
    pub secrets: SecretMap,
    /// Short-lived cloud credentials for the definition's account, if any.
    pub credentials: Option<SecretMap>,
    pub context: DriverContext,
    /// The cookie from the previous call for this GUResID.
    pub cookie: Option<Cookie>,
}

/// A Kubernetes manifest with where it goes. It may be a `Secret`, so `Debug`
/// shows only its location and kind.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// `namespace` or `cluster`.
    pub location: String,
    pub data: Value,
}

impl fmt::Debug for Manifest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Manifest")
            .field("location", &self.location)
            .field("kind", &self.data.get("kind").and_then(Value::as_str))
            .finish_non_exhaustive()
    }
}

/// What a provisioned resource produced.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DriverOutputs {
    pub values: Map<String, Value>,
    pub secrets: SecretMap,
    pub manifests: Vec<Manifest>,
}

/// The result of `put`.
#[derive(Debug, Clone, PartialEq)]
pub enum PutOutcome {
    /// Provisioned.
    Ready {
        outputs: DriverOutputs,
        cookie: Option<Cookie>,
    },
    /// Still converging; poll `get` after `poll_after`.
    InProgress {
        poll_after: Duration,
        cookie: Option<Cookie>,
    },
}

/// What `get` observed.
#[derive(Debug, Clone, PartialEq)]
pub enum Observation {
    /// Nothing exists for the GUResID.
    Absent,
    /// An operation is still running; poll again after `poll_after`.
    InProgress {
        poll_after: Duration,
        cookie: Option<Cookie>,
    },
    /// The resource exists with these outputs.
    Ready {
        outputs: DriverOutputs,
        cookie: Option<Cookie>,
    },
    /// The last operation failed.
    Failed { error: DriverError },
    /// The driver cannot tell; an operator must establish the outcome.
    Unknown { reason: String },
}

/// The result of `delete`.
#[derive(Debug, Clone, PartialEq)]
pub enum DeleteOutcome {
    /// Gone, or never existed.
    Deleted,
    /// Still deleting; poll `get` until it reports `Absent`.
    InProgress {
        poll_after: Duration,
        cookie: Option<Cookie>,
    },
}

/// A planned change to infrastructure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Create,
    Update,
    Replace,
    Delete,
}

/// The result of `plan`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// The driver cannot preview; the impact is unknown.
    Unknown,
    /// The call changes no infrastructure.
    NoEffects,
    /// Classified actions on named objects, with a fingerprint of the inputs and
    /// state they were planned against.
    Actions {
        actions: Vec<(Action, String)>,
        fingerprint: String,
    },
}

/// Driver failures. Messages never contain secret values.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DriverError {
    /// The inputs are wrong; retrying cannot help.
    #[error("{}", match code { Some(c) => format!("{c}: {message}"), None => message.clone() })]
    Invalid {
        code: Option<String>,
        message: String,
    },
    /// The driver reports a conflicting operation; observe before mutating again.
    #[error("conflict: {0}")]
    Conflict(String),
    /// A temporary failure that may be retried within the deadline.
    #[error("{message}")]
    Transient {
        message: String,
        retry_after: Option<Duration>,
    },
    /// The call did not finish within its timeout; the outcome is uncertain.
    #[error("the driver did not finish within {0:?}")]
    Timeout(Duration),
    /// The outcome of a mutation is unknown and must be recovered first.
    #[error("the outcome is unknown: {0}")]
    Uncertain(String),
    /// The driver does not support the request.
    #[error("unsupported: {0}")]
    Unsupported(String),
}

impl DriverError {
    /// Whether the engine may retry the same operation automatically.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Transient { .. })
    }
}

/// What a driver can provision.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Targets {
    pub resources: bool,
    pub workloads: bool,
    pub triggers: bool,
}

/// What a driver supports beyond `put`, `get` and `delete`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Capabilities {
    pub targets: Targets,
    /// `get` reports the real state of the resource.
    pub observe: bool,
    /// A retried operation identity converges instead of duplicating effects.
    pub recover: bool,
    /// `plan` returns classified actions.
    pub preview: bool,
    /// Calls can be cancelled.
    pub cancel: bool,
}

/// What a registry records about a driver.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DriverInfo {
    /// `<namespace>/<name>`; `openporch` is reserved for built-ins.
    pub id: String,
    pub protocol_version: String,
    pub cookie_version: u32,
    pub input_schema: Value,
    pub output_schema: Value,
    pub capabilities: Capabilities,
}

/// A driver. Every call for one GUResID is serialized by the engine.
pub trait Driver: Send + Sync + fmt::Debug {
    /// The registry record.
    fn info(&self) -> &DriverInfo;

    /// Previews a `put` without changing anything.
    fn plan<'a>(&'a self, _req: &'a DriverRequest) -> DriverFuture<'a, Plan> {
        Box::pin(async { Ok(Plan::Unknown) })
    }

    /// Converges the resource on the request.
    fn put<'a>(&'a self, req: &'a DriverRequest) -> DriverFuture<'a, PutOutcome>;

    /// Observes the resource.
    fn get<'a>(&'a self, req: &'a DriverRequest) -> DriverFuture<'a, Observation>;

    /// Removes the resource.
    fn delete<'a>(&'a self, req: &'a DriverRequest) -> DriverFuture<'a, DeleteOutcome>;

    /// Stops the operation `req` identifies after a timeout, and observes the
    /// result. Drivers that declare the `cancel` capability implement it; an
    /// in-process call is cancelled by dropping its future.
    fn cancel<'a>(&'a self, _req: &'a DriverRequest) -> DriverFuture<'a, Observation> {
        Box::pin(async { Err(DriverError::Unsupported("cancellation".into())) })
    }
}

/// The input and output schemas of drivers that take any object.
fn any_object() -> Value {
    serde_json::json!({"type": "object"})
}

/// The capabilities of an in-process driver that creates nothing.
fn stateless() -> Capabilities {
    Capabilities {
        targets: Targets {
            resources: true,
            workloads: true,
            triggers: false,
        },
        observe: true,
        recover: true,
        preview: true,
        cancel: false,
    }
}

#[cfg(test)]
mod tests;
