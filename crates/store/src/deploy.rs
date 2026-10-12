//! Records for environments, deployments, driver operations and active resources.

use std::collections::BTreeMap;

use openporch_core::ids::{Descriptor, Guresid};
use openporch_core::model::SecretRef;
use serde::Serialize;
use serde_json::Value;

use crate::AuditIntent;
use crate::secrets::Sealed;

/// An environment's runtime summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EnvRecord {
    /// Application ID.
    pub app: String,
    /// Environment ID.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Environment type.
    pub env_type: String,
    /// Incremented by every accepted deployment or deletion.
    pub generation: i64,
    /// The set the latest accepted deployment converges to.
    pub desired_set_id: Option<String>,
    /// The set of the latest successful deployment.
    pub last_successful_set_id: Option<String>,
    /// The latest successful deployment.
    pub last_successful_deploy_id: Option<String>,
    /// `active`, `deleting` or `deletion_failed`.
    pub state: String,
}

/// A new environment.
#[derive(Debug, Clone, PartialEq)]
pub struct NewEnv {
    /// Environment ID.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Environment type.
    pub env_type: String,
    /// A draft delta created with it, for a clone.
    pub draft: Option<DraftDelta>,
}

/// A draft delta created together with an environment.
#[derive(Debug, Clone, PartialEq)]
pub struct DraftDelta {
    /// Delta ID.
    pub id: String,
    /// The delta.
    pub content: Value,
}

/// A stored delta.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DeltaRecord {
    /// Delta ID.
    pub id: String,
    /// The environment it was created for, if any.
    pub env: Option<String>,
    /// The delta.
    pub content: Value,
    /// Whether a deployment froze it.
    pub frozen: bool,
    /// The creating principal.
    pub created_by: Option<String>,
}

/// A request to accept a deployment.
#[derive(Debug, Clone)]
pub struct DeploymentRequest {
    /// Organization.
    pub org: String,
    /// Application.
    pub app: String,
    /// Environment.
    pub env: String,
    /// The requesting principal.
    pub actor: String,
    /// The generation the client read; a different current generation is a conflict.
    pub expected_generation: i64,
    /// The client's idempotency key, scoped to principal, application and environment.
    pub idempotency_key: Option<String>,
    /// A digest of the request content, compared on retries with the same key.
    pub fingerprint: String,
    /// The desired set, already stored with [`crate::Store::put_set`].
    pub set_id: String,
    /// The delta being deployed, frozen on acceptance.
    pub delta_id: Option<String>,
    /// The delta's content the set was derived from; acceptance fails if the
    /// stored delta differs.
    pub delta_content: Option<Value>,
    /// Free-text comment.
    pub comment: String,
    /// The execution snapshot: configuration captured at acceptance.
    pub snapshot: Value,
    /// The audit intent of the request.
    pub audit: AuditIntent,
}

/// The status of a deployment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentStatus {
    /// Running.
    InProgress,
    /// Every resource converged.
    Succeeded,
    /// At least one resource failed, or the deployment was interrupted.
    Failed,
}

impl DeploymentStatus {
    pub(crate) fn parse(s: &str) -> crate::Result<Self> {
        match s {
            "in_progress" => Ok(Self::InProgress),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            other => Err(crate::StoreError::Corrupt(format!(
                "deployment status `{other}`"
            ))),
        }
    }
}

/// A stored deployment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeploymentRecord {
    /// Deployment ID.
    pub id: String,
    /// Application.
    pub app: String,
    /// Environment.
    pub env: String,
    /// Attempt order within the environment, starting at 1.
    pub seq: i64,
    /// The principal that requested it.
    pub actor: String,
    /// The desired set.
    pub set_id: Option<String>,
    /// The delta that produced the set.
    pub delta_id: Option<String>,
    /// Comment.
    pub comment: String,
    /// The environment generation this deployment created.
    pub generation: i64,
    /// Status.
    pub status: DeploymentStatus,
    /// The failure, if any.
    pub error: Option<String>,
    /// Acceptance time.
    pub created_at: i64,
    /// Completion time.
    pub finished_at: Option<i64>,
}

/// The result of [`crate::Store::accept_deployment`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Acceptance {
    /// A new deployment was accepted.
    Created(DeploymentRecord),
    /// A retry with the same idempotency key and content; nothing changed.
    Replayed(DeploymentRecord),
}

impl Acceptance {
    /// The deployment.
    pub fn deployment(&self) -> &DeploymentRecord {
        match self {
            Self::Created(d) | Self::Replayed(d) => d,
        }
    }
}

/// The kind of driver operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationKind {
    /// Create or update.
    Put,
    /// Delete.
    Delete,
}

impl OperationKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Put => "put",
            Self::Delete => "delete",
        }
    }
}

/// A driver operation about to start.
#[derive(Debug, Clone)]
pub struct OperationStart {
    /// Organization.
    pub org: String,
    /// Application.
    pub app: String,
    /// Environment.
    pub env: String,
    /// The deployment running the operation.
    pub deployment_id: String,
    /// The resource.
    pub descriptor: Descriptor,
    /// The matched definition.
    pub definition_id: Option<String>,
    /// The definition's version.
    pub definition_version: Option<i64>,
    /// The driver.
    pub driver_type: Option<String>,
    /// Put or delete.
    pub kind: OperationKind,
    /// A digest of the resolved inputs, so retries can be recognized.
    pub input_fingerprint: String,
    /// Non-secret metadata needed to recover or destroy the resource.
    pub recovery: Value,
}

/// A persisted operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationStarted {
    /// The operation ID, passed to the driver as its idempotency identity.
    pub operation_id: String,
    /// The resource's permanent GUResID.
    pub guresid: Guresid,
    /// The attempt number within the deployment, starting at 1.
    pub attempt: i64,
}

/// The result of a successful put.
#[derive(Debug, Clone)]
pub struct Provisioned {
    /// Non-secret outputs.
    pub outputs: Value,
    /// Secret outputs, already written to a secret store.
    pub secret_outputs: BTreeMap<String, SecretRef>,
    /// The driver cookie, encrypted.
    pub cookie: Option<Sealed>,
    /// Recovery metadata after the operation.
    pub recovery: Value,
    /// The inputs it was provisioned with, secrets as references.
    pub inputs: Value,
    /// The descriptors of the resources it depends on.
    pub dependencies: Vec<String>,
    /// The configuration it was provisioned from, such as its definition and
    /// version.
    pub snapshot: Value,
    /// The cloud account it was provisioned in.
    pub account: Option<String>,
}

/// The outcome of a driver operation.
#[derive(Debug, Clone)]
pub enum CheckpointResult {
    /// The resource was provisioned.
    Provisioned(Box<Provisioned>),
    /// The resource was deleted; its record is replaced by a tombstone.
    Deleted,
    /// The operation failed with a known outcome.
    Failed {
        /// The redacted error.
        error: String,
    },
    /// It is unknown whether the operation took effect.
    Unknown {
        /// The redacted error.
        error: String,
    },
}

/// The status of a resource in a deployment that no operation decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceRunStatus {
    /// Not started, because a dependency did not provision or time ran out.
    Pending,
    /// Deliberately not visited.
    Skipped,
    /// Failed before an operation was recorded.
    Failed,
    /// Its outcome is unknown.
    Unknown,
}

impl ResourceRunStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Skipped => "skipped",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }
}

/// An active resource. It holds secret references and the encrypted cookie,
/// so it is not serialized directly into responses.
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveResource {
    /// Application.
    pub app: String,
    /// Environment.
    pub env: String,
    /// `<type>.<class>#<res_id>`.
    pub descriptor: String,
    /// Permanent identity.
    pub guresid: String,
    /// The matched definition.
    pub definition_id: Option<String>,
    /// The definition's version.
    pub definition_version: Option<i64>,
    /// The driver.
    pub driver_type: Option<String>,
    /// The cloud account it was provisioned in.
    pub driver_account: Option<String>,
    /// Status.
    pub status: String,
    /// The inputs of the last successful operation, secrets as references.
    pub inputs: Value,
    /// Non-secret outputs.
    pub outputs: Value,
    /// Secret outputs as references.
    pub secret_outputs: BTreeMap<String, SecretRef>,
    /// The encrypted cookie.
    pub cookie: Option<Sealed>,
    /// The configuration of the last successful operation.
    pub snapshot: Value,
    /// `owned`, `adopted` or `external`.
    pub ownership: String,
    /// The descriptors of the resources it depends on.
    pub dependencies: Vec<String>,
    /// The deployment that last provisioned it.
    pub last_deploy_id: Option<String>,
    /// Whether it is deleted by the next deployment that leaves it out.
    pub scheduled_deletion: bool,
    /// When the status was last observed.
    pub observed_at: Option<i64>,
    /// When it was first recorded.
    pub created_at: i64,
}

/// A driver operation whose outcome is unknown after a restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UncertainOperation {
    /// Operation ID.
    pub operation_id: String,
    /// The deployment it belonged to.
    pub deployment_id: String,
    /// The resource.
    pub guresid: String,
}

/// What [`crate::Store::recover_interrupted`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Deployments that were running and are now failed.
    pub failed_deployments: Vec<String>,
    /// Operations that must be resolved before conflicting mutations.
    pub uncertain: Vec<UncertainOperation>,
}
