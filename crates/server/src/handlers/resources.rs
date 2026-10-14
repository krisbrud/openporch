//! Active resources: what deployments provisioned in an environment, with usage.
//!
//! Secret outputs are listed by name only. A resource stays listed after a
//! deployment stops referencing it, because it still exists and may be billed.

use std::collections::{BTreeMap, BTreeSet};

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use openporch_core::graph::ResourceGraph;
use openporch_core::ids::Descriptor;
use openporch_engine::planner::{RecoverError, Recovered};
use openporch_store::{
    ActiveResource, DeploymentStatus, Detach, EnvRecord, Resolution, Store, Tombstone,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::catalog::definition_or_404;
use super::envs::env_or_404;
use crate::api::{Ctx, JsonBody};
use crate::deploy::Recovery;
use crate::error::{ApiError, ApiResult, code};
use crate::state::AppState;

/// An active resource as clients see it.
#[derive(Debug, Serialize)]
pub struct ResourceView {
    /// Application.
    pub app: String,
    /// Environment.
    pub env: String,
    /// `<type>.<class>#<res_id>`.
    pub descriptor: String,
    /// Permanent identity.
    pub guresid: String,
    /// The definition that provisioned it.
    pub definition_id: Option<String>,
    /// That definition's version.
    pub definition_version: Option<i64>,
    /// The driver.
    pub driver_type: Option<String>,
    /// The cloud account it was provisioned in.
    pub driver_account: Option<String>,
    /// Status.
    pub status: String,
    /// The operation whose outcome is unknown, while the status is `unknown`
    /// or `recovery_required`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unknown_operation: Option<String>,
    /// `owned`, `adopted` or `external`.
    pub ownership: String,
    /// Non-secret outputs.
    pub outputs: Value,
    /// The names of secret outputs; their values are never returned.
    pub secret_outputs: Vec<String>,
    /// The resources it depends on.
    pub dependencies: Vec<String>,
    /// Whether the next deployment that leaves it out deletes it.
    pub scheduled_deletion: bool,
    /// How deployments use it.
    pub usage: Usage,
    /// When the status was last observed.
    pub observed_at: Option<i64>,
    /// When it was first recorded.
    pub created_at: i64,
}

/// How deployments use a resource.
#[derive(Debug, Serialize)]
pub struct Usage {
    /// The deployment that last provisioned it.
    pub last_deploy_id: Option<String>,
    /// How many deployments of the environment came after that one: `0` when
    /// the latest deployment provisioned it.
    pub deploys_ago: Option<i64>,
    /// Whether the environment's latest deployment includes it.
    pub referenced: bool,
}

/// A list of resources.
#[derive(Debug, Serialize)]
pub struct ResourceList {
    /// The resources.
    pub resources: Vec<ResourceView>,
}

/// The facts about an environment's deployments that usage derives from.
struct EnvUsage {
    latest_seq: Option<i64>,
    seqs: BTreeMap<String, i64>,
    referenced: BTreeSet<String>,
}

impl EnvUsage {
    async fn load(
        state: &AppState,
        app: &str,
        env: &str,
        resources: &[ActiveResource],
    ) -> ApiResult<Self> {
        let org = state.org();
        let latest = state.store.deployments(org, app, env, 1, None).await?;
        let mut usage = Self {
            latest_seq: latest.first().map(|d| d.seq),
            seqs: BTreeMap::new(),
            referenced: BTreeSet::new(),
        };
        if let Some(d) = latest.first() {
            usage.seqs.insert(d.id.clone(), d.seq);
            if let Some(snapshot) = state
                .store
                .deployment_snapshot(org, app, env, &d.id)
                .await?
            {
                let nodes = snapshot["graph"]["nodes"].as_object();
                usage.referenced = nodes
                    .into_iter()
                    .flatten()
                    .map(|(k, _)| k.clone())
                    .collect();
            }
        }
        let ids: BTreeSet<&String> = resources
            .iter()
            .filter_map(|r| r.last_deploy_id.as_ref())
            .collect();
        for id in ids {
            if usage.seqs.contains_key(id) {
                continue;
            }
            if let Some(d) = state.store.deployment(org, app, env, id).await? {
                usage.seqs.insert(d.id, d.seq);
            }
        }
        Ok(usage)
    }

    fn view(&self, r: ActiveResource) -> ResourceView {
        let deploys_ago = r
            .last_deploy_id
            .as_ref()
            .and_then(|id| self.seqs.get(id))
            .zip(self.latest_seq)
            .map(|(seq, latest)| latest - seq);
        ResourceView {
            usage: Usage {
                referenced: self.referenced.contains(&r.descriptor),
                last_deploy_id: r.last_deploy_id,
                deploys_ago,
            },
            app: r.app,
            env: r.env,
            descriptor: r.descriptor,
            guresid: r.guresid,
            definition_id: r.definition_id,
            definition_version: r.definition_version,
            driver_type: r.driver_type,
            driver_account: r.driver_account,
            status: r.status,
            unknown_operation: None,
            ownership: r.ownership,
            outputs: r.outputs,
            secret_outputs: r.secret_outputs.into_keys().collect(),
            dependencies: r.dependencies,
            scheduled_deletion: r.scheduled_deletion,
            observed_at: r.observed_at,
            created_at: r.created_at,
        }
    }
}

/// `GET /orgs/{org}/apps/{app}/envs/{env}/resources`.
pub async fn list(
    State(state): State<AppState>,
    Path((_, app, env)): Path<(String, String, String)>,
) -> ApiResult<Json<ResourceList>> {
    let env = env_or_404(&state, &app, &env).await?;
    let resources = state
        .store
        .active_resources(state.org(), &env.app, &env.id)
        .await?;
    let usage = EnvUsage::load(&state, &env.app, &env.id, &resources).await?;
    let mut views = Vec::with_capacity(resources.len());
    for r in resources {
        views.push(with_operation(&state, usage.view(r)).await?);
    }
    Ok(Json(ResourceList { resources: views }))
}

/// `view` with the operation whose outcome is unknown, if its status says
/// there is one.
async fn with_operation(state: &AppState, mut view: ResourceView) -> ApiResult<ResourceView> {
    if view.status == "unknown" || view.status == "recovery_required" {
        view.unknown_operation = state
            .store
            .unknown_operation(state.org(), &view.app, &view.env, &view.descriptor)
            .await?
            .map(|op| op.operation_id);
    }
    Ok(view)
}

/// `GET /orgs/{org}/apps/{app}/envs/{env}/resources/{type}/{res_id}`.
pub async fn get(
    State(state): State<AppState>,
    Path((_, app, env, ty, res_id)): Path<(String, String, String, String, String)>,
) -> ApiResult<Json<ResourceView>> {
    let env = env_or_404(&state, &app, &env).await?;
    let resource = resource_or_404(&state, &env, &ty, &res_id).await?;
    let usage = EnvUsage::load(&state, &env.app, &env.id, std::slice::from_ref(&resource)).await?;
    Ok(Json(with_operation(&state, usage.view(resource)).await?))
}

/// `DELETE /orgs/{org}/apps/{app}/envs/{env}/resources/{type}/{res_id}`:
/// schedules the resource for deletion. The next deployment that leaves it
/// out deletes it, given a destructive-action permit; until then it stays.
pub async fn schedule_deletion(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, app, env, ty, res_id)): Path<(String, String, String, String, String)>,
) -> ApiResult<(StatusCode, Json<ResourceView>)> {
    let env = env_or_404(&state, &app, &env).await?;
    let resource = resource_or_404(&state, &env, &ty, &res_id).await?;
    let org = state.org();
    state
        .store
        .schedule_deletion(
            org,
            &env.app,
            &env.id,
            &resource.descriptor,
            ctx.audit(
                org,
                "resource.schedule_deletion",
                format!("{}/{}/{}", env.app, env.id, resource.descriptor),
            ),
        )
        .await?;
    // A deployment that started right after may already have deleted it; the
    // request was still accepted.
    let resource = match state
        .store
        .active_resource(org, &env.app, &env.id, &resource.descriptor)
        .await?
    {
        Some(current) => current,
        None => ActiveResource {
            scheduled_deletion: true,
            ..resource
        },
    };
    let usage = EnvUsage::load(&state, &env.app, &env.id, std::slice::from_ref(&resource)).await?;
    Ok((StatusCode::ACCEPTED, Json(usage.view(resource))))
}

/// A resource in a deployment's graph.
#[derive(Debug, Serialize)]
pub struct GraphNode {
    /// `<type>.<class>#<res_id>`.
    pub descriptor: String,
    /// The matched definition; absent for a resource the deployment deleted or
    /// kept, which is not in its graph.
    pub definition_id: Option<String>,
    /// The definition's driver.
    pub driver_type: Option<String>,
    /// Its permanent identity, once it has one.
    pub guresid: Option<String>,
    /// How it fared: `pending` until the deployment reaches it.
    pub status: String,
    /// The failure, if any.
    pub error: Option<String>,
}

/// A dependency: `from` uses `to`.
#[derive(Debug, Serialize)]
pub struct GraphEdge {
    /// The dependent resource.
    pub from: String,
    /// The resource it depends on.
    pub to: String,
}

/// The resource graph of an environment's latest deployment.
#[derive(Debug, Serialize)]
pub struct GraphView {
    /// The deployment.
    pub deployment_id: String,
    /// Its status.
    pub status: DeploymentStatus,
    /// Its resources, by descriptor.
    pub nodes: Vec<GraphNode>,
    /// Their dependencies.
    pub edges: Vec<GraphEdge>,
}

/// `GET /orgs/{org}/apps/{app}/envs/{env}/resources/graph`: the latest
/// attempt's graph with each resource's status and error.
pub async fn graph(
    State(state): State<AppState>,
    Path((_, app, env)): Path<(String, String, String)>,
) -> ApiResult<Json<GraphView>> {
    let env = env_or_404(&state, &app, &env).await?;
    let org = state.org();
    // The latest attempt, also an environment deletion, whose graph is empty
    // and whose nodes are what it deleted, kept or failed to delete.
    let latest = state
        .store
        .latest_deployment(org, &env.app, &env.id)
        .await?;
    let d = latest.ok_or_else(|| {
        ApiError::not_found(format!("`{}/{}` has no deployments yet", env.app, env.id))
    })?;
    let snapshot = state
        .store
        .deployment_snapshot(org, &env.app, &env.id, &d.id)
        .await?
        .unwrap_or_default();
    if !snapshot["graph"].is_object() {
        return Err(ApiError::conflict(format!(
            "deployment `{}` recorded no graph",
            d.id
        )));
    }
    let graph: ResourceGraph = serde_json::from_value(snapshot["graph"].clone()).map_err(|e| {
        ApiError::internal(format!("deployment `{}` has no readable graph: {e}", d.id))
    })?;
    let mut runs: BTreeMap<String, _> = state
        .store
        .deployment_resources(org, &env.app, &env.id, &d.id)
        .await?
        .into_iter()
        .map(|r| (r.descriptor.clone(), r))
        .collect();
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    for (descriptor, node) in &graph.nodes {
        let descriptor = descriptor.to_string();
        let run = runs.remove(&descriptor);
        for dep in &node.depends_on {
            edges.push(GraphEdge {
                from: descriptor.clone(),
                to: dep.to_string(),
            });
        }
        nodes.push(GraphNode {
            definition_id: Some(node.definition.to_string()),
            driver_type: Some(node.driver_type.clone()),
            guresid: run
                .as_ref()
                .map(|r| r.guresid.clone())
                .filter(|g| !g.is_empty()),
            status: run
                .as_ref()
                .map_or_else(|| "pending".to_owned(), |r| r.status.clone()),
            error: run.and_then(|r| r.error),
            descriptor,
        });
    }
    // What the deployment deleted or kept while removing it.
    for (descriptor, run) in runs {
        nodes.push(GraphNode {
            descriptor,
            definition_id: None,
            driver_type: None,
            guresid: Some(run.guresid).filter(|g| !g.is_empty()),
            status: run.status,
            error: run.error,
        });
    }
    nodes.sort_by(|a, b| a.descriptor.cmp(&b.descriptor));
    Ok(Json(GraphView {
        deployment_id: d.id,
        status: d.status,
        nodes,
        edges,
    }))
}

/// The longest detach reason accepted.
const MAX_REASON: usize = 1000;

/// A request to detach a resource.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DetachRequest {
    /// Why; recorded in the tombstone and the audit log.
    pub reason: String,
    /// Destructive-action permits.
    #[serde(default)]
    pub permits: Vec<String>,
}

/// `POST /orgs/{org}/apps/{app}/envs/{env}/resources/{type}/{res_id}/detach`:
/// stops managing the resource without calling its driver, so the real object
/// stays, and returns its tombstone.
pub async fn detach(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, app, env, ty, res_id)): Path<(String, String, String, String, String)>,
    JsonBody(req): JsonBody<DetachRequest>,
) -> ApiResult<Json<Tombstone>> {
    let reason = req.reason.trim();
    if reason.is_empty() || reason.chars().count() > MAX_REASON {
        return Err(ApiError::invalid(format!(
            "`reason` must hold 1 to {MAX_REASON} characters"
        )));
    }
    let env = env_or_404(&state, &app, &env).await?;
    let resource = resource_or_404(&state, &env, &ty, &res_id).await?;
    let org = state.org();
    let mut audit = ctx.audit(
        org,
        "resource.detach",
        format!("{}/{}/{}", env.app, env.id, resource.descriptor),
    );
    audit.reason = Some(reason.to_owned());
    let tombstone = state
        .store
        .detach_resource(
            org,
            &env.app,
            &env.id,
            Detach {
                permits: Some(&req.permits),
                descriptor: &resource.descriptor,
                guresid: &resource.guresid,
                reason,
            },
            audit,
        )
        .await?;
    Ok(Json(tombstone))
}

/// What recovering a resource established.
#[derive(Debug, Serialize)]
pub struct RecoveryView {
    /// `provisioned`, `deleted`, `repeatable` (the next deployment repeats
    /// the operation with the same identity), `in_progress` (nothing changed;
    /// recover again later) or `recovery_required` (an administrator must
    /// resolve it).
    pub outcome: &'static str,
    /// What the observation found.
    pub reason: Option<String>,
    /// The resource as it is now; absent once it is deleted.
    pub resource: Option<ResourceView>,
}

/// `POST /orgs/{org}/apps/{app}/envs/{env}/resources/{type}/{res_id}/recover`:
/// observes a resource whose last operation has an unknown outcome, without
/// changing it, and records the outcome the observation establishes.
pub async fn recover(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, app, env, ty, res_id)): Path<(String, String, String, String, String)>,
) -> ApiResult<Json<RecoveryView>> {
    let env = env_or_404(&state, &app, &env).await?;
    let resource = resource_or_404(&state, &env, &ty, &res_id).await?;
    let org = state.org();
    let audit = ctx.audit(
        org,
        "resource.recover",
        format!("{}/{}/{}", env.app, env.id, resource.descriptor),
    );
    let recovered = state
        .runners
        .recover(Recovery {
            app: env.app.clone(),
            env: env.id.clone(),
            descriptor: resource.descriptor.clone(),
            audit,
        })
        .await
        .map_err(|e| match e {
            RecoverError::NothingToRecover(_) => ApiError::conflict(e.to_string()),
            RecoverError::Observation(_) => ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                code::OBSERVATION_FAILED,
                e.to_string(),
            ),
            RecoverError::Store(e) => e.into(),
        })?;
    let (outcome, reason) = match recovered {
        Recovered::Provisioned => ("provisioned", None),
        Recovered::Deleted => ("deleted", None),
        Recovered::Repeatable(r) => ("repeatable", Some(r)),
        Recovered::InProgress => ("in_progress", None),
        Recovered::RecoveryRequired(r) => ("recovery_required", Some(r)),
    };
    let resource = current_view(&state, &env, &resource.descriptor).await?;
    Ok(Json(RecoveryView {
        outcome,
        reason,
        resource,
    }))
}

/// The request to resolve an unknown outcome.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolveRequest {
    /// How the administrator established that repeating the operation is
    /// safe; recorded with the operation and in the audit log.
    pub reason: String,
}

/// `POST /orgs/{org}/apps/{app}/envs/{env}/resources/{type}/{res_id}/resolve`:
/// an administrator states that repeating the resource's operation with the
/// same identity is safe, which unblocks it; the next deployment repeats it.
pub async fn resolve(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, app, env, ty, res_id)): Path<(String, String, String, String, String)>,
    JsonBody(req): JsonBody<ResolveRequest>,
) -> ApiResult<Json<ResourceView>> {
    let reason = req.reason.trim();
    if reason.is_empty() || reason.chars().count() > MAX_REASON {
        return Err(ApiError::invalid(format!(
            "`reason` must hold 1 to {MAX_REASON} characters"
        )));
    }
    let env = env_or_404(&state, &app, &env).await?;
    let resource = resource_or_404(&state, &env, &ty, &res_id).await?;
    let org = state.org();
    let op = state
        .store
        .unknown_operation(org, &env.app, &env.id, &resource.descriptor)
        .await?
        .ok_or_else(|| {
            ApiError::conflict(format!(
                "`{}` has no operation with an unknown outcome",
                resource.descriptor
            ))
        })?;
    let mut audit = ctx.audit(
        org,
        "resource.resolve",
        format!("{}/{}/{}", env.app, env.id, resource.descriptor),
    );
    audit.reason = Some(reason.to_owned());
    state
        .store
        .resolve_operation(
            &op.operation_id,
            Resolution::Operator {
                reason: reason.to_owned(),
            },
            audit,
        )
        .await?;
    current_view(&state, &env, &resource.descriptor)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("active resource `{}`", resource.descriptor)))
}

/// The resource's current view; `None` once it is gone.
async fn current_view(
    state: &AppState,
    env: &EnvRecord,
    descriptor: &str,
) -> ApiResult<Option<ResourceView>> {
    let Some(resource) = state
        .store
        .active_resource(state.org(), &env.app, &env.id, descriptor)
        .await?
    else {
        return Ok(None);
    };
    let usage = EnvUsage::load(state, &env.app, &env.id, std::slice::from_ref(&resource)).await?;
    Ok(Some(usage.view(resource)))
}

/// A list of tombstones.
#[derive(Debug, Serialize)]
pub struct TombstoneList {
    /// The tombstones, oldest first.
    pub tombstones: Vec<Tombstone>,
}

/// `GET /orgs/{org}/apps/{app}/envs/{env}/tombstones`: what remains of the
/// environment's deleted and detached resources.
pub async fn tombstones(
    State(state): State<AppState>,
    Path((_, app, env)): Path<(String, String, String)>,
) -> ApiResult<Json<TombstoneList>> {
    let env = env_or_404(&state, &app, &env).await?;
    let tombstones = state
        .store
        .tombstones(state.org(), &env.app, &env.id)
        .await?;
    Ok(Json(TombstoneList { tombstones }))
}

/// `GET /orgs/{org}/resources/defs/{def}/resources`: every environment's
/// resources that a definition provisioned.
pub async fn for_definition(
    State(state): State<AppState>,
    Path((_, def)): Path<(String, String)>,
) -> ApiResult<Json<ResourceList>> {
    definition_or_404(&state, &def).await?;
    let resources = state.store.definition_resources(state.org(), &def).await?;
    let mut by_env: BTreeMap<(String, String), Vec<ActiveResource>> = BTreeMap::new();
    for r in resources {
        by_env
            .entry((r.app.clone(), r.env.clone()))
            .or_default()
            .push(r);
    }
    let mut views = Vec::new();
    for ((app, env), resources) in by_env {
        let usage = EnvUsage::load(&state, &app, &env, &resources).await?;
        views.extend(resources.into_iter().map(|r| usage.view(r)));
    }
    Ok(Json(ResourceList { resources: views }))
}

/// The resource of type `ty` with ID `res_id`, whatever its class.
async fn resource_or_404(
    state: &AppState,
    env: &EnvRecord,
    ty: &str,
    res_id: &str,
) -> ApiResult<ActiveResource> {
    let not_found = || {
        ApiError::not_found(format!(
            "resource `{ty}/{res_id}` not found in `{}/{}`",
            env.app, env.id
        ))
    };
    let mut matching: Vec<ActiveResource> = state
        .store
        .active_resources(state.org(), &env.app, &env.id)
        .await?
        .into_iter()
        .filter(|r| {
            Descriptor::parse(&r.descriptor)
                .is_ok_and(|d| d.ty.as_str() == ty && d.id.as_str() == res_id)
        })
        .collect();
    match matching.len() {
        0 => Err(not_found()),
        1 => Ok(matching.remove(0)),
        _ => Err(ApiError::conflict(format!(
            "`{ty}/{res_id}` names resources of several classes in `{}/{}`",
            env.app, env.id
        ))),
    }
}
