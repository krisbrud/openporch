//! Deployment sets, deltas, set IDs and set diffs.
//!
//! A set is the immutable, environment-agnostic description of what runs in an
//! environment. Its ID is the unpadded base64url SHA-256 of its JCS form, so equal
//! content always has an equal ID. A delta describes a change to a set.

use std::collections::BTreeMap;

use base64::Engine as _;
use json_patch::PatchOperation;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::canonical::to_canonical_string;
use crate::ids::{ClassId, ResourceTypeId, WorkloadId};

/// The algorithm used for set IDs, recorded with stored sets so it can change safely.
pub const SET_ID_ALGORITHM: &str = "jcs-sha256-base64url-v1";

/// A resource dependency of a workload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct External {
    /// Resource type.
    #[serde(rename = "type")]
    pub ty: ResourceTypeId,
    /// Resource class.
    #[serde(default)]
    pub class: ClassId,
    /// Shared resource ID. Without it the dependency is private to the workload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Parameters for the resource.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// A workload in a set.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Module {
    /// Workload class, used to match the `workload` resource definition.
    #[serde(default)]
    pub class: ClassId,
    /// The workload spec with set placeholders (`externals`, `shared`, `values`).
    #[serde(default)]
    pub spec: serde_json::Map<String, Value>,
    /// Resource dependencies by key.
    #[serde(default)]
    pub externals: BTreeMap<String, External>,
}

/// A shared resource declared at application level.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SharedResource {
    /// Resource type.
    #[serde(rename = "type")]
    pub ty: ResourceTypeId,
    /// Resource class.
    #[serde(default)]
    pub class: ClassId,
    /// Parameters for the resource.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// A deployment set.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentSet {
    /// Workloads by ID.
    #[serde(default)]
    pub modules: BTreeMap<WorkloadId, Module>,
    /// Shared resources by ID.
    #[serde(default)]
    pub shared: BTreeMap<String, SharedResource>,
}

impl DeploymentSet {
    /// The set's canonical JSON form.
    pub fn canonical_json(&self) -> String {
        to_canonical_string(&serde_json::to_value(self).expect("sets serialize to JSON"))
    }

    /// The set ID: unpadded base64url SHA-256 of the canonical JSON.
    pub fn id(&self) -> String {
        let digest = Sha256::digest(self.canonical_json().as_bytes());
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
    }
}

/// Changes to the workloads of a set.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModuleChanges {
    /// Workloads to add.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub add: BTreeMap<WorkloadId, Module>,
    /// Workloads to remove.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<WorkloadId>,
    /// JSON Patch operations per workload.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub update: BTreeMap<WorkloadId, Vec<PatchOperation>>,
}

/// The content of a delta.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Delta {
    /// Workload changes.
    #[serde(default)]
    pub modules: ModuleChanges,
    /// JSON Patch operations on the `shared` map.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shared: Vec<PatchOperation>,
}

impl Delta {
    /// Whether the delta changes nothing.
    pub fn is_empty(&self) -> bool {
        self.modules.add.is_empty()
            && self.modules.remove.is_empty()
            && self.modules.update.is_empty()
            && self.shared.is_empty()
    }
}

/// Why a delta could not be applied.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DeltaError {
    /// `modules.remove` names a workload that is not in the set.
    #[error("cannot remove workload `{0}`: it is not in the set")]
    RemoveMissing(WorkloadId),
    /// `modules.add` names a workload that is already in the set.
    #[error("cannot add workload `{0}`: it is already in the set")]
    AddExisting(WorkloadId),
    /// `modules.update` names a workload that is not in the set.
    #[error("cannot update workload `{0}`: it is not in the set")]
    UpdateMissing(WorkloadId),
    /// A patch failed or produced an invalid workload.
    #[error("cannot update workload `{workload}`: {reason}")]
    UpdateFailed {
        /// The workload.
        workload: WorkloadId,
        /// Why.
        reason: String,
    },
    /// The shared patch failed or produced invalid shared resources.
    #[error("cannot update shared resources: {0}")]
    SharedFailed(String),
}

/// Applies `delta` to `set`: removals, then additions, then updates, then shared changes.
pub fn apply_delta(set: &DeploymentSet, delta: &Delta) -> Result<DeploymentSet, DeltaError> {
    let mut out = set.clone();
    for id in &delta.modules.remove {
        if out.modules.remove(id).is_none() {
            return Err(DeltaError::RemoveMissing(id.clone()));
        }
    }
    for (id, module) in &delta.modules.add {
        if out.modules.contains_key(id) {
            return Err(DeltaError::AddExisting(id.clone()));
        }
        out.modules.insert(id.clone(), module.clone());
    }
    for (id, ops) in &delta.modules.update {
        let module = out
            .modules
            .get_mut(id)
            .ok_or_else(|| DeltaError::UpdateMissing(id.clone()))?;
        let failed = |reason: String| DeltaError::UpdateFailed {
            workload: id.clone(),
            reason,
        };
        let mut doc = serde_json::to_value(&*module).map_err(|e| failed(e.to_string()))?;
        json_patch::patch(&mut doc, ops).map_err(|e| failed(e.to_string()))?;
        *module = serde_json::from_value(doc).map_err(|e| failed(e.to_string()))?;
    }
    if !delta.shared.is_empty() {
        let failed = |e: &dyn std::fmt::Display| DeltaError::SharedFailed(e.to_string());
        let mut doc = serde_json::to_value(&out.shared).map_err(|e| failed(&e))?;
        json_patch::patch(&mut doc, &delta.shared).map_err(|e| failed(&e))?;
        out.shared = serde_json::from_value(doc).map_err(|e| failed(&e))?;
    }
    Ok(out)
}

/// The delta that transforms `a` into `b`.
pub fn diff_sets(a: &DeploymentSet, b: &DeploymentSet) -> Delta {
    let mut delta = Delta::default();
    for id in a.modules.keys() {
        if !b.modules.contains_key(id) {
            delta.modules.remove.push(id.clone());
        }
    }
    for (id, module) in &b.modules {
        match a.modules.get(id) {
            None => {
                delta.modules.add.insert(id.clone(), module.clone());
            }
            Some(old) if old != module => {
                let ops = json_patch::diff(
                    &serde_json::to_value(old).expect("modules serialize"),
                    &serde_json::to_value(module).expect("modules serialize"),
                );
                delta.modules.update.insert(id.clone(), ops.0);
            }
            Some(_) => {}
        }
    }
    if a.shared != b.shared {
        delta.shared = json_patch::diff(
            &serde_json::to_value(&a.shared).expect("shared serializes"),
            &serde_json::to_value(&b.shared).expect("shared serializes"),
        )
        .0;
    }
    delta
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;

    fn set(v: Value) -> DeploymentSet {
        serde_json::from_value(v).unwrap()
    }

    fn products() -> DeploymentSet {
        set(json!({
            "modules": {
                "products": {
                    "class": "default",
                    "spec": {"containers": {"web": {"image": "ghcr.io/acme/products:1", "variables": {"PAGE_SIZE": "10"}}}},
                    "externals": {"db": {"type": "postgres"}}
                }
            },
            "shared": {"api-dns": {"type": "dns"}}
        }))
    }

    #[test]
    fn fixture_set_id_is_pinned() {
        let s = products();
        assert_eq!(
            s.canonical_json(),
            r#"{"modules":{"products":{"class":"default","externals":{"db":{"class":"default","type":"postgres"}},"spec":{"containers":{"web":{"image":"ghcr.io/acme/products:1","variables":{"PAGE_SIZE":"10"}}}}}},"shared":{"api-dns":{"class":"default","type":"dns"}}}"#
        );
        assert_eq!(s.id(), "nJBrh6r0lOTMAo60-8jfjSkOsQWUbzW02bm8YX4kMfI");
        assert_eq!(
            DeploymentSet::default().id(),
            "eMoBSbfNQ27XowYz-VFrWop3EsmE-SpA3gSJvyHfwZo"
        );
    }

    #[test]
    fn identical_content_identical_id() {
        // Key order and an explicit default class do not change the ID.
        let a = products();
        let b = set(json!({
            "shared": {"api-dns": {"class": "default", "type": "dns"}},
            "modules": {"products": {"externals": {"db": {"type": "postgres", "class": "default"}},
                "spec": {"containers": {"web": {"variables": {"PAGE_SIZE": "10"}, "image": "ghcr.io/acme/products:1"}}}}}
        }));
        assert_eq!(a.id(), b.id());
    }

    #[test]
    fn patch_a_variable() {
        let delta: Delta = serde_json::from_value(json!({
            "modules": {"update": {"products": [
                {"op": "replace", "path": "/spec/containers/web/variables/PAGE_SIZE", "value": "20"}
            ]}}
        }))
        .unwrap();
        let out = apply_delta(&products(), &delta).unwrap();
        assert_eq!(
            out.modules[&"products".parse().unwrap()].spec["containers"]["web"]["variables"]["PAGE_SIZE"],
            "20"
        );
    }

    #[test]
    fn add_to_empty_environment() {
        let delta: Delta =
            serde_json::from_value(json!({"modules": {"add": {"web": {"spec": {}}}}})).unwrap();
        let out = apply_delta(&DeploymentSet::default(), &delta).unwrap();
        assert_eq!(
            out.modules.keys().map(|k| k.as_str()).collect::<Vec<_>>(),
            ["web"]
        );
    }

    #[test]
    fn errors_name_the_workload() {
        let s = products();
        let d = |v: Value| serde_json::from_value::<Delta>(v).unwrap();
        let e = apply_delta(&s, &d(json!({"modules": {"remove": ["web"]}}))).unwrap_err();
        assert_eq!(
            e.to_string(),
            "cannot remove workload `web`: it is not in the set"
        );
        let e = apply_delta(&s, &d(json!({"modules": {"add": {"products": {}}}}))).unwrap_err();
        assert!(matches!(e, DeltaError::AddExisting(w) if w.as_str() == "products"));
        let e = apply_delta(&s, &d(json!({"modules": {"update": {"web": []}}}))).unwrap_err();
        assert!(matches!(e, DeltaError::UpdateMissing(w) if w.as_str() == "web"));
        let e = apply_delta(
            &s,
            &d(json!({"modules": {"update": {"products": [{"op": "remove", "path": "/nope"}]}}})),
        )
        .unwrap_err();
        assert!(e.to_string().contains("products"));
    }

    #[test]
    fn removal_happens_before_addition_and_update_after() {
        // Re-adding a removed workload in one delta works, and the update applies to the new one.
        let delta: Delta = serde_json::from_value(json!({
            "modules": {
                "remove": ["products"],
                "add": {"products": {"spec": {"a": 1}}},
                "update": {"products": [{"op": "add", "path": "/spec/b", "value": 2}]}
            },
            "shared": [{"op": "remove", "path": "/api-dns"}]
        }))
        .unwrap();
        let out = apply_delta(&products(), &delta).unwrap();
        let m = &out.modules[&"products".parse().unwrap()];
        assert_eq!(Value::Object(m.spec.clone()), json!({"a": 1, "b": 2}));
        assert!(out.shared.is_empty());
    }

    #[test]
    fn diff_of_equal_sets_is_empty() {
        assert!(diff_sets(&products(), &products()).is_empty());
    }

    fn json_leaf() -> impl Strategy<Value = Value> {
        prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::Bool),
            (-1000i64..1000).prop_map(|i| json!(i)),
            "[a-z/~ ]{0,6}".prop_map(Value::String),
        ]
    }

    fn json_value() -> impl Strategy<Value = Value> {
        json_leaf().prop_recursive(3, 24, 4, |inner| {
            prop_oneof![
                proptest::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
                proptest::collection::btree_map("[a-z~/]{1,4}", inner, 0..4)
                    .prop_map(|m| Value::Object(m.into_iter().collect())),
            ]
        })
    }

    fn module() -> impl Strategy<Value = Module> {
        (
            prop_oneof![Just("default"), Just("large")],
            proptest::collection::btree_map("[a-z]{1,4}", json_value(), 0..3),
            proptest::collection::btree_map(
                "[a-z]{1,4}",
                (
                    prop_oneof![Just("postgres"), Just("s3")],
                    proptest::option::of("[a-z]{2,4}"),
                ),
                0..3,
            ),
        )
            .prop_map(|(class, spec, externals)| Module {
                class: class.parse().unwrap(),
                spec: spec.into_iter().collect(),
                externals: externals
                    .into_iter()
                    .map(|(k, (ty, id))| {
                        (
                            k,
                            External {
                                ty: ty.parse().unwrap(),
                                class: ClassId::default(),
                                id,
                                params: None,
                            },
                        )
                    })
                    .collect(),
            })
    }

    fn deployment_set() -> impl Strategy<Value = DeploymentSet> {
        (
            proptest::collection::btree_map(
                prop_oneof![Just("web"), Just("api"), Just("worker")],
                module(),
                0..3,
            ),
            proptest::collection::btree_map(
                prop_oneof![Just("dns"), Just("bucket")],
                // `params: null` reads back as absent, so generate only non-null params.
                proptest::option::of(json_value().prop_filter("non-null", |v| !v.is_null())),
                0..2,
            ),
        )
            .prop_map(|(modules, shared)| DeploymentSet {
                modules: modules
                    .into_iter()
                    .map(|(k, v)| (k.parse().unwrap(), v))
                    .collect(),
                shared: shared
                    .into_iter()
                    .map(|(k, params)| {
                        (
                            k.to_owned(),
                            SharedResource {
                                ty: "dns".parse().unwrap(),
                                class: ClassId::default(),
                                params,
                            },
                        )
                    })
                    .collect(),
            })
    }

    proptest! {
        #[test]
        fn apply_diff_round_trips(a in deployment_set(), b in deployment_set()) {
            let delta = diff_sets(&a, &b);
            let applied = apply_delta(&a, &delta).unwrap();
            prop_assert_eq!(&applied, &b);
            prop_assert_eq!(applied.id(), b.id());
            // The delta survives a JSON round trip, as it does through the API.
            let delta2: Delta = serde_json::from_str(&serde_json::to_string(&delta).unwrap()).unwrap();
            prop_assert_eq!(apply_delta(&a, &delta2).unwrap(), b);
        }

        #[test]
        fn set_id_depends_only_on_content(a in deployment_set()) {
            let reparsed: DeploymentSet = serde_json::from_str(&serde_json::to_string(&a).unwrap()).unwrap();
            prop_assert_eq!(a.id(), reparsed.id());
        }
    }
}
