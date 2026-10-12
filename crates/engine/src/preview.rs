//! Previews of a deployment without running drivers: what each resource's
//! change is, as far as it is known before drivers plan, and which changes
//! need a destructive-action permit.
//!
//! A preview is not an infrastructure plan. An existing resource whose driver
//! previews is reported as an update whose effects its driver plans when the
//! deployment runs; a replacement found then needs a permit for that plan.
//! Moving a resource to another driver, cloud account or state backend is
//! refused: it is an explicit migration.

use std::collections::BTreeMap;

use openporch_core::canonical::to_canonical_string;
use openporch_store::ActiveResource;
use serde::Serialize;

use crate::planner::{Deployment, deletion_candidates};

/// What a preview knows about a driver.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DriverFacts {
    /// Whether the driver previews its changes.
    pub preview: bool,
    /// Where the driver keeps state now, as resources' footprints record it.
    pub backend: Option<String>,
}

/// A resource's change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    /// Provisioned for the first time.
    Create,
    /// Converged again; its driver plans the effects when the deployment runs.
    Update,
    /// Deleted, because the deployment leaves it out.
    Delete,
    /// Changed by a driver that cannot preview, so with unknown impact.
    Unknown,
    /// Replaced, as a driver's plan from an earlier attempt showed.
    Replace,
    /// No longer managed; the object stays.
    Detach,
}

impl Change {
    /// The destructive action a permit for this change names, if it needs one.
    pub fn permit_action(self) -> Option<&'static str> {
        match self {
            Self::Delete => Some("delete"),
            Self::Unknown => Some("unknown"),
            Self::Replace => Some("replace"),
            Self::Detach => Some("detach"),
            Self::Create | Self::Update => None,
        }
    }
}

/// A driver's plan from an earlier attempt that needed a permit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DriverPlan {
    /// The permit action it needs: `replace`, `delete`, or `unknown` when the
    /// driver could not tell, which has no fingerprint.
    pub action: String,
    /// The plan's fingerprint, which a permit for it names.
    pub fingerprint: String,
    /// The planned actions on named objects.
    pub actions: Vec<(String, String)>,
}

/// One resource in a preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Entry {
    /// `<type>.<class>#<res_id>`.
    pub descriptor: String,
    /// Its permanent identity, once it has one.
    pub guresid: Option<String>,
    /// The matched definition, or the last one for a deletion.
    pub definition: Option<String>,
    /// The driver.
    pub driver_type: Option<String>,
    /// The change.
    pub action: Change,
    /// Whether the change is known; an update's effects are known only once
    /// its driver plans.
    pub known: bool,
    /// Why.
    pub reason: String,
    /// Whether the change needs a destructive-action permit.
    pub permit_required: bool,
    /// The driver's plan from an earlier attempt, when one needed a permit.
    pub driver_plan: Option<DriverPlan>,
    /// A digest of the resource's configuration as previewed; a permit for
    /// the change binds only while the deployment's digest is the same.
    pub digest: String,
}

/// A digest of `value`'s canonical JSON.
fn digest(value: &serde_json::Value) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(to_canonical_string(value).as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Previews `deployment` against the environment's `active` resources.
/// `facts` describes each driver by ID; `plans` holds driver plans from
/// earlier attempts that needed a permit, by GUResID. Fails with every
/// migration the deployment would need.
pub fn preview(
    deployment: &Deployment,
    active: &[ActiveResource],
    facts: impl Fn(&str) -> Option<DriverFacts>,
    plans: &BTreeMap<String, DriverPlan>,
) -> Result<Vec<Entry>, Vec<String>> {
    let by_descriptor: BTreeMap<&str, &ActiveResource> =
        active.iter().map(|r| (r.descriptor.as_str(), r)).collect();
    let mut entries = Vec::new();
    let mut migrations = Vec::new();
    for node in deployment.graph.nodes.values() {
        let descriptor = node.descriptor.to_string();
        let current = by_descriptor.get(descriptor.as_str()).copied();
        let def = deployment.definitions.get(&node.definition).map(|(_, d)| d);
        let driver = facts(&node.driver_type);
        // What the resource is provisioned from: its node and the definition
        // version it matched.
        let config = digest(&serde_json::json!({
            "node": node,
            "definition": deployment.definitions.get(&node.definition),
        }));
        let entry = |action, known, reason: &str, permit_required, plan| Entry {
            descriptor: descriptor.clone(),
            guresid: current.map(|r| r.guresid.clone()),
            definition: Some(node.definition.to_string()),
            driver_type: Some(node.driver_type.clone()),
            action,
            known,
            reason: reason.to_owned(),
            permit_required,
            driver_plan: plan,
            digest: config.clone(),
        };
        let existing = current
            .filter(|r| r.last_deploy_id.is_some() || r.cookie.is_some() || r.target.is_some());
        // A driver plan an earlier attempt could not run without a permit:
        // the change needs a permit for that action, whatever else is true.
        if let Some(plan) = current.and_then(|r| plans.get(&r.guresid)) {
            let refused = match plan.action.as_str() {
                "delete" => Some((
                    Change::Delete,
                    "an earlier attempt's driver plan deletes objects",
                )),
                "replace" => Some((
                    Change::Replace,
                    "an earlier attempt's driver plan destroys or replaces objects",
                )),
                "unknown" => Some((
                    Change::Unknown,
                    "an earlier attempt's driver could not tell what the change does",
                )),
                _ => None,
            };
            if let Some((action, reason)) = refused {
                let fingerprinted = action != Change::Unknown;
                entries.push(entry(
                    action,
                    fingerprinted,
                    reason,
                    true,
                    fingerprinted.then(|| plan.clone()),
                ));
                continue;
            }
        }
        let Some(r) = existing else {
            // A failed first attempt may have left state the driver finds when
            // it plans, so only a resource with no record is known to be new.
            let (known, reason) = if current.is_some() && driver.as_ref().is_some_and(|f| f.preview)
            {
                (
                    false,
                    "an earlier attempt did not finish; the driver plans the change when the \
                     deployment runs",
                )
            } else {
                (true, "not provisioned yet")
            };
            entries.push(entry(Change::Create, known, reason, false, None));
            continue;
        };
        if let Some(from) = &r.driver_type
            && *from != node.driver_type
        {
            migrations.push(format!(
                "`{descriptor}` is managed by driver `{from}`, but its definition now uses `{}`; \
                 migrate or replace the resource explicitly",
                node.driver_type
            ));
            continue;
        }
        // An account given as a placeholder resolves only when the deployment
        // runs, which checks it then.
        let account = def.and_then(|d| d.driver_account.as_deref());
        let literal = def.is_some() && !account.is_some_and(|a| a.contains("${"));
        if literal && r.last_deploy_id.is_some() && r.driver_account.as_deref() != account {
            let name = |a: Option<&str>| match a {
                Some(a) => format!("cloud account `{a}`"),
                None => "no cloud account".to_owned(),
            };
            migrations.push(format!(
                "`{descriptor}` was provisioned in {}, but its definition now uses {}; migrate \
                 or replace the resource explicitly",
                name(r.driver_account.as_deref()),
                name(account)
            ));
            continue;
        }
        if let (Some(was), Some(now)) =
            (&r.backend, driver.as_ref().and_then(|f| f.backend.clone()))
            && *was != now
        {
            migrations.push(format!(
                "`{descriptor}` keeps its state in {was}, but its driver now uses {now}; migrate \
                 the state explicitly"
            ));
            continue;
        }
        if driver.is_some_and(|f| f.preview) {
            entries.push(entry(
                Change::Update,
                false,
                "the driver plans the change when the deployment runs; a replacement it finds \
                 needs a permit for that plan",
                false,
                None,
            ));
        } else {
            entries.push(entry(
                Change::Unknown,
                false,
                "the driver cannot preview changes",
                true,
                None,
            ));
        }
    }
    let (candidates, mandatory) =
        deletion_candidates(&deployment.graph, deployment.teardown, active);
    for (descriptor, r) in candidates {
        let reason = if r.cleanup_pending {
            "deleted by its driver already; only its record is removed"
        } else if deployment.teardown {
            "the environment is deleted"
        } else if descriptor.ty.as_str() == "workload" {
            "the workload was removed"
        } else if mandatory.contains(&r.descriptor) {
            "it belongs to a removed workload"
        } else {
            "scheduled for deletion; kept while something that stays uses it"
        };
        entries.push(Entry {
            descriptor: descriptor.to_string(),
            guresid: Some(r.guresid.clone()),
            definition: r.definition_id.clone(),
            driver_type: r.driver_type.clone(),
            action: Change::Delete,
            known: true,
            reason: reason.to_owned(),
            permit_required: !r.cleanup_pending,
            driver_plan: None,
            digest: digest(&serde_json::json!({
                "delete": r.guresid,
                "last_deploy_id": r.last_deploy_id,
                "definition": r.definition_id,
                "version": r.definition_version,
            })),
        });
    }
    entries.sort_by(|a, b| a.descriptor.cmp(&b.descriptor));
    if migrations.is_empty() {
        Ok(entries)
    } else {
        Err(migrations)
    }
}

/// The preview of detaching `r`: the orchestrator stops managing it and the
/// object stays.
pub fn detach(r: &ActiveResource) -> Entry {
    Entry {
        descriptor: r.descriptor.clone(),
        guresid: Some(r.guresid.clone()),
        definition: r.definition_id.clone(),
        driver_type: r.driver_type.clone(),
        action: Change::Detach,
        known: true,
        reason: "detached: no longer managed, and the object stays".to_owned(),
        permit_required: true,
        driver_plan: None,
        digest: digest(&serde_json::json!({
            "detach": r.guresid,
            "last_deploy_id": r.last_deploy_id,
            "definition": r.definition_id,
            "version": r.definition_version,
        })),
    }
}

#[cfg(test)]
mod tests;
