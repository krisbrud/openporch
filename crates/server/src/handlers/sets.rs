//! Deployment sets, deltas and set diffs.
//!
//! Sets are content addressed and immutable, so storing one twice is a no-op. The
//! empty set is always readable, by its computed ID or by the alias `0`.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use openporch_core::catalog::builtin_types;
use openporch_core::sets::{Delta, DeploymentSet, apply_delta, diff_sets, workload_delta};
use openporch_core::workload::{Overrides, Workload};
use openporch_store::{CatalogStore, DeltaRecord, Store};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api::{Ctx, JsonBody};
use crate::auth;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// The alias for the empty set.
pub const EMPTY_SET_ALIAS: &str = "0";

/// A set with its ID.
#[derive(Serialize)]
pub struct SetView {
    id: String,
    #[serde(flatten)]
    set: DeploymentSet,
}

/// The ID of a stored or derived set.
#[derive(Serialize)]
pub struct SetId {
    id: String,
}

/// Reads a set of `app`; the empty set exists in every application.
pub(super) async fn set_or_404(state: &AppState, app: &str, id: &str) -> ApiResult<DeploymentSet> {
    let empty = DeploymentSet::default();
    if id == EMPTY_SET_ALIAS || id == empty.id() {
        return Ok(empty);
    }
    state
        .store
        .set(state.org(), app, id)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("set `{id}` not found in `{app}`")))
}

pub async fn get(
    State(state): State<AppState>,
    Path((_, app, id)): Path<(String, String, String)>,
) -> ApiResult<Json<SetView>> {
    let set = set_or_404(&state, &app, &id).await?;
    Ok(Json(SetView { id: set.id(), set }))
}

/// Applies a delta to a set and stores the result, audited with the new set ID.
pub async fn apply(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, app, id)): Path<(String, String, String)>,
    JsonBody(delta): JsonBody<Delta>,
) -> ApiResult<Json<SetId>> {
    let base = set_or_404(&state, &app, &id).await?;
    let next = apply_delta(&base, &delta).map_err(|e| ApiError::invalid(e.to_string()))?;
    let org = state.org();
    let audit = ctx.audit(org, "set.create", format!("{app}/{}", next.id()));
    let id = state.store.put_set_audited(org, &app, &next, audit).await?;
    Ok(Json(SetId { id }))
}

/// The delta that transforms set `a` into set `b`.
pub async fn diff(
    State(state): State<AppState>,
    Path((_, app, a, b)): Path<(String, String, String, String)>,
) -> ApiResult<Json<Delta>> {
    let a = set_or_404(&state, &app, &a).await?;
    let b = set_or_404(&state, &app, &b).await?;
    Ok(Json(diff_sets(&a, &b)))
}

/// A delta as stored, with its parsed content.
#[derive(Serialize)]
pub struct DeltaView {
    id: String,
    env: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    base_generation: Option<i64>,
    frozen: bool,
    created_by: Option<String>,
    #[serde(flatten)]
    content: Value,
}

impl From<DeltaRecord> for DeltaView {
    fn from(d: DeltaRecord) -> Self {
        Self {
            id: d.id,
            env: d.env,
            base_generation: d.base_generation,
            frozen: d.frozen,
            created_by: d.created_by,
            content: d.content,
        }
    }
}

pub(super) async fn delta_or_404(state: &AppState, app: &str, id: &str) -> ApiResult<DeltaRecord> {
    state
        .store
        .delta(state.org(), app, id)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("delta `{id}` not found in `{app}`")))
}

fn content(delta: &Delta) -> ApiResult<Value> {
    serde_json::to_value(delta).map_err(ApiError::internal)
}

pub async fn list_deltas(
    State(state): State<AppState>,
    Path((_, app)): Path<(String, String)>,
) -> ApiResult<Json<Vec<DeltaView>>> {
    let deltas = state.store.deltas(state.org(), &app).await?;
    Ok(Json(deltas.into_iter().map(DeltaView::from).collect()))
}

pub async fn get_delta(
    State(state): State<AppState>,
    Path((_, app, id)): Path<(String, String, String)>,
) -> ApiResult<Json<DeltaView>> {
    Ok(Json(delta_or_404(&state, &app, &id).await?.into()))
}

/// A request to draft a delta from a workload document.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkloadRequest {
    /// The environment whose desired set the delta changes.
    env: String,
    /// An openporch workload document or a Score file, as YAML or JSON text.
    workload: String,
    /// A `WorkloadOverlay` document for a Score file.
    #[serde(default)]
    overlay: Option<String>,
    /// Images, artifacts and properties supplied at deploy time.
    #[serde(default)]
    overrides: Overrides,
}

/// Creates a delta, either from a raw delta document or, when the body has a
/// `workload`, by converting a workload document: the delta brings that
/// workload to the document's content relative to the environment's desired
/// set, and deploying it requires the environment generation it was drafted
/// against. The server picks the ID.
pub async fn create_delta(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, app)): Path<(String, String)>,
    JsonBody(body): JsonBody<Value>,
) -> ApiResult<(StatusCode, Json<DeltaView>)> {
    let org = state.org();
    let id = auth::random_id(10);
    let audit = ctx.audit(org, "delta.create", format!("{app}/{id}"));
    if body.get("workload").is_some() {
        let req: WorkloadRequest =
            serde_json::from_value(body).map_err(|e| ApiError::invalid(e.to_string()))?;
        let (delta, generation) = convert(&state, &app, &req).await?;
        state
            .store
            .put_env_delta(
                org,
                &app,
                &id,
                (&req.env, generation),
                &content(&delta)?,
                audit,
            )
            .await?;
    } else {
        let delta: Delta =
            serde_json::from_value(body).map_err(|e| ApiError::invalid(e.to_string()))?;
        state
            .store
            .put_delta(org, &app, &id, &content(&delta)?, audit)
            .await?;
    }
    let created = delta_or_404(&state, &app, &id).await?;
    Ok((StatusCode::CREATED, Json(created.into())))
}

/// The delta a workload request makes against the environment's desired set,
/// and the generation that set belongs to.
async fn convert(state: &AppState, app: &str, req: &WorkloadRequest) -> ApiResult<(Delta, i64)> {
    let env = super::deploys::env_or_404(state, app, &req.env).await?;
    if env.state != "active" {
        return Err(ApiError::conflict(format!(
            "environment `{app}/{}` is {}",
            env.id, env.state
        )));
    }
    let invalid = |e: openporch_core::workload::WorkloadError| ApiError::invalid(e.to_string());
    let workload = Workload::parse(&req.workload, req.overlay.as_deref())
        .map_err(invalid)?
        .with_overrides(&req.overrides)
        .map_err(invalid)?;
    let mut types = builtin_types();
    types.extend(state.store.custom_types(state.org()).await?);
    let converted = workload.to_module(&types).map_err(invalid)?;
    let base = match &env.desired_set_id {
        Some(desired) => set_or_404(state, app, desired).await?,
        None => DeploymentSet::default(),
    };
    Ok((
        workload_delta(&base, &converted.id, &converted.module),
        env.generation,
    ))
}

/// Replaces a delta's content. A delta frozen by a deployment cannot change.
pub async fn replace_delta(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, app, id)): Path<(String, String, String)>,
    JsonBody(delta): JsonBody<Delta>,
) -> ApiResult<Json<DeltaView>> {
    let org = state.org();
    delta_or_404(&state, &app, &id).await?;
    state
        .store
        .put_delta(
            org,
            &app,
            &id,
            &content(&delta)?,
            ctx.audit(org, "delta.update", format!("{app}/{id}")),
        )
        .await?;
    Ok(Json(delta_or_404(&state, &app, &id).await?.into()))
}
