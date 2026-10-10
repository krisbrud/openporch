//! Turning secret inputs into references before they are stored.
//!
//! Definitions and driver registrations accept secrets as references
//! (`{store, ref, version?}`), as values to write (`{value}`), or, for driver
//! inputs, inline under `secrets`. Values are written to the primary store and
//! replaced by references pinning the written version; only placeholder templates
//! stay inline. Every input is checked before anything is written, and error
//! messages name paths, never values.

use openporch_core::model::{DriverInputs, SecretRef};
use openporch_core::placeholder::{Part, Template};
use openporch_store::secrets::{
    BuiltinSecretStore, SecretError, SecretInput, SecretStore, SecretValue, validate_reference,
};
use serde_json::{Map, Value};

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// The primary secret store. External primary stores are task 13.2.
pub fn primary(state: &AppState) -> ApiResult<BuiltinSecretStore> {
    let keyring = state
        .keyring
        .clone()
        .ok_or_else(|| ApiError::not_ready("the master key is not available"))?;
    Ok(BuiltinSecretStore::new(&state.store, keyring, state.org()))
}

fn store_error(e: SecretError) -> ApiError {
    match e {
        SecretError::InvalidReference { .. } => ApiError::invalid(e.to_string()),
        other => ApiError::internal(other),
    }
}

/// Replaces every double-quoted string in a deserialization error with
/// `"[REDACTED]"`. Serde quotes input values that way (field names use backticks),
/// so a body carrying secrets is never echoed back.
fn redact_quoted(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    let mut chars = message.chars();
    while let Some(c) = chars.next() {
        if c != '"' {
            out.push(c);
            continue;
        }
        out.push_str("\"[REDACTED]\"");
        while let Some(c) = chars.next() {
            match c {
                '\\' => {
                    chars.next();
                }
                '"' => break,
                _ => {}
            }
        }
    }
    out
}

/// Parses a request body that may carry secrets, with values redacted from errors.
pub fn parse_body<T: serde::de::DeserializeOwned>(body: Value) -> ApiResult<T> {
    serde_json::from_value(body).map_err(|e| {
        ApiError::invalid(format!(
            "invalid request body: {}",
            redact_quoted(&e.to_string())
        ))
    })
}

/// Deletes versions written for a change that was then rejected, so no secret is
/// left behind that no accepted object refers to.
pub async fn discard(primary: &BuiltinSecretStore, written: Vec<SecretRef>) {
    for r in written {
        if let Err(e) = primary.discard(&r).await {
            tracing::warn!(store = %r.store, reference = %r.reference, error = %e,
                "could not discard a secret version written for a rejected change");
        }
    }
}

/// What to put at one path of `secret_refs`.
enum Planned {
    Ref(SecretRef),
    Write(SecretValue),
}

/// A secret input that has been checked but not written yet.
pub struct PlannedSecret(Planned);

impl PlannedSecret {
    /// Checks one secret input. `what` names it in errors.
    pub fn new(what: &str, input: &Value) -> ApiResult<Self> {
        let parsed: SecretInput = serde_json::from_value(input.clone()).map_err(|_| {
            ApiError::invalid(format!(
                "{what} must be a secret reference `{{store, ref, version?}}` or `{{value}}`"
            ))
        })?;
        Ok(Self(match parsed {
            SecretInput::Ref(r) => {
                validate_reference(&r).map_err(store_error)?;
                Planned::Ref(r)
            }
            SecretInput::Value(v) => {
                if is_template(v.value.expose()) {
                    return Err(ApiError::invalid(format!(
                        "{what} is a `{{value}}` with a placeholder; put placeholder templates \
                         under `driver_inputs.secrets`"
                    )));
                }
                Planned::Write(v.value)
            }
        }))
    }

    /// The reference, if this input is one; values have none until written.
    pub fn reference(&self) -> Option<&SecretRef> {
        match &self.0 {
            Planned::Ref(r) => Some(r),
            Planned::Write(_) => None,
        }
    }

    /// Writes a value under `name`, recording the new version in `written`, or
    /// passes a reference through.
    pub async fn materialize(
        self,
        primary: &dyn SecretStore,
        name: &str,
        written: &mut Vec<SecretRef>,
    ) -> ApiResult<SecretRef> {
        match self.0 {
            Planned::Ref(r) => Ok(r),
            Planned::Write(v) => {
                let r = primary.write(name, &v).await.map_err(store_error)?;
                written.push(r.clone());
                Ok(r)
            }
        }
    }
}

/// Whether an object in `secret_refs` is a single secret input.
///
/// A leaf has exactly the shape of a reference (`store` and `ref`, optionally
/// `version`) or of `{value}` with a string, so containers may have children
/// named `store`, `ref`, `version` or `value` like `values` can.
fn is_secret_leaf(m: &Map<String, Value>) -> bool {
    let is_ref = m.len() <= 3
        && m.contains_key("store")
        && m.contains_key("ref")
        && m.keys()
            .all(|k| ["store", "ref", "version"].contains(&k.as_str()))
        && m.values().all(|v| !v.is_object() && !v.is_array());
    let is_value = m.len() == 1 && m.get("value").is_some_and(Value::is_string);
    is_ref || is_value
}

/// The text a literal inline secret stands for: escapes such as `$\{` are applied
/// when it parses as a template without placeholders; anything that does not
/// parse is taken as it is.
fn literal_text(s: &str) -> String {
    match Template::parse(s) {
        Ok(t) if t.is_literal() => t
            .parts
            .into_iter()
            .map(|p| match p {
                Part::Literal(l) => l,
                Part::Placeholder(raw) => raw.text,
            })
            .collect(),
        _ => s.to_owned(),
    }
}

/// Whether `s` is a placeholder template rather than a literal that merely
/// contains `${`. Anything that does not parse as a template is a literal.
fn is_template(s: &str) -> bool {
    Template::parse(s).is_ok_and(|t| !t.is_literal())
}

fn display(path: &[String]) -> String {
    path.join(".")
}

fn collect_refs(
    map: &Map<String, Value>,
    path: &mut Vec<String>,
    out: &mut Vec<(Vec<String>, PlannedSecret)>,
) -> ApiResult<()> {
    for (k, v) in map {
        path.push(k.clone());
        let what = format!("driver_inputs.secret_refs.{}", display(path));
        match v {
            Value::Object(m) if is_secret_leaf(m) => {
                out.push((path.clone(), PlannedSecret::new(&what, v)?));
            }
            Value::Object(m) => collect_refs(m, path, out)?,
            _ => {
                return Err(ApiError::invalid(format!(
                    "{what} must be an object of secret references"
                )));
            }
        }
        path.pop();
    }
    Ok(())
}

/// Splits inline secrets into placeholder templates, which stay inline, and
/// literal values, which become references.
fn collect_inline(
    map: &Map<String, Value>,
    path: &mut Vec<String>,
    templates: &mut Vec<(Vec<String>, Value)>,
    out: &mut Vec<(Vec<String>, PlannedSecret)>,
) -> ApiResult<()> {
    for (k, v) in map {
        path.push(k.clone());
        match v {
            Value::String(s) if is_template(s) => {
                // A template mixing literal text with placeholders would keep that
                // text readable; only whole placeholders stay inline.
                if Template::parse(s)
                    .ok()
                    .and_then(|t| t.whole().cloned())
                    .is_none()
                {
                    return Err(ApiError::invalid(format!(
                        "driver_inputs.secrets.{} mixes literal text with placeholders; a \
                         secret template must be a single placeholder, with literal parts in \
                         their own secret input",
                        display(path)
                    )));
                }
                templates.push((path.clone(), v.clone()));
            }
            Value::String(s) => out.push((
                path.clone(),
                PlannedSecret(Planned::Write(SecretValue::new(literal_text(s)))),
            )),
            Value::Object(m) => collect_inline(m, path, templates, out)?,
            _ => {
                return Err(ApiError::invalid(format!(
                    "driver_inputs.secrets.{} must be a string or an object of strings",
                    display(path)
                )));
            }
        }
        path.pop();
    }
    Ok(())
}

/// Inserts `value` at `path`, refusing to overwrite or nest inside another leaf.
fn insert(root: &mut Map<String, Value>, path: &[String], value: Value) -> Result<(), ()> {
    let Some((last, parents)) = path.split_last() else {
        return Err(());
    };
    let mut node = root;
    for p in parents {
        let entry = node
            .entry(p.clone())
            .or_insert_with(|| Value::Object(Map::new()));
        match entry {
            // An existing leaf here means one secret would nest inside another.
            Value::Object(m) if !is_secret_leaf(m) => node = m,
            _ => return Err(()),
        }
    }
    if node.contains_key(last) {
        return Err(());
    }
    node.insert(last.clone(), value);
    Ok(())
}

/// Driver inputs whose secrets have been checked and await writing.
pub struct PlannedInputs {
    values: Map<String, Value>,
    templates: Vec<(Vec<String>, Value)>,
    secrets: Vec<(Vec<String>, PlannedSecret)>,
}

impl PlannedInputs {
    /// Checks driver inputs. A path set both inline and under `secret_refs`, or
    /// nested inside another secret, is rejected.
    pub fn new(inputs: DriverInputs) -> ApiResult<Self> {
        let mut secrets = Vec::new();
        collect_refs(&inputs.secret_refs, &mut Vec::new(), &mut secrets)?;
        let mut templates = Vec::new();
        collect_inline(
            &inputs.secrets,
            &mut Vec::new(),
            &mut templates,
            &mut secrets,
        )?;
        // Dry run of the final layout: every path must be its own leaf.
        let mut layout = Map::new();
        let marker = || {
            Value::Object(Map::from_iter([
                ("store".to_owned(), Value::Null),
                ("ref".to_owned(), Value::Null),
            ]))
        };
        for path in templates
            .iter()
            .map(|t| &t.0)
            .chain(secrets.iter().map(|s| &s.0))
        {
            insert(&mut layout, path, marker()).map_err(|()| {
                ApiError::invalid(format!(
                    "secret input `{}` is given more than once or overlaps another secret input",
                    display(path)
                ))
            })?;
        }
        Ok(Self {
            values: inputs.values,
            templates,
            secrets,
        })
    }

    /// The given references alone, without writing anything, so the stores they
    /// name can be checked first.
    pub fn references(&self) -> DriverInputs {
        let mut out = DriverInputs::default();
        for (path, planned) in &self.secrets {
            if let Planned::Ref(r) = &planned.0
                && let Ok(value) = serde_json::to_value(r)
            {
                // The layout was checked in `new`, so insertion cannot clash.
                let _ = insert(&mut out.secret_refs, path, value);
            }
        }
        out
    }

    /// Writes the values to `primary` under `{prefix}/{path}` and returns inputs
    /// that hold only references and placeholder templates.
    pub async fn write(
        self,
        primary: &dyn SecretStore,
        prefix: &str,
        written: &mut Vec<SecretRef>,
    ) -> ApiResult<DriverInputs> {
        let mut out = DriverInputs {
            values: self.values,
            ..DriverInputs::default()
        };
        for (path, template) in self.templates {
            insert(&mut out.secrets, &path, template)
                .map_err(|()| ApiError::internal("checked secret layout changed"))?;
        }
        for (path, planned) in self.secrets {
            let name = format!("{prefix}/{}", path.join("/"));
            let reference = planned.materialize(primary, &name, written).await?;
            let value = serde_json::to_value(reference).map_err(ApiError::internal)?;
            insert(&mut out.secret_refs, &path, value)
                .map_err(|()| ApiError::internal("checked secret layout changed"))?;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn inputs(v: Value) -> DriverInputs {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn body_errors_are_redacted() {
        let err = parse_body::<DriverInputs>(json!({"values": {}, "secrets": "hun\"ter2"}))
            .err()
            .unwrap();
        assert!(!err.message.contains("hun"), "{}", err.message);
        assert!(!err.message.contains("ter2"), "{}", err.message);
        assert!(err.message.contains("[REDACTED]"), "{}", err.message);
        let err = parse_body::<DriverInputs>(json!({"extra": 1}))
            .err()
            .unwrap();
        assert!(err.message.contains("`extra`"), "{}", err.message);
    }

    #[test]
    fn overlapping_paths_are_rejected() {
        let both = inputs(json!({
            "secrets": {"password": "hunter2"},
            "secret_refs": {"password": {"store": "openporch", "ref": "x"}}
        }));
        let err = PlannedInputs::new(both).err().unwrap();
        assert!(!err.message.contains("hunter2"));
        let nested = inputs(json!({
            "secrets": {"db": {"password": "hunter2"}},
            "secret_refs": {"db": {"store": "openporch", "ref": "x"}}
        }));
        assert!(PlannedInputs::new(nested).is_err());
    }

    #[test]
    fn errors_never_show_values() {
        let bad = inputs(json!({"secret_refs": {"password": {"value": 42, "extra": "hunter2"}}}));
        let err = PlannedInputs::new(bad).err().unwrap();
        assert!(!err.message.contains("hunter2"));
        assert!(err.message.contains("secret_refs.password"));
    }

    #[test]
    fn containers_may_use_reference_key_names() {
        let planned = PlannedInputs::new(inputs(json!({
            "secret_refs": {"variables": {
                "value": {"store": "openporch", "ref": "x"},
                "store": {"value": "s"},
            }}
        })))
        .unwrap();
        assert_eq!(planned.secrets.len(), 2);
    }

    #[test]
    fn literals_containing_placeholder_syntax_are_secrets() {
        let planned = PlannedInputs::new(inputs(json!({
            "secrets": {"a": "abc${def", "b": "x${not a path!}y", "c": "${context.app.id}"}
        })))
        .unwrap();
        assert_eq!(planned.templates.len(), 1);
        assert_eq!(planned.secrets.len(), 2);
    }

    #[test]
    fn escapes_are_applied_to_literal_secrets() {
        assert_eq!(literal_text("a$\\{context.env.id}"), "a${context.env.id}");
        assert_eq!(literal_text("abc${def"), "abc${def");
        assert_eq!(literal_text("plain"), "plain");
    }

    #[test]
    fn mixed_secret_templates_are_rejected() {
        let mixed = inputs(json!({"secrets": {"p": "hunter2-${context.env.id}"}}));
        let err = PlannedInputs::new(mixed).err().unwrap();
        assert!(err.message.contains("secrets.p"), "{}", err.message);
        assert!(!err.message.contains("hunter2"), "{}", err.message);
    }

    #[test]
    fn placeholders_stay_inline() {
        let planned = PlannedInputs::new(inputs(json!({
            "secrets": {"password": "${resources.postgres-instance.outputs.password}", "token": "t"}
        })))
        .unwrap();
        assert_eq!(planned.templates.len(), 1);
        assert_eq!(planned.secrets.len(), 1);
    }

    #[test]
    fn placeholder_in_reference_is_rejected() {
        let bad = inputs(json!({
            "secret_refs": {"password": {"store": "openporch", "ref": "${context.env.id}"}}
        }));
        let err = PlannedInputs::new(bad).err().unwrap();
        assert!(err.message.contains("placeholders are not allowed"));
    }
}
