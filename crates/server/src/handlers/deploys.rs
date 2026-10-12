//! Deployments: triggering, history and reading one attempt.
//!
//! Acceptance resolves the desired set and builds its resource graph, so a set
//! that cannot be deployed is rejected before anything is recorded. The graph,
//! the definitions it matched and the request's permits form the execution
//! snapshot that the deployment runs from, whatever changes afterwards.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use openporch_core::canonical::to_canonical_string;
use openporch_core::catalog::{ClassRegistry, builtin_types};
use openporch_core::graph::{GraphInput, ResourceGraph, build_graph};
use openporch_core::sets::{Delta, DeploymentSet, apply_delta};
use openporch_store::{
    Acceptance, CatalogStore, DeploymentKind, DeploymentRecord, DeploymentRequest,
    DeploymentStatus, EnvRecord, LogLine, Store, StoreError,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use super::sets::{delta_or_404, set_or_404};
use crate::api::{Ctx, JsonBody, QueryParams};
use crate::deploy::Job;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// The snapshot format this server writes and replays.
pub(super) const SNAPSHOT_VERSION: u64 = 2;

const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 500;

/// How a deployment visits the graph.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Every resource in the desired graph.
    #[default]
    Full,
    /// Only changed resources; not supported in milestone 1.
    Incremental,
}

/// Which configuration a redeploy uses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Replay {
    /// The configuration recorded by the earlier deployment.
    #[default]
    Snapshot,
    /// The definitions and values as they are now.
    Current,
}

/// `POST .../deploys`. Exactly one of `delta_id`, `set_id` and `deploy_id`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriggerRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    delta_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    set_id: Option<String>,
    /// A deployment ID, `+n` for the nth earlier attempt (`+0` is the latest),
    /// or `last-successful`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    deploy_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    replay: Option<Replay>,
    #[serde(default)]
    mode: Mode,
    #[serde(default)]
    comment: String,
    /// Destructive-action permits for this deployment.
    #[serde(default)]
    permits: Vec<String>,
    /// The environment generation the client read.
    expected_generation: i64,
}

/// The set a request deploys, with the snapshot to run it from.
struct Resolved {
    set: DeploymentSet,
    delta_id: Option<String>,
    delta_content: Option<Value>,
    snapshot: Value,
}

/// `POST /orgs/{org}/apps/{app}/envs/{env}/deploys`.
pub async fn trigger(
    State(state): State<AppState>,
    ctx: Ctx,
    Path((_, app, env)): Path<(String, String, String)>,
    headers: HeaderMap,
    JsonBody(mut req): JsonBody<TriggerRequest>,
) -> ApiResult<(StatusCode, Json<DeploymentRecord>)> {
    let org = state.org();
    // Permits are a set; their order does not make a different request.
    req.permits.sort();
    req.permits.dedup();
    if req.mode == Mode::Incremental {
        return Err(ApiError::invalid(
            "mode `incremental` is not supported; use `full`",
        ));
    }
    let sources = [&req.delta_id, &req.set_id, &req.deploy_id]
        .iter()
        .filter(|s| s.is_some())
        .count();
    if sources != 1 {
        return Err(ApiError::invalid(
            "give exactly one of `delta_id`, `set_id` and `deploy_id`",
        ));
    }
    if req.replay.is_some() && req.deploy_id.is_none() {
        return Err(ApiError::invalid("`replay` applies only to `deploy_id`"));
    }
    let key = idempotency_key(&headers)?;
    let fingerprint = fingerprint(&serde_json::to_value(&req).map_err(ApiError::internal)?);
    // A retry returns the original deployment without deriving anything again:
    // the desired set it would apply a delta to has moved on.
    if let Some(key) = &key
        && let Some((prior, prior_fingerprint)) = state
            .store
            .idempotent_deployment(org, &app, &env, &ctx.principal.id, key)
            .await?
    {
        if prior_fingerprint != fingerprint {
            return Err(ApiError::conflict(format!(
                "idempotency key `{key}` was used for a different request"
            )));
        }
        return Ok((StatusCode::ACCEPTED, Json(prior)));
    }

    let env_record = env_or_404(&state, &app, &env).await?;
    // Conflicts are reported before the request is resolved, so a delta that no
    // longer applies to the moved-on desired set still reads as stale. Acceptance
    // checks again atomically.
    if env_record.state != "active" {
        return Err(ApiError::conflict(format!(
            "environment `{app}/{env}` is {}",
            env_record.state
        )));
    }
    if env_record.generation != req.expected_generation {
        return Err(StoreError::StaleGeneration {
            expected: req.expected_generation,
            actual: env_record.generation,
        }
        .into());
    }
    if let Some(running) = state.store.latest_deployment(org, &app, &env).await?
        && running.status == DeploymentStatus::InProgress
    {
        return Err(StoreError::Busy(running.id).into());
    }
    let resolved = resolve(&state, &env_record, &req).await?;
    let set_id = state.store.put_set(org, &app, &resolved.set).await?;
    let acceptance = state
        .store
        .accept_deployment(DeploymentRequest {
            org: org.to_owned(),
            app: app.clone(),
            env: env.clone(),
            actor: ctx.principal.id.clone(),
            expected_generation: req.expected_generation,
            idempotency_key: key,
            fingerprint,
            kind: DeploymentKind::Deploy,
            set_id: Some(set_id),
            delta_id: resolved.delta_id,
            delta_content: resolved.delta_content,
            comment: req.comment,
            snapshot: resolved.snapshot.clone(),
            audit: ctx.audit(org, "deployment.create", format!("{app}/{env}")),
        })
        .await?;
    if let Acceptance::Created(d) = &acceptance {
        state.runners.spawn(
            state.clone(),
            Job {
                app,
                env,
                id: d.id.clone(),
                snapshot: resolved.snapshot,
            },
        );
    }
    Ok((StatusCode::ACCEPTED, Json(acceptance.deployment().clone())))
}

/// A digest of a request body, compared on retries with the same idempotency
/// key.
pub(super) fn fingerprint(body: &Value) -> String {
    Sha256::digest(to_canonical_string(body).as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The `Idempotency-Key` header, if sent.
pub(super) fn idempotency_key(headers: &HeaderMap) -> ApiResult<Option<String>> {
    let Some(value) = headers.get("idempotency-key") else {
        return Ok(None);
    };
    let key = value
        .to_str()
        .ok()
        .filter(|k| !k.is_empty() && k.len() <= 255 && k.bytes().all(|b| b.is_ascii_graphic()))
        .ok_or_else(|| {
            ApiError::invalid("`Idempotency-Key` must be 1 to 255 visible ASCII characters")
        })?;
    Ok(Some(key.to_owned()))
}

async fn env_or_404(state: &AppState, app: &str, env: &str) -> ApiResult<EnvRecord> {
    state
        .store
        .env(state.org(), app, env)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("environment `{app}/{env}` not found")))
}

/// Resolves the request into the set to deploy and its snapshot.
async fn resolve(state: &AppState, env: &EnvRecord, req: &TriggerRequest) -> ApiResult<Resolved> {
    let app = &env.app;
    if let Some(id) = &req.delta_id {
        let record = delta_or_404(state, app, id).await?;
        if let Some(for_env) = &record.env
            && for_env != &env.id
        {
            return Err(ApiError::invalid(format!(
                "delta `{id}` was drafted for `{for_env}`, not `{}`",
                env.id
            )));
        }
        let delta: Delta = serde_json::from_value(record.content.clone())
            .map_err(|e| ApiError::internal(format!("stored delta `{id}`: {e}")))?;
        // Deltas apply to the desired set, which starts empty.
        let base = match &env.desired_set_id {
            Some(desired) => set_or_404(state, app, desired).await?,
            None => DeploymentSet::default(),
        };
        let set = apply_delta(&base, &delta)
            .map_err(|e| ApiError::invalid(format!("delta `{id}`: {e}")))?;
        let snapshot = snapshot(state, env, &set, req).await?;
        return Ok(Resolved {
            set,
            delta_id: Some(id.clone()),
            delta_content: Some(record.content),
            snapshot,
        });
    }
    if let Some(id) = &req.set_id {
        let set = set_or_404(state, app, id).await?;
        let snapshot = snapshot(state, env, &set, req).await?;
        return Ok(Resolved {
            set,
            delta_id: None,
            delta_content: None,
            snapshot,
        });
    }
    let reference = req.deploy_id.as_deref().unwrap_or_default();
    let source = deployment_by_ref(state, env, reference).await?;
    let Some(source_set) = &source.set_id else {
        return Err(ApiError::invalid(format!(
            "deployment `{}` has no set to deploy",
            source.id
        )));
    };
    let set = set_or_404(state, app, source_set).await?;
    let snapshot = match req.replay.unwrap_or_default() {
        Replay::Current => snapshot(state, env, &set, req).await?,
        Replay::Snapshot => {
            if source.env != env.id {
                return Err(ApiError::invalid(format!(
                    "deployment `{}` ran in `{}`; its snapshot cannot be replayed in `{}`, \
                     use `\"replay\": \"current\"`",
                    source.id, source.env, env.id
                )));
            }
            let recorded = state
                .store
                .deployment_snapshot(state.org(), app, &source.env, &source.id)
                .await?
                .filter(|s| s["version"] == SNAPSHOT_VERSION && s["graph"].is_object())
                .ok_or_else(|| {
                    ApiError::invalid(format!(
                        "deployment `{}` has no replayable snapshot; use \
                         `\"replay\": \"current\"`",
                        source.id
                    ))
                })?;
            // Replay uses the recorded content, but only of definitions that
            // still exist.
            let current = state.store.definitions(state.org()).await?;
            if let Some(gone) = recorded["definitions"]
                .as_object()
                .into_iter()
                .flat_map(|d| d.keys())
                .find(|id| {
                    !current
                        .iter()
                        .any(|r| r.definition.id.as_str() == id.as_str())
                })
            {
                return Err(ApiError::invalid(format!(
                    "deployment `{}` used definition `{gone}`, which no longer exists",
                    source.id
                )));
            }
            let mut snapshot = recorded;
            snapshot["permits"] = json!(req.permits);
            snapshot["replayed_from"] = json!(source.id);
            snapshot
        }
    };
    Ok(Resolved {
        set,
        delta_id: None,
        delta_content: None,
        snapshot,
    })
}

/// Builds the resource graph of `set` with the current catalog and records it,
/// with the definitions it matched, as the execution snapshot.
async fn snapshot(
    state: &AppState,
    env: &EnvRecord,
    set: &DeploymentSet,
    req: &TriggerRequest,
) -> ApiResult<Value> {
    let org = state.org();
    let records = state.store.definitions(org).await?;
    let definitions: Vec<_> = records.iter().map(|r| r.definition.clone()).collect();
    let mut types = builtin_types();
    types.extend(state.store.custom_types(org).await?);
    let classes = ClassRegistry::new(state.store.classes(org, None).await?);
    let id = |kind: &str, v: &str| ApiError::internal(format!("stored {kind} ID `{v}` is invalid"));
    let graph: ResourceGraph = build_graph(GraphInput {
        org: &org.parse().map_err(|_| id("organization", org))?,
        app: &env.app.parse().map_err(|_| id("application", &env.app))?,
        env: &env.id.parse().map_err(|_| id("environment", &env.id))?,
        env_type: &env
            .env_type
            .parse()
            .map_err(|_| id("environment type", &env.env_type))?,
        set,
        definitions: &definitions,
        types: &types,
        classes: &classes,
    })
    .map_err(|e| ApiError::invalid(format!("the set cannot be deployed: {e}")))?;
    let used: BTreeMap<String, Value> = records
        .iter()
        .filter(|r| {
            graph
                .nodes
                .values()
                .any(|n| n.definition == r.definition.id)
        })
        .map(|r| {
            (
                r.definition.id.to_string(),
                json!({"version": r.version, "definition": r.definition}),
            )
        })
        .collect();
    Ok(json!({
        "version": SNAPSHOT_VERSION,
        "mode": req.mode,
        "permits": req.permits,
        "graph": graph,
        "definitions": used,
    }))
}

/// A deployment by ID (in any environment of the application), `+n` (the nth
/// attempt before the latest in this environment) or `last-successful`.
async fn deployment_by_ref(
    state: &AppState,
    env: &EnvRecord,
    reference: &str,
) -> ApiResult<DeploymentRecord> {
    let org = state.org();
    let (app, env_id) = (&env.app, &env.id);
    if reference == "last-successful" {
        let id = env.last_successful_deploy_id.as_deref().ok_or_else(|| {
            ApiError::not_found(format!("`{app}/{env_id}` has no successful deployment"))
        })?;
        return state
            .store
            .deployment(org, app, env_id, id)
            .await?
            .ok_or_else(|| ApiError::not_found(format!("deployment `{id}` not found")));
    }
    if let Some(n) = reference.strip_prefix('+') {
        let n: i64 = n
            .parse()
            .ok()
            .filter(|n| (0..MAX_LIMIT).contains(n))
            .ok_or_else(|| {
                ApiError::invalid(format!(
                    "`{reference}` must be `+n` with n from 0 to {}",
                    MAX_LIMIT - 1
                ))
            })?;
        return state
            .store
            .deployments(org, app, env_id, n + 1, None)
            .await?
            .into_iter()
            .nth(n as usize)
            .ok_or_else(|| {
                ApiError::not_found(format!("`{app}/{env_id}` has no deployment `{reference}`"))
            });
    }
    state
        .store
        .app_deployment(org, app, reference)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("deployment `{reference}` not found")))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListQuery {
    before: Option<i64>,
    limit: Option<i64>,
}

/// A page of deployments, newest first. Pass `next` as `before` for the next page.
#[derive(Serialize)]
pub struct Page {
    deployments: Vec<DeploymentRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next: Option<i64>,
}

/// `GET .../deploys`.
pub async fn list(
    State(state): State<AppState>,
    Path((_, app, env)): Path<(String, String, String)>,
    QueryParams(q): QueryParams<ListQuery>,
) -> ApiResult<Json<Page>> {
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(ApiError::invalid(format!(
            "`limit` must be between 1 and {MAX_LIMIT}"
        )));
    }
    env_or_404(&state, &app, &env).await?;
    let deployments = state
        .store
        .deployments(state.org(), &app, &env, limit, q.before)
        .await?;
    let next = (deployments.len() == limit as usize)
        .then(|| deployments.last().map(|d| d.seq))
        .flatten();
    Ok(Json(Page { deployments, next }))
}

/// `GET .../deploys/{deploy}`, by ID, `+n` or `last-successful`.
pub async fn get(
    State(state): State<AppState>,
    Path((_, app, env, reference)): Path<(String, String, String, String)>,
) -> ApiResult<Json<DeploymentRecord>> {
    let env = env_or_404(&state, &app, &env).await?;
    let d = deployment_by_ref(&state, &env, &reference).await?;
    if d.env != env.id {
        return Err(ApiError::not_found(format!(
            "deployment `{reference}` not found in `{}/{}`",
            env.app, env.id
        )));
    }
    Ok(Json(d))
}

/// Log lines returned by default, and at most.
const DEFAULT_LOG_LIMIT: i64 = 500;
const MAX_LOG_LIMIT: i64 = 5000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogQuery {
    after: Option<i64>,
    limit: Option<i64>,
}

/// A page of a deployment's log, oldest first. Pass `next` as `after` for
/// the lines that follow.
#[derive(Serialize)]
pub struct LogPage {
    lines: Vec<LogLine>,
    next: i64,
    /// The deployment has finished and there are no more lines.
    finished: bool,
}

/// `GET .../deploys/{deploy}/logs`. Lines are redacted before they are
/// written, so they are returned as stored.
pub async fn logs(
    State(state): State<AppState>,
    Path((_, app, env, reference)): Path<(String, String, String, String)>,
    QueryParams(q): QueryParams<LogQuery>,
) -> ApiResult<Json<LogPage>> {
    let limit = q.limit.unwrap_or(DEFAULT_LOG_LIMIT);
    if !(1..=MAX_LOG_LIMIT).contains(&limit) {
        return Err(ApiError::invalid(format!(
            "`limit` must be between 1 and {MAX_LOG_LIMIT}"
        )));
    }
    let after = q.after.unwrap_or(0);
    if after < 0 {
        return Err(ApiError::invalid("`after` must not be negative"));
    }
    let env = env_or_404(&state, &app, &env).await?;
    let d = deployment_by_ref(&state, &env, &reference).await?;
    if d.env != env.id {
        return Err(ApiError::not_found(format!(
            "deployment `{reference}` not found in `{}/{}`",
            env.app, env.id
        )));
    }
    // The log is complete before the deployment finishes, so a deployment
    // read as finished before its lines has no lines still to come.
    let done = d.status != DeploymentStatus::InProgress;
    // One more line than asked for tells whether any follow.
    let mut lines = state
        .store
        .deployment_logs(
            state.org(),
            &env.app,
            &env.id,
            &d.id,
            Some(after),
            limit + 1,
        )
        .await?;
    let more = lines.len() > limit as usize;
    lines.truncate(limit as usize);
    let next = lines.last().map_or(after, |l| l.seq);
    let finished = done && !more;
    Ok(Json(LogPage {
        lines,
        next,
        finished,
    }))
}
