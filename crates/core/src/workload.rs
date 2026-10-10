//! The openporch workload document (`openporch.dev/v1`, `kind: Workload`), Score
//! import, validation, and conversion into a deployment set module.
//!
//! A workload is either `containers` (Score's container shape) or a `function`, with
//! `triggers` that point at its own `resources`. Conversion translates
//! `${resources.<key>.<output>}` into the set placeholders `${externals.<key>.<output>}`
//! or `${shared.<id>.<output>}`, checking each output against the type's schema.

use std::collections::BTreeMap;

use indexmap::IndexMap;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::catalog::{OutputKind, ResourceType};
use crate::ids::{ClassId, ResourceTypeId, WorkloadId, is_valid_object_id};
use crate::placeholder::{Part, RawPlaceholder, Segment, Template};
use crate::sets::{External, Module};

/// The `apiVersion` of openporch documents.
pub const API_VERSION: &str = "openporch.dev/v1";
/// The `apiVersion` of Score documents accepted for import.
pub const SCORE_API_VERSION: &str = "score.dev/v1b1";

/// A workload document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
#[schemars(
    title = "openporch Workload",
    extend("$id" = "https://openporch.dev/schemas/workload-v1.json"),
    // A workload is either containers or a function.
    extend("oneOf" = [
        {
            "required": ["containers"],
            "properties": {"containers": {"type": "object"}},
            "anyOf": [{"not": {"required": ["function"]}}, {"properties": {"function": {"type": "null"}}}]
        },
        {
            "required": ["function"],
            "properties": {"function": {"type": "object"}},
            "anyOf": [{"not": {"required": ["containers"]}}, {"properties": {"containers": {"type": "null"}}}]
        }
    ])
)]
pub struct Workload {
    /// Always `openporch.dev/v1`.
    #[schemars(extend("const" = "openporch.dev/v1"))]
    pub api_version: String,
    /// Always `Workload`.
    #[schemars(extend("const" = "Workload"))]
    pub kind: String,
    /// Name, class and annotations.
    pub metadata: Metadata,
    /// Containers by ID. Exactly one of `containers` and `function` is required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub containers: Option<IndexMap<String, Container>>,
    /// A function. Exactly one of `containers` and `function` is required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function: Option<Function>,
    /// Network ports exposed by containers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<Service>,
    /// Triggers by name.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub triggers: IndexMap<String, Trigger>,
    /// Resource dependencies by key.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub resources: IndexMap<String, ResourceRequest>,
    /// Deployment settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deploy: Option<Deploy>,
    /// Driver-specific settings, validated by the selected driver.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<serde_json::Map<String, Value>>,
}

/// Workload metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Metadata {
    /// Workload name.
    pub name: WorkloadId,
    /// Workload class, which selects the target. Defaults to `default`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<ClassId>,
    /// Annotations.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
}

/// A container, in Score's shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Container {
    /// Image reference, or `.` to supply it at deploy time.
    pub image: String,
    /// Overrides the image entrypoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    /// Overrides the image arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    /// Environment variables.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub variables: IndexMap<String, String>,
    /// Files by target path.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub files: IndexMap<String, File>,
    /// Compute limits and requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ContainerResources>,
    /// Liveness probe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub liveness_probe: Option<Probe>,
    /// Readiness probe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readiness_probe: Option<Probe>,
}

/// A file mounted into a container.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct File {
    /// File mode, such as `0644`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Path of a local file to read, relative to the document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Inline content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Disables placeholder expansion in the content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_expand: Option<bool>,
}

/// Container compute resources.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ContainerResources {
    /// Upper bounds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<Quantities>,
    /// Guaranteed amounts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests: Option<Quantities>,
}

/// CPU and memory quantities.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Quantities {
    /// Memory, such as `512Mi`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<String>,
    /// CPU, such as `250m`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<String>,
}

/// A container probe.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Probe {
    /// An HTTP GET check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_get: Option<HttpProbe>,
    /// A command check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec: Option<ExecProbe>,
}

/// An HTTP GET probe.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct HttpProbe {
    /// `HTTP` or `HTTPS`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheme: Option<String>,
    /// Host header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Request path.
    pub path: String,
    /// Container port.
    pub port: u16,
    /// Extra headers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub http_headers: Vec<HttpHeader>,
}

/// An HTTP header.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HttpHeader {
    /// Header name.
    pub name: String,
    /// Header value.
    pub value: String,
}

/// A command probe.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExecProbe {
    /// Command and arguments.
    pub command: Vec<String>,
}

/// Ports exposed by a container workload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Service {
    /// Ports by name.
    #[serde(default)]
    pub ports: IndexMap<String, ServicePort>,
}

/// A service port.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ServicePort {
    /// Exposed port.
    pub port: u16,
    /// Container port; defaults to `port`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_port: Option<u16>,
    /// `TCP` or `UDP`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
}

/// A function workload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Function {
    /// How the code is packaged.
    pub package: Package,
    /// Environment variables.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub variables: IndexMap<String, String>,
    /// Memory, such as `256Mi`.
    #[serde(default = "default_memory")]
    pub memory: String,
    /// Timeout, such as `30s`.
    #[serde(default = "default_timeout")]
    pub timeout: String,
    /// Concurrency settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<Concurrency>,
}

fn default_memory() -> String {
    "256Mi".to_owned()
}

fn default_timeout() -> String {
    "30s".to_owned()
}

/// Function packaging.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum Package {
    /// A zip archive with a runtime and handler.
    Zip {
        /// Artifact URI, or `.` to supply it at deploy time.
        artifact: String,
        /// Runtime, such as `python3.12`.
        runtime: String,
        /// Handler, such as `app.handler`.
        handler: String,
    },
    /// A container image.
    Image {
        /// Image reference, or `.` to supply it at deploy time.
        image: String,
    },
}

/// Function concurrency.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Concurrency {
    /// Reserved concurrency.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserved: Option<u32>,
    /// Provisioned concurrency.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provisioned: Option<u32>,
}

/// A trigger. Each trigger has exactly one kind.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
pub enum Trigger {
    /// HTTP requests.
    Http(HttpTrigger),
    /// A cron schedule.
    Schedule(ScheduleTrigger),
    /// Messages from a queue resource.
    Queue(QueueTrigger),
    /// Messages from a topic resource.
    Topic(TopicTrigger),
    /// Records from a stream resource.
    Stream(StreamTrigger),
}

impl Trigger {
    /// The kind name, such as `queue`.
    pub fn kind(&self) -> &'static str {
        match self {
            Trigger::Http(_) => "http",
            Trigger::Schedule(_) => "schedule",
            Trigger::Queue(_) => "queue",
            Trigger::Topic(_) => "topic",
            Trigger::Stream(_) => "stream",
        }
    }

    /// The resource key the trigger reads from, if any.
    pub fn resource(&self) -> Option<&str> {
        match self {
            Trigger::Queue(t) => Some(&t.resource),
            Trigger::Topic(t) => Some(&t.resource),
            Trigger::Stream(t) => Some(&t.resource),
            Trigger::Http(_) | Trigger::Schedule(_) => None,
        }
    }
}

/// HTTP trigger.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HttpTrigger {
    /// For containers, the name of a service port.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<String>,
    /// Path prefix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Authentication; the default follows the target's policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<HttpAuth>,
}

/// HTTP trigger authentication.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum HttpAuth {
    /// Public.
    None,
    /// Cloud IAM authentication.
    Iam,
}

/// Schedule trigger.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScheduleTrigger {
    /// A 5-field cron expression.
    pub cron: String,
    /// IANA time zone; UTC when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    /// Input passed to each invocation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Value>,
}

/// Queue trigger.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct QueueTrigger {
    /// Key of the queue under `resources`.
    pub resource: String,
    /// Messages per invocation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_size: Option<u32>,
}

/// Topic trigger.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TopicTrigger {
    /// Key of the topic under `resources`.
    pub resource: String,
}

/// Stream trigger.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct StreamTrigger {
    /// Key of the stream under `resources`.
    pub resource: String,
    /// Records per invocation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_size: Option<u32>,
    /// `latest` or `trim_horizon`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub starting_position: Option<String>,
}

/// A resource dependency.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResourceRequest {
    /// Resource type.
    #[serde(rename = "type")]
    pub ty: ResourceTypeId,
    /// Resource class. Defaults to `default`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<ClassId>,
    /// Shared resource ID. Without it the dependency is private to the workload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend(
        "minLength" = 2,
        "maxLength" = crate::ids::MAX_ID_LEN,
        "pattern" = crate::ids::OBJECT_ID_PATTERN
    ))]
    pub id: Option<String>,
    /// Parameters for the resource.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    /// Annotations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<ResourceMetadata>,
}

/// Resource dependency metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResourceMetadata {
    /// Annotations.
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
}

/// Deployment settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Deploy {
    /// What a successful deployment waits for. Defaults to `available`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<DeployStatus>,
    /// How long to wait. Defaults to `5m`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
}

/// What a deployment waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum DeployStatus {
    /// The target accepted the workload.
    Deploy,
    /// The workload is available.
    Available,
}

/// A Score file (`score.dev/v1b1`), for import.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ScoreWorkload {
    /// Always `score.dev/v1b1`.
    pub api_version: String,
    /// Name and annotations.
    pub metadata: ScoreMetadata,
    /// Containers by ID.
    pub containers: IndexMap<String, Container>,
    /// Ports.
    #[serde(default)]
    pub service: Option<Service>,
    /// Resource dependencies.
    #[serde(default)]
    pub resources: IndexMap<String, ResourceRequest>,
}

/// Score metadata.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScoreMetadata {
    /// Workload name.
    pub name: WorkloadId,
    /// Annotations.
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
}

/// An overlay adding openporch settings to an imported Score file.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WorkloadOverlay {
    /// Always `openporch.dev/v1`.
    pub api_version: String,
    /// Always `WorkloadOverlay`.
    pub kind: String,
    /// The workload class.
    #[serde(default)]
    pub metadata: Option<OverlayMetadata>,
    /// Triggers by name.
    #[serde(default)]
    pub triggers: IndexMap<String, Trigger>,
    /// Deployment settings.
    #[serde(default)]
    pub deploy: Option<Deploy>,
    /// Driver-specific settings.
    #[serde(default)]
    pub target: Option<serde_json::Map<String, Value>>,
}

/// Overlay metadata.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OverlayMetadata {
    /// The workload class.
    #[serde(default)]
    pub class: Option<ClassId>,
}

/// A workload document error, naming the field or path involved.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{path}: {message}")]
pub struct WorkloadError {
    /// Where the problem is, such as `triggers.inbox.queue.resource`, or `.` for the root.
    pub path: String,
    /// What is wrong.
    pub message: String,
}

impl WorkloadError {
    fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }
}

fn parse_yaml<T: serde::de::DeserializeOwned>(
    value: serde_yaml_ng::Value,
) -> Result<T, WorkloadError> {
    // Going through JSON lets externally tagged enums (triggers) read from plain maps.
    let json = serde_json::to_value(value).map_err(|e| WorkloadError::new(".", e.to_string()))?;
    serde_path_to_error::deserialize(json).map_err(|e| {
        let path = e.path().to_string();
        WorkloadError::new(path, e.into_inner().to_string())
    })
}

fn yaml_str<'a>(v: &'a serde_yaml_ng::Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(serde_yaml_ng::Value::as_str)
}

impl Workload {
    /// Parses and validates a YAML (or JSON) document: an openporch workload, or a
    /// Score file that is imported with an optional overlay.
    pub fn parse(document: &str, overlay: Option<&str>) -> Result<Self, WorkloadError> {
        let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(document)
            .map_err(|e| WorkloadError::new(".", e.to_string()))?;
        let workload = match yaml_str(&value, "apiVersion") {
            Some(API_VERSION) => {
                if overlay.is_some() {
                    return Err(WorkloadError::new(
                        ".",
                        "overlays apply only to Score files",
                    ));
                }
                let w: Workload = parse_yaml(value)?;
                if w.kind != "Workload" {
                    return Err(WorkloadError::new("kind", "expected `Workload`"));
                }
                w
            }
            Some(SCORE_API_VERSION) => {
                let score: ScoreWorkload = parse_yaml(value)?;
                let overlay = overlay.map(WorkloadOverlay::parse).transpose()?;
                Workload::from_score(score, overlay)
            }
            Some(other) => {
                return Err(WorkloadError::new(
                    "apiVersion",
                    format!(
                        "unsupported `{other}`; expected `{API_VERSION}` or `{SCORE_API_VERSION}`"
                    ),
                ));
            }
            None => return Err(WorkloadError::new("apiVersion", "missing")),
        };
        workload.validate()?;
        Ok(workload)
    }

    /// Converts a Score file and optional overlay into a workload document.
    pub fn from_score(score: ScoreWorkload, overlay: Option<WorkloadOverlay>) -> Self {
        let overlay = overlay.unwrap_or_default();
        Workload {
            api_version: API_VERSION.to_owned(),
            kind: "Workload".to_owned(),
            metadata: Metadata {
                name: score.metadata.name,
                class: overlay.metadata.and_then(|m| m.class),
                annotations: score.metadata.annotations,
            },
            containers: Some(score.containers),
            function: None,
            service: score.service,
            triggers: overlay.triggers,
            resources: score.resources,
            deploy: overlay.deploy,
            target: overlay.target,
        }
    }

    /// The workload class, `default` when unset.
    pub fn class(&self) -> ClassId {
        self.metadata.class.clone().unwrap_or_default()
    }

    /// The triggers, plus the `http` trigger implied by the first service port when a
    /// container workload declares ports and no `http` trigger.
    pub fn effective_triggers(&self) -> IndexMap<String, Trigger> {
        let mut triggers = self.triggers.clone();
        let has_http = triggers.values().any(|t| matches!(t, Trigger::Http(_)));
        if !has_http
            && self.containers.is_some()
            && let Some((name, _)) = self.service.as_ref().and_then(|s| s.ports.first())
            && !triggers.contains_key(name)
        {
            let http = HttpTrigger {
                port: Some(name.clone()),
                ..Default::default()
            };
            triggers.insert(name.clone(), Trigger::Http(http));
        }
        triggers
    }

    /// Checks the rules serde cannot express.
    pub fn validate(&self) -> Result<(), WorkloadError> {
        match (&self.containers, &self.function) {
            (Some(_), Some(_)) => {
                return Err(WorkloadError::new(
                    ".",
                    "a workload is either containers or a function, not both",
                ));
            }
            (None, None) => {
                return Err(WorkloadError::new(
                    ".",
                    "a workload needs `containers` or `function`",
                ));
            }
            (Some(c), None) if c.is_empty() => {
                return Err(WorkloadError::new(
                    "containers",
                    "at least one container is required",
                ));
            }
            _ => {}
        }
        for (id, c) in self.containers.iter().flatten() {
            if c.image.is_empty() {
                return Err(WorkloadError::new(
                    format!("containers.{id}.image"),
                    "must not be empty",
                ));
            }
            for (target, f) in &c.files {
                if f.source.is_some() == f.content.is_some() {
                    return Err(WorkloadError::new(
                        format!("containers.{id}.files.{target}"),
                        "set exactly one of `source` and `content`",
                    ));
                }
            }
            for (field, q) in [
                (
                    "limits",
                    c.resources.as_ref().and_then(|r| r.limits.as_ref()),
                ),
                (
                    "requests",
                    c.resources.as_ref().and_then(|r| r.requests.as_ref()),
                ),
            ] {
                for (name, v) in [
                    ("memory", q.and_then(|q| q.memory.as_ref())),
                    ("cpu", q.and_then(|q| q.cpu.as_ref())),
                ] {
                    if let Some(v) = v
                        && !is_quantity(v)
                    {
                        return Err(WorkloadError::new(
                            format!("containers.{id}.resources.{field}.{name}"),
                            format!("`{v}` is not a quantity"),
                        ));
                    }
                }
            }
        }
        if self.service.is_some() && self.function.is_some() {
            return Err(WorkloadError::new(
                "service",
                "`service` applies to container workloads only",
            ));
        }
        if let Some(f) = &self.function {
            match &f.package {
                Package::Zip {
                    artifact,
                    runtime,
                    handler,
                } => {
                    for (name, v) in [
                        ("artifact", artifact),
                        ("runtime", runtime),
                        ("handler", handler),
                    ] {
                        if v.is_empty() {
                            return Err(WorkloadError::new(
                                format!("function.package.{name}"),
                                "must not be empty",
                            ));
                        }
                    }
                }
                Package::Image { image } if image.is_empty() => {
                    return Err(WorkloadError::new(
                        "function.package.image",
                        "must not be empty",
                    ));
                }
                Package::Image { .. } => {}
            }
            if !is_quantity(&f.memory) {
                return Err(WorkloadError::new(
                    "function.memory",
                    format!("`{}` is not a quantity such as `256Mi`", f.memory),
                ));
            }
            if !is_duration(&f.timeout) {
                return Err(WorkloadError::new(
                    "function.timeout",
                    format!("`{}` is not a duration such as `30s`", f.timeout),
                ));
            }
        }
        for (key, r) in &self.resources {
            if let Some(id) = &r.id
                && !is_valid_object_id(id)
            {
                return Err(WorkloadError::new(
                    format!("resources.{key}.id"),
                    crate::ids::OBJECT_ID_RULE,
                ));
            }
            // Params go to the resource's driver as they are: placeholders in them would
            // be neither translated nor ordered in the graph, so they are rejected.
            if let Some(params) = &r.params {
                reject_placeholders(params, &format!("resources.{key}.params"))?;
            }
        }
        for (name, t) in &self.triggers {
            let path = format!("triggers.{name}.{}", t.kind());
            if let Some(res) = t.resource()
                && !self.resources.contains_key(res)
            {
                return Err(WorkloadError::new(
                    format!("{path}.resource"),
                    format!("resource `{res}` is not declared under `resources`"),
                ));
            }
            match t {
                Trigger::Schedule(s) if !is_cron(&s.cron) => {
                    return Err(WorkloadError::new(
                        format!("{path}.cron"),
                        format!("`{}` is not a 5-field cron expression", s.cron),
                    ));
                }
                Trigger::Http(h) => {
                    if let Some(port) = &h.port {
                        let ports = self.service.as_ref().map(|s| &s.ports);
                        if self.function.is_some() {
                            return Err(WorkloadError::new(
                                format!("{path}.port"),
                                "functions have no ports",
                            ));
                        }
                        if !ports.is_some_and(|p| p.contains_key(port)) {
                            return Err(WorkloadError::new(
                                format!("{path}.port"),
                                format!("`{port}` is not a service port"),
                            ));
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(d) = &self.deploy
            && let Some(t) = &d.timeout
            && !is_duration(t)
        {
            return Err(WorkloadError::new(
                "deploy.timeout",
                format!("`{t}` is not a duration such as `5m`"),
            ));
        }
        // Placeholders: only `resources`, `values` and `pod` roots, and never `context`.
        // File content with `noExpand` is kept literally.
        let mut json = serde_json::to_value(self).expect("workloads serialize");
        set_aside(&mut json, &self.no_expand_pointers(""));
        check_placeholders(&json, "")?;
        Ok(())
    }

    /// Converts the workload to a deployment set module, translating placeholders and
    /// checking referenced outputs against `types`.
    pub fn to_module(&self, types: &[ResourceType]) -> Result<ConvertedWorkload, WorkloadError> {
        let mut externals = BTreeMap::new();
        for (key, r) in &self.resources {
            externals.insert(
                key.clone(),
                External {
                    ty: r.ty.clone(),
                    class: r.class.clone().unwrap_or_default(),
                    id: r.id.clone(),
                    params: r.params.clone().filter(|p| !p.is_null()),
                },
            );
        }
        let mut spec = serde_json::Map::new();
        if let Some(c) = &self.containers {
            spec.insert(
                "containers".into(),
                serde_json::to_value(c).expect("serializes"),
            );
        }
        if let Some(f) = &self.function {
            spec.insert(
                "function".into(),
                serde_json::to_value(f).expect("serializes"),
            );
        }
        if let Some(s) = &self.service {
            spec.insert(
                "service".into(),
                serde_json::to_value(s).expect("serializes"),
            );
        }
        let triggers = self.effective_triggers();
        if !triggers.is_empty() {
            spec.insert(
                "triggers".into(),
                serde_json::to_value(&triggers).expect("serializes"),
            );
        }
        let deploy = self.deploy.clone().unwrap_or(Deploy {
            status: None,
            timeout: None,
        });
        spec.insert(
            "deploy".into(),
            serde_json::json!({
                "status": deploy.status.unwrap_or(DeployStatus::Available),
                "timeout": deploy.timeout.unwrap_or_else(|| "5m".to_owned()),
            }),
        );
        if let Some(t) = &self.target {
            spec.insert("target".into(), Value::Object(t.clone()));
        }
        if !self.metadata.annotations.is_empty() {
            spec.insert(
                "annotations".into(),
                serde_json::to_value(&self.metadata.annotations).expect("serializes"),
            );
        }
        let mut secret_paths = Vec::new();
        let mut spec = Value::Object(spec);
        let kept = set_aside(&mut spec, &self.no_expand_pointers(""));
        translate(&mut spec, "", &self.resources, types, &mut secret_paths)?;
        for (pointer, content) in kept {
            *spec
                .pointer_mut(&pointer)
                .expect("set-aside content exists") = content;
        }
        let Value::Object(spec) = spec else {
            unreachable!()
        };
        Ok(ConvertedWorkload {
            id: self.metadata.name.clone(),
            module: Module {
                class: self.class(),
                spec,
                externals,
            },
            secret_paths,
        })
    }

    /// JSON pointers to the content of files with `noExpand: true`.
    fn no_expand_pointers(&self, prefix: &str) -> Vec<String> {
        let mut out = Vec::new();
        for (id, c) in self.containers.iter().flatten() {
            for (target, f) in &c.files {
                if f.no_expand == Some(true) && f.content.is_some() {
                    out.push(format!(
                        "{prefix}/containers/{}/files/{}/content",
                        escape_pointer(id),
                        escape_pointer(target)
                    ));
                }
            }
        }
        out
    }

    /// The published JSON Schema of the workload document.
    pub fn json_schema() -> Value {
        serde_json::to_value(schemars::schema_for!(Workload)).expect("schemas serialize")
    }
}

impl WorkloadOverlay {
    /// Parses an overlay document.
    pub fn parse(document: &str) -> Result<Self, WorkloadError> {
        let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(document)
            .map_err(|e| WorkloadError::new("overlay", e.to_string()))?;
        let o: WorkloadOverlay = parse_yaml(value)
            .map_err(|e| WorkloadError::new(format!("overlay {}", e.path), e.message))?;
        if o.api_version != API_VERSION || o.kind != "WorkloadOverlay" {
            return Err(WorkloadError::new(
                "overlay",
                format!("expected `apiVersion: {API_VERSION}` and `kind: WorkloadOverlay`"),
            ));
        }
        Ok(o)
    }
}

/// A workload converted to a set module.
#[derive(Debug, Clone, PartialEq)]
pub struct ConvertedWorkload {
    /// The module ID.
    pub id: WorkloadId,
    /// The module.
    pub module: Module,
    /// JSON pointers into `module.spec` of strings that contain secret outputs.
    pub secret_paths: Vec<String>,
}

fn join(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_owned()
    } else {
        format!("{path}.{key}")
    }
}

fn reject_placeholders(v: &Value, path: &str) -> Result<(), WorkloadError> {
    match v {
        Value::String(s) => {
            let t = Template::parse(s).map_err(|e| WorkloadError::new(path, e.to_string()))?;
            if t.placeholders().next().is_some() {
                return Err(WorkloadError::new(
                    path,
                    "placeholders are not supported in resource params",
                ));
            }
            Ok(())
        }
        Value::Array(items) => items
            .iter()
            .enumerate()
            .try_for_each(|(i, item)| reject_placeholders(item, &join(path, &i.to_string()))),
        Value::Object(map) => map
            .iter()
            .try_for_each(|(k, item)| reject_placeholders(item, &join(path, k))),
        _ => Ok(()),
    }
}

fn check_placeholders(v: &Value, path: &str) -> Result<(), WorkloadError> {
    match v {
        Value::String(s) => {
            let t = Template::parse(s).map_err(|e| WorkloadError::new(path, e.to_string()))?;
            for p in t.placeholders() {
                match p.root() {
                    "resources" | "values" | "pod" => {}
                    "context" => {
                        return Err(WorkloadError::new(
                            path,
                            format!(
                                "`{}`: workloads cannot use `context` placeholders; use `values` for environment configuration",
                                p.text
                            ),
                        ));
                    }
                    other => {
                        return Err(WorkloadError::new(
                            path,
                            format!("`{}`: unknown placeholder root `{other}`", p.text),
                        ));
                    }
                }
            }
        }
        Value::Array(a) => {
            for (i, item) in a.iter().enumerate() {
                check_placeholders(item, &format!("{path}[{i}]"))?;
            }
        }
        Value::Object(m) => {
            for (k, item) in m {
                check_placeholders(item, &join(path, k))?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn escape_pointer(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

/// Replaces the values at `pointers` with `null` and returns them.
fn set_aside(v: &mut Value, pointers: &[String]) -> Vec<(String, Value)> {
    pointers
        .iter()
        .filter_map(|p| {
            v.pointer_mut(p)
                .map(|slot| (p.clone(), std::mem::take(slot)))
        })
        .collect()
}

/// Whether a spec pointer is a workload secret sink: a container or function
/// variable, or container file content.
fn is_secret_sink(pointer: &str) -> bool {
    let segs: Vec<&str> = pointer.trim_start_matches('/').split('/').collect();
    matches!(
        segs.as_slice(),
        ["containers", _, "variables", _]
            | ["containers", _, "files", _, "content"]
            | ["function", "variables", _]
    )
}

fn translate(
    v: &mut Value,
    pointer: &str,
    resources: &IndexMap<String, ResourceRequest>,
    types: &[ResourceType],
    secret_paths: &mut Vec<String>,
) -> Result<(), WorkloadError> {
    match v {
        Value::String(s) => {
            let path = pointer.trim_start_matches('/').replace('/', ".");
            let t = Template::parse(s).map_err(|e| WorkloadError::new(&path, e.to_string()))?;
            if t.is_literal() {
                return Ok(());
            }
            let mut out = String::new();
            let mut secret: Option<&str> = None;
            for part in &t.parts {
                match part {
                    Part::Literal(l) => out.push_str(&l.replace("${", "$\\{")),
                    Part::Placeholder(p) if p.root() == "resources" => {
                        let (text, is_secret) = translate_resource_placeholder(p, resources, types)
                            .map_err(|m| WorkloadError::new(&path, m))?;
                        if is_secret && secret.is_none() {
                            secret = Some(&p.text);
                        }
                        out.push_str(&text);
                    }
                    Part::Placeholder(p) => out.push_str(&p.text),
                }
            }
            if let Some(placeholder) = secret {
                if !is_secret_sink(pointer) {
                    return Err(WorkloadError::new(
                        &path,
                        format!(
                            "`{placeholder}` is a secret output; secrets can only be used in \
                             variables and file content"
                        ),
                    ));
                }
                secret_paths.push(pointer.to_owned());
            }
            *s = out;
        }
        Value::Array(a) => {
            for (i, item) in a.iter_mut().enumerate() {
                translate(
                    item,
                    &format!("{pointer}/{i}"),
                    resources,
                    types,
                    secret_paths,
                )?;
            }
        }
        Value::Object(m) => {
            for (k, item) in m.iter_mut() {
                let escaped = escape_pointer(k);
                translate(
                    item,
                    &format!("{pointer}/{escaped}"),
                    resources,
                    types,
                    secret_paths,
                )?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Translates `${resources.<key>.<output>...}` and reports whether the output is secret.
fn translate_resource_placeholder(
    p: &RawPlaceholder,
    resources: &IndexMap<String, ResourceRequest>,
    types: &[ResourceType],
) -> Result<(String, bool), String> {
    let (key, output, rest) = match p.segments.as_slice() {
        [_, key, output, rest @ ..] => (
            key.key()
                .ok_or_else(|| format!("`{}`: expected `resources.<key>.<output>`", p.text))?,
            output
                .key()
                .ok_or_else(|| format!("`{}`: expected `resources.<key>.<output>`", p.text))?,
            rest,
        ),
        _ => return Err(format!("`{}`: expected `resources.<key>.<output>`", p.text)),
    };
    let r = resources.get(key).ok_or_else(|| {
        format!(
            "`{}`: resource `{key}` is not declared under `resources`",
            p.text
        )
    })?;
    let ty = types
        .iter()
        .find(|t| t.id == r.ty)
        .ok_or_else(|| format!("`{}`: unknown resource type `{}`", p.text, r.ty))?;
    let kind = match ty.output_kind(output) {
        Some(k) => k,
        None if ty.has_open_outputs() => OutputKind::Value,
        None => {
            return Err(format!(
                "`{}`: resource type `{}` has no output `{output}`",
                p.text, r.ty
            ));
        }
    };
    if kind == OutputKind::Secret && !rest.is_empty() {
        return Err(format!(
            "`{}`: secret output `{output}` has no fields",
            p.text
        ));
    }
    let head = match &r.id {
        Some(id) => format!("shared{}", segment_text(id)),
        None => format!("externals{}", segment_text(key)),
    };
    let mut text = format!("${{{head}{}", segment_text(output));
    for seg in rest {
        text.push_str(&seg.to_string());
    }
    text.push('}');
    Ok((text, kind == OutputKind::Secret))
}

/// A name as a path segment: `.name` when it can be dotted, `['name']` otherwise.
fn segment_text(name: &str) -> String {
    if name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        Segment::Name(name.to_owned()).to_string()
    } else {
        Segment::Quoted(name.to_owned()).to_string()
    }
}

/// Whether `s` is a Kubernetes-style quantity such as `256Mi`, `0.5` or `250m`.
pub fn is_quantity(s: &str) -> bool {
    let suffixes = [
        "Ki", "Mi", "Gi", "Ti", "Pi", "Ei", "k", "M", "G", "T", "P", "E", "m", "",
    ];
    suffixes.iter().any(|suf| {
        s.strip_suffix(suf).is_some_and(|num| {
            let mut parts = num.splitn(2, '.');
            let int = parts.next().unwrap_or("");
            let frac = parts.next();
            !int.is_empty()
                && int.bytes().all(|b| b.is_ascii_digit())
                && frac.is_none_or(|f| !f.is_empty() && f.bytes().all(|b| b.is_ascii_digit()))
        })
    })
}

/// Whether `s` is a duration such as `30s`, `5m` or `1h30m`.
pub fn is_duration(s: &str) -> bool {
    let mut rest = s;
    if rest.is_empty() {
        return false;
    }
    while !rest.is_empty() {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return false;
        }
        rest = &rest[digits..];
        let unit = ["ms", "s", "m", "h"]
            .into_iter()
            .find(|u| rest.starts_with(u));
        match unit {
            Some(u) => rest = &rest[u.len()..],
            None => return false,
        }
    }
    true
}

/// Whether `s` is a 5-field cron expression.
pub fn is_cron(s: &str) -> bool {
    let fields: Vec<&str> = s.split_whitespace().collect();
    fields.len() == 5
        && fields.iter().all(|f| {
            f.chars()
                .all(|c| c.is_ascii_alphanumeric() || "*,/-?LW#".contains(c))
        })
}

#[cfg(test)]
mod tests;
