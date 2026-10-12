//! Previews and destructive-action permits.
//!
//! A preview runs the checks a deployment's acceptance runs and classifies each
//! resource's change without calling drivers. Permits are issued from a stored
//! preview for the changes it marks as needing one, and a deployment presents
//! them. Acceptance runs the same preflight again, so a direct deploy cannot
//! skip it.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use openporch_engine::preview::{self, DriverPlan, Entry};
use openporch_store::{
    EnvRecord, NewPermit, NewPlan, PermitBinding, PermitRecord, PermitStore, PlanRecord, Store,
    StoreError,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::deploys::{self, Mode, Replay, TriggerRequest};
use super::envs;
use crate::api::{Ctx, JsonBody};
use crate::deploy;
use crate::error::{ApiError, ApiResult, code};
use crate::state::AppState;

/// What a preview previews.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanKind {
    /// A deployment, from the same sources as `POST .../deploys`.
    #[default]
    Deploy,
    /// Deleting the environment.
    DeleteEnv,
    /// Detaching one resource.
    Detach,
}

impl PlanKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Deploy => "deploy",
            Self::DeleteEnv => "delete_env",
            Self::Detach => "detach",
        }
    }
}

/// `POST .../plans`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanRequest {
    #[serde(default)]
    kind: PlanKind,
    #[serde(default)]
    delta_id: Option<String>,
    #[serde(default)]
    set_id: Option<String>,
    #[serde(default)]
    deploy_id: Option<String>,
    #[serde(default)]
    replay: Option<Replay>,
    #[serde(default)]
    mode: Mode,
    /// The resource to detach.
    #[serde(default)]
    descriptor: Option<String>,
    /// The environment generation the client read.
    expected_generation: i64,
}

/// A stored preview.
#[derive(Debug, Serialize)]
pub struct PlanView {
    id: String,
    kind: String,
    /// The environment generation it was made at; its permits bind only at
    /// this generation.
    base_generation: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    set_id: Option<String>,
    created_by: String,
    created_at: i64,
    /// When permits can no longer be issued from it.
    expires_at: i64,
    /// Always `false`: drivers plan when the deployment runs, so a preview is
    /// not a complete infrastructure plan.
    complete: bool,
    /// Each resource's change.
    resources: Value,
}

impl From<PlanRecord> for PlanView {
    fn from(p: PlanRecord) -> Self {
        Self {
            id: p.id,
            kind: p.kind,
            base_generation: p.base_generation,
            set_id: p.set_id,
            created_by: p.actor,
            created_at: p.created_at,
            expires_at: p.expires_at,
            complete: false,
            resources: p.body["resources"].clone(),
        }
    }
}

/// `POST /orgs/{org}/apps/{app}/envs/{env}/plans`: previews a deployment, the
/// environment's deletion or a detach.
pub async fn create(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, app, env)): Path<(String, String, String)>,
    JsonBody(req): JsonBody<PlanRequest>,
) -> ApiResult<(StatusCode, Json<PlanView>)> {
    let org = state.org();
    let env = deploys::env_or_404(&state, &app, &env).await?;
    if env.generation != req.expected_generation {
        return Err(StoreError::StaleGeneration {
            expected: req.expected_generation,
            actual: env.generation,
        }
        .into());
    }
    let deploying = req.kind == PlanKind::Deploy;
    if !deploying
        && (req.delta_id.is_some()
            || req.set_id.is_some()
            || req.deploy_id.is_some()
            || req.replay.is_some())
    {
        return Err(ApiError::invalid(format!(
            "a `{}` preview takes no set",
            req.kind.as_str()
        )));
    }
    if req.descriptor.is_some() != (req.kind == PlanKind::Detach) {
        return Err(ApiError::invalid(
            "`descriptor` names the resource to detach, and only a detach takes it",
        ));
    }
    let (set_id, entries) = match req.kind {
        PlanKind::Deploy => {
            if env.state != "active" {
                return Err(ApiError::conflict(format!(
                    "environment `{app}/{}` is {}",
                    env.id, env.state
                )));
            }
            let trigger = TriggerRequest::preview(
                req.delta_id,
                req.set_id,
                req.deploy_id,
                req.replay,
                req.mode,
                req.expected_generation,
            );
            deploys::check_trigger(&trigger)?;
            let resolved = deploys::resolve(&state, &env, &trigger).await?;
            let set_id = state.store.put_set(org, &app, &resolved.set).await?;
            let entries =
                preflight(&state, &env, set_id.clone(), &resolved.snapshot, false).await?;
            (Some(set_id), entries)
        }
        PlanKind::DeleteEnv => {
            let snapshot = envs::deletion_snapshot(&[]);
            (
                None,
                preflight(&state, &env, String::new(), &snapshot, true).await?,
            )
        }
        PlanKind::Detach => {
            let descriptor = req.descriptor.unwrap_or_default();
            let resource = state
                .store
                .active_resource(org, &app, &env.id, &descriptor)
                .await?
                .ok_or_else(|| {
                    ApiError::not_found(format!("active resource `{descriptor}` not found"))
                })?;
            (None, vec![preview::detach(&resource)])
        }
    };
    let plan = state
        .store
        .put_plan(NewPlan {
            org: org.to_owned(),
            app: app.clone(),
            env: env.id.clone(),
            kind: req.kind.as_str().to_owned(),
            base_generation: env.generation,
            set_id,
            body: json!({"resources": entries}),
            audit: ctx.audit(org, "plan.create", format!("{app}/{}", env.id)),
        })
        .await?;
    Ok((StatusCode::CREATED, Json(plan.into())))
}

/// `GET /orgs/{org}/apps/{app}/envs/{env}/plans/{plan}`.
pub async fn get(
    State(state): State<AppState>,
    Path((_, app, env, plan)): Path<(String, String, String, String)>,
) -> ApiResult<Json<PlanView>> {
    Ok(Json(plan_or_404(&state, &app, &env, &plan).await?.into()))
}

async fn plan_or_404(state: &AppState, app: &str, env: &str, id: &str) -> ApiResult<PlanRecord> {
    deploys::env_or_404(state, app, env).await?;
    state
        .store
        .plan(state.org(), app, env, id)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("plan `{id}` not found")))
}

/// One permit to issue.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermitAsk {
    /// The resource, as the preview names it.
    descriptor: String,
    /// `delete`, `replace`, `unknown` or `detach`: the preview's action.
    action: String,
    /// The driver plan a replacement covers; the preview's when omitted.
    #[serde(default)]
    fingerprint: Option<String>,
}

/// `POST .../plans/{plan}/permits`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermitsRequest {
    permits: Vec<PermitAsk>,
}

/// Issued permits.
#[derive(Debug, Serialize)]
pub struct PermitList {
    permits: Vec<PermitRecord>,
}

/// `POST /orgs/{org}/apps/{app}/envs/{env}/plans/{plan}/permits`: issues
/// permits for changes the preview marked as needing one. A detach permit
/// needs an administrator.
pub async fn permits(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, app, env, plan)): Path<(String, String, String, String)>,
    JsonBody(req): JsonBody<PermitsRequest>,
) -> ApiResult<(StatusCode, Json<PermitList>)> {
    let org = state.org();
    let record = plan_or_404(&state, &app, &env, &plan).await?;
    if req.permits.is_empty() {
        return Err(ApiError::invalid("ask for at least one permit"));
    }
    let resources = record.body["resources"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut issue = Vec::with_capacity(req.permits.len());
    for ask in req.permits {
        let entry = resources
            .iter()
            .find(|r| r["descriptor"] == ask.descriptor.as_str())
            .ok_or_else(|| {
                ApiError::invalid(format!(
                    "plan `{plan}` does not change `{}`",
                    ask.descriptor
                ))
            })?;
        if entry["permit_required"] != true || entry["action"] != ask.action.as_str() {
            return Err(ApiError::invalid(format!(
                "plan `{plan}` shows `{}` as {}, which needs no `{}` permit",
                ask.descriptor,
                entry["action"].as_str().unwrap_or("unchanged"),
                ask.action
            )));
        }
        let planned = entry["driver_plan"]["fingerprint"].as_str();
        if ask.fingerprint.is_some() && ask.fingerprint.as_deref() != planned {
            return Err(ApiError::invalid(format!(
                "plan `{plan}` shows no driver plan `{}` for `{}`",
                ask.fingerprint.as_deref().unwrap_or_default(),
                ask.descriptor
            )));
        }
        issue.push(NewPermit {
            descriptor: ask.descriptor,
            guresid: entry["guresid"].as_str().map(str::to_owned),
            action: ask.action,
            plan_fingerprint: planned.map(str::to_owned),
            config_digest: entry["digest"].as_str().unwrap_or_default().to_owned(),
        });
    }
    let permits = state
        .store
        .issue_permits(
            org,
            &app,
            &env,
            &plan,
            issue,
            ctx.audit(org, "permit.create", format!("{app}/{env}/{plan}")),
        )
        .await?;
    Ok((StatusCode::CREATED, Json(PermitList { permits })))
}

/// Previews a deployment with `snapshot` in `env`: the checks every
/// acceptance runs. A move to another driver, cloud account or state backend
/// is `409`.
pub(super) async fn preflight(
    state: &AppState,
    env: &EnvRecord,
    set_id: String,
    snapshot: &Value,
    teardown: bool,
) -> ApiResult<Vec<Entry>> {
    let org = state.org();
    let deployment = deploy::planned(
        state, &env.app, &env.id, "preview", set_id, snapshot, teardown,
    )
    .await
    .map_err(ApiError::invalid)?;
    let active = state.store.active_resources(org, &env.app, &env.id).await?;
    // Only the latest attempt's refused driver plans are current.
    let latest = state
        .store
        .latest_deployment(org, &env.app, &env.id)
        .await?
        .map(|d| d.id);
    let plans: BTreeMap<String, DriverPlan> = state
        .store
        .approval_requests(org, &env.app, &env.id)
        .await?
        .into_iter()
        .filter(|r| latest.as_deref() == Some(r.deployment_id.as_str()))
        .map(|r| {
            (
                r.guresid,
                DriverPlan {
                    fingerprint: r.fingerprint,
                    actions: r.actions,
                },
            )
        })
        .collect();
    preview::preview(&deployment, &active, |id| state.drivers.facts(id), &plans).map_err(
        |migrations| {
            ApiError::new(
                StatusCode::CONFLICT,
                code::MIGRATION_REQUIRED,
                migrations.join("; "),
            )
        },
    )
}

/// What an acceptance checks `presented` permits against: every previewed
/// resource's configuration, and the changes that need a permit.
pub(super) fn binding(entries: &[Entry], presented: &[String]) -> PermitBinding {
    PermitBinding {
        presented: presented.to_vec(),
        digests: entries
            .iter()
            .map(|e| (e.descriptor.clone(), e.digest.clone()))
            .collect(),
        required: entries
            .iter()
            .filter(|e| e.permit_required)
            .filter_map(|e| {
                e.action
                    .permit_action()
                    .map(|a| (e.descriptor.clone(), a.to_owned()))
            })
            .collect(),
    }
}
