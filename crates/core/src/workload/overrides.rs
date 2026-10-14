//! Deploy-time overrides: images, artifacts and properties applied to a workload
//! after conversion and before its delta is created, and the check that nothing
//! left for deploy time (`.`) is still missing.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Package, Workload, WorkloadError};

/// The name `--image` and `--artifact` use for a workload's function.
pub const FUNCTION_NAME: &str = "main";

/// Values supplied at deploy time.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Overrides {
    /// Image references by container ID, or by `main` for an image function.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub images: IndexMap<String, String>,
    /// Artifact URIs by function name (`main`).
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub artifacts: IndexMap<String, String>,
    /// Values by dotted path into the workload document, applied in order. A
    /// value is read as YAML, except that it stays a string where it replaces
    /// a string, is not YAML, or the document needs a string.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub properties: IndexMap<String, String>,
}

impl Overrides {
    /// Whether nothing is overridden.
    pub fn is_empty(&self) -> bool {
        self.images.is_empty() && self.artifacts.is_empty() && self.properties.is_empty()
    }
}

impl Workload {
    /// Applies `overrides`, validates the result and checks that every value left
    /// for deploy time was supplied.
    pub fn with_overrides(mut self, overrides: &Overrides) -> Result<Self, WorkloadError> {
        for (name, image) in &overrides.images {
            let path = format!("images.{name}");
            if let Some(f) = self.function.as_mut().filter(|_| name == FUNCTION_NAME) {
                match &mut f.package {
                    Package::Image { image: current } => *current = image.clone(),
                    Package::Zip { .. } => {
                        return Err(WorkloadError::new(
                            path,
                            "function `main` is a zip package; supply an artifact instead",
                        ));
                    }
                }
            } else if let Some(c) = self.containers.as_mut().and_then(|c| c.get_mut(name)) {
                c.image = image.clone();
            } else {
                return Err(WorkloadError::new(
                    path,
                    format!("the workload has no container or function `{name}`"),
                ));
            }
        }
        for (name, artifact) in &overrides.artifacts {
            let path = format!("artifacts.{name}");
            let Some(f) = self.function.as_mut().filter(|_| name == FUNCTION_NAME) else {
                return Err(WorkloadError::new(
                    path,
                    format!(
                        "the workload has no function `{name}`; a function is named `{FUNCTION_NAME}`"
                    ),
                ));
            };
            match &mut f.package {
                Package::Zip {
                    artifact: current, ..
                } => *current = artifact.clone(),
                Package::Image { .. } => {
                    return Err(WorkloadError::new(
                        path,
                        "function `main` is an image package; supply an image instead",
                    ));
                }
            }
        }
        if !overrides.properties.is_empty() {
            let mut doc =
                serde_json::to_value(&self).map_err(|e| WorkloadError::new(".", e.to_string()))?;
            for (path, raw) in &overrides.properties {
                let before = doc.clone();
                let as_yaml = set_property(&mut doc, path, raw)?;
                // A value the document needs as a string, such as a
                // variable's `20` or `a: b`, stays the string it was written as.
                if as_yaml && serde_json::from_value::<Workload>(doc.clone()).is_err() {
                    let mut as_string = before;
                    set_value(&mut as_string, path, Value::String(raw.clone()))?;
                    if serde_json::from_value::<Workload>(as_string.clone()).is_ok() {
                        doc = as_string;
                    }
                }
            }
            self = serde_json::from_value(doc).map_err(|e| {
                WorkloadError::new("properties", format!("the result is not a workload: {e}"))
            })?;
        }
        self.validate()?;
        self.check_supplied()?;
        Ok(self)
    }

    /// Fails naming the first image or artifact that is still `.`.
    pub fn check_supplied(&self) -> Result<(), WorkloadError> {
        for (id, c) in self.containers.iter().flatten() {
            if c.image == "." {
                return Err(WorkloadError::new(
                    format!("containers.{id}.image"),
                    format!("supplied at deploy time: pass `--image {id}=<ref>`"),
                ));
            }
        }
        if let Some(f) = &self.function {
            match &f.package {
                Package::Zip { artifact, .. } if artifact == "." => {
                    return Err(WorkloadError::new(
                        "function.package.artifact",
                        format!("supplied at deploy time: pass `--artifact {FUNCTION_NAME}=<uri>`"),
                    ));
                }
                Package::Image { image } if image == "." => {
                    return Err(WorkloadError::new(
                        "function.package.image",
                        format!("supplied at deploy time: pass `--image {FUNCTION_NAME}=<ref>`"),
                    ));
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// Sets `path` to `raw` read as YAML, or as a string where it replaces a
/// string or is not YAML. Returns whether it was read as something other
/// than a string.
fn set_property(doc: &mut Value, path: &str, raw: &str) -> Result<bool, WorkloadError> {
    let slot = slot(doc, path)?;
    let value = if slot.is_string() {
        None
    } else {
        serde_yaml_ng::from_str::<Value>(raw).ok()
    };
    let as_yaml = value.as_ref().is_some_and(|v| !v.is_string());
    *slot = value.unwrap_or_else(|| Value::String(raw.to_owned()));
    Ok(as_yaml)
}

fn set_value(doc: &mut Value, path: &str, value: Value) -> Result<(), WorkloadError> {
    *slot(doc, path)? = value;
    Ok(())
}

/// The value at `path` (dot-separated keys, or indexes into lists) in `doc`,
/// creating missing maps on the way and `null` at the end.
fn slot<'a>(doc: &'a mut Value, path: &str) -> Result<&'a mut Value, WorkloadError> {
    let error = |message: String| WorkloadError::new(format!("properties.{path}"), message);
    let keys: Vec<&str> = path.split('.').collect();
    if keys.iter().any(|k| k.is_empty()) {
        return Err(error("a path is keys separated by single dots".into()));
    }
    let (last, parents) = keys.split_last().expect("split yields at least one key");
    let mut node = doc;
    for key in parents {
        node = match node {
            Value::Object(map) => map
                .entry(*key)
                .or_insert_with(|| Value::Object(Default::default())),
            Value::Array(items) => key
                .parse::<usize>()
                .ok()
                .and_then(|i| items.get_mut(i))
                .ok_or_else(|| error(format!("`{key}` is not an index of the list")))?,
            _ => return Err(error(format!("cannot descend into `{key}`"))),
        };
        if node.is_null() {
            *node = Value::Object(Default::default());
        }
    }
    match node {
        Value::Object(map) => Ok(map.entry(*last).or_insert(Value::Null)),
        Value::Array(items) => last
            .parse::<usize>()
            .ok()
            .and_then(|i| items.get_mut(i))
            .ok_or_else(|| error(format!("`{last}` is not an index of the list"))),
        _ => Err(error(format!("cannot set `{last}` here"))),
    }
}
