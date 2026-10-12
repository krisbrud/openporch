//! Active resources: what deployments provisioned in an environment, with usage.
//!
//! Secret outputs are listed by name only. A resource stays listed after a
//! deployment stops referencing it, because it still exists and may be billed.

use std::collections::{BTreeMap, BTreeSet};

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use openporch_core::ids::Descriptor;
use openporch_store::{ActiveResource, EnvRecord, Store};
use serde::Serialize;
use serde_json::Value;

use super::catalog::definition_or_404;
use super::envs::env_or_404;
use crate::api::Ctx;
use crate::error::{ApiError, ApiResult};
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
    Ok(Json(ResourceList {
        resources: resources.into_iter().map(|r| usage.view(r)).collect(),
    }))
}

/// `GET /orgs/{org}/apps/{app}/envs/{env}/resources/{type}/{res_id}`.
pub async fn get(
    State(state): State<AppState>,
    Path((_, app, env, ty, res_id)): Path<(String, String, String, String, String)>,
) -> ApiResult<Json<ResourceView>> {
    let env = env_or_404(&state, &app, &env).await?;
    let resource = resource_or_404(&state, &env, &ty, &res_id).await?;
    let usage = EnvUsage::load(&state, &env.app, &env.id, std::slice::from_ref(&resource)).await?;
    Ok(Json(usage.view(resource)))
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
