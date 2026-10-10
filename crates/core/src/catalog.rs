//! The resource type catalog: built-in types with input and output schemas,
//! resource classes, and output validation.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::ids::{ClassId, ResourceTypeId};

/// How a resource type is used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResourceUse {
    /// Requested by workloads under `resources`.
    Direct,
    /// Only referenced from other resources' definitions.
    Indirect,
    /// Exists once per environment, such as `base-env` or `k8s-cluster`.
    Implicit,
}

/// A resource type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceType {
    /// Type ID.
    pub id: ResourceTypeId,
    /// Display name.
    pub name: String,
    /// How the type is used.
    #[serde(rename = "use")]
    pub use_: ResourceUse,
    /// Grouping for display, such as `datastore`.
    pub category: String,
    /// JSON Schema of the resource inputs (workload `params`).
    pub inputs_schema: Value,
    /// JSON Schema of the outputs: an object with `values` and `secrets` objects.
    pub outputs_schema: Value,
}

/// The kind of an output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutputKind {
    /// A non-secret value.
    Value,
    /// A secret.
    Secret,
}

impl ResourceType {
    fn section(&self, kind: OutputKind) -> Option<&Map<String, Value>> {
        let key = match kind {
            OutputKind::Value => "values",
            OutputKind::Secret => "secrets",
        };
        self.outputs_schema.get("properties")?.get(key)?.as_object()
    }

    /// Whether outputs not named in the schema are allowed in this section.
    fn is_open(&self, kind: OutputKind) -> bool {
        self.section(kind)
            .and_then(|s| s.get("additionalProperties"))
            .is_none_or(|v| v != &Value::Bool(false))
    }

    fn declared(&self, kind: OutputKind) -> Vec<(&str, &Value, bool)> {
        let Some(section) = self.section(kind) else {
            return Vec::new();
        };
        let required: BTreeSet<&str> = section
            .get("required")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        section
            .get("properties")
            .and_then(Value::as_object)
            .map(|p| {
                p.iter()
                    .map(|(k, v)| (k.as_str(), v, required.contains(k.as_str())))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The kind of output `name`, or `None` if the schema does not declare it.
    /// Workload documents can only use declared outputs, unless the schema is open.
    pub fn output_kind(&self, name: &str) -> Option<OutputKind> {
        [OutputKind::Value, OutputKind::Secret]
            .into_iter()
            .find(|&k| self.declared(k).iter().any(|(n, _, _)| *n == name))
    }

    /// Whether workload documents may use outputs the schema does not declare.
    pub fn has_open_outputs(&self) -> bool {
        self.is_open(OutputKind::Value) || self.is_open(OutputKind::Secret)
    }

    /// Checks driver outputs against the schema: every required output is present,
    /// of the declared kind, and of the declared JSON type. Extra outputs are allowed.
    pub fn validate_outputs(
        &self,
        values: &Map<String, Value>,
        secrets: &BTreeSet<String>,
    ) -> Result<(), OutputError> {
        let err = |output: &str, problem: String| OutputError {
            ty: self.id.clone(),
            output: output.to_owned(),
            problem,
        };
        // A name in both maps would be stored and shown once as plaintext, so it is
        // rejected whether or not the type declares it.
        if let Some(name) = values.keys().find(|k| secrets.contains(*k)) {
            return Err(err(name, "must be a secret output, not a value".to_owned()));
        }
        for (name, schema, required) in self.declared(OutputKind::Value) {
            match values.get(name) {
                Some(v) => {
                    if let Some(want) = schema.get("type").and_then(Value::as_str)
                        && !json_type_matches(want, v)
                    {
                        return Err(err(name, format!("must be of type {want}")));
                    }
                }
                None if secrets.contains(name) => {
                    return Err(err(name, "must be a value output, not a secret".to_owned()));
                }
                None if required => return Err(err(name, "is missing".to_owned())),
                None => {}
            }
        }
        for (name, _, required) in self.declared(OutputKind::Secret) {
            // A plaintext copy in `values` would be stored and shown as a non-secret
            // output, so it is rejected even when the secret is also present.
            if values.contains_key(name) {
                return Err(err(name, "must be a secret output, not a value".to_owned()));
            }
            if required && !secrets.contains(name) {
                return Err(err(name, "is missing".to_owned()));
            }
        }
        Ok(())
    }
}

fn json_type_matches(want: &str, v: &Value) -> bool {
    match want {
        "string" => v.is_string(),
        "integer" => v.is_i64() || v.is_u64(),
        "number" => v.is_number(),
        "boolean" => v.is_boolean(),
        "object" => v.is_object(),
        "array" => v.is_array(),
        "null" => v.is_null(),
        _ => true,
    }
}

/// Driver outputs that do not satisfy the type's schema.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("output `{output}` of resource type `{ty}` {problem}")]
pub struct OutputError {
    /// The resource type.
    pub ty: ResourceTypeId,
    /// The output name.
    pub output: String,
    /// What is wrong.
    pub problem: String,
}

/// A resource class: a flavour of a resource type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceClass {
    /// Class ID.
    pub id: ClassId,
    /// The resource type.
    pub resource_type: ResourceTypeId,
    /// Description.
    #[serde(default)]
    pub description: String,
}

/// The classes defined for each type. `default` exists implicitly for every type.
#[derive(Debug, Clone, Default)]
pub struct ClassRegistry {
    classes: BTreeSet<(ResourceTypeId, ClassId)>,
}

impl ClassRegistry {
    /// Builds a registry from defined classes.
    pub fn new(classes: impl IntoIterator<Item = ResourceClass>) -> Self {
        Self {
            classes: classes
                .into_iter()
                .map(|c| (c.resource_type, c.id))
                .collect(),
        }
    }

    /// Whether `class` exists for `ty`.
    pub fn contains(&self, ty: &ResourceTypeId, class: &ClassId) -> bool {
        class.is_default() || self.classes.contains(&(ty.clone(), class.clone()))
    }
}

/// Builds an outputs schema. Each entry is `(name, JSON type, required)`.
fn outputs(values: &[(&str, &str, bool)], secrets: &[(&str, bool)], open: bool) -> Value {
    let section = |props: Map<String, Value>, required: Vec<&str>| {
        json!({
            "type": "object",
            "properties": props,
            "required": required,
            "additionalProperties": open,
        })
    };
    let vprops = values
        .iter()
        .map(|(n, t, _)| ((*n).to_owned(), json!({"type": t})))
        .collect();
    let vreq = values.iter().filter(|v| v.2).map(|v| v.0).collect();
    let sprops = secrets
        .iter()
        .map(|(n, _)| ((*n).to_owned(), json!({"type": "string"})))
        .collect();
    let sreq = secrets.iter().filter(|s| s.1).map(|s| s.0).collect();
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": {"values": section(vprops, vreq), "secrets": section(sprops, sreq)},
    })
}

fn open_inputs() -> Value {
    json!({"$schema": "https://json-schema.org/draft/2020-12/schema", "type": "object"})
}

fn ty(
    id: &str,
    name: &str,
    use_: ResourceUse,
    category: &str,
    inputs_schema: Value,
    outputs_schema: Value,
) -> ResourceType {
    ResourceType {
        id: ResourceTypeId::new(id).expect("built-in type IDs are valid"),
        name: name.to_owned(),
        use_,
        category: category.to_owned(),
        inputs_schema,
        outputs_schema,
    }
}

/// The built-in resource types.
pub fn builtin_types() -> Vec<ResourceType> {
    use ResourceUse::{Direct, Implicit, Indirect};
    let db = || {
        outputs(
            &[
                ("host", "string", true),
                ("name", "string", true),
                ("port", "integer", true),
            ],
            &[("username", true), ("password", true)],
            false,
        )
    };
    vec![
        ty(
            "workload",
            "Workload",
            Implicit,
            "workload",
            json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "class": {"type": "string"},
                    "spec": {"type": "object", "description": "The resolved workload document: containers or function, triggers, service, deploy, target"},
                    "dependencies": {"type": "object", "description": "Outputs of the workload's direct dependencies by key"}
                },
                "required": ["id", "class", "spec", "dependencies"],
                "additionalProperties": false
            }),
            outputs(&[("url", "string", false)], &[], true),
        ),
        ty(
            "base-env",
            "Base environment",
            Implicit,
            "environment",
            open_inputs(),
            outputs(&[], &[], true),
        ),
        ty(
            "k8s-cluster",
            "Kubernetes cluster",
            Implicit,
            "kubernetes",
            open_inputs(),
            outputs(
                &[
                    ("name", "string", true),
                    ("server", "string", false),
                    ("loadbalancer", "string", false),
                    ("region", "string", false),
                ],
                &[("credentials", false)],
                false,
            ),
        ),
        ty(
            "k8s-namespace",
            "Kubernetes namespace",
            Implicit,
            "kubernetes",
            open_inputs(),
            outputs(&[("namespace", "string", true)], &[], false),
        ),
        ty(
            "k8s-service-account",
            "Kubernetes service account",
            Indirect,
            "kubernetes",
            open_inputs(),
            outputs(&[("name", "string", true)], &[], false),
        ),
        ty(
            "config",
            "Configuration",
            Direct,
            "config",
            open_inputs(),
            outputs(&[], &[], true),
        ),
        ty(
            "dns",
            "DNS name",
            Direct,
            "dns",
            open_inputs(),
            outputs(&[("host", "string", true)], &[], false),
        ),
        ty(
            "route",
            "Route",
            Direct,
            "dns",
            json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {
                    "host": {"type": "string"},
                    "path": {"type": "string"},
                    "port": {"type": "integer"},
                    "service": {"type": "string"}
                }
            }),
            outputs(&[], &[], false),
        ),
        ty(
            "postgres",
            "PostgreSQL database",
            Direct,
            "datastore",
            open_inputs(),
            db(),
        ),
        ty(
            "postgres-instance",
            "PostgreSQL instance",
            Indirect,
            "datastore",
            open_inputs(),
            outputs(
                &[("host", "string", true), ("port", "integer", true)],
                &[("username", true), ("password", true)],
                false,
            ),
        ),
        ty(
            "mysql",
            "MySQL database",
            Direct,
            "datastore",
            open_inputs(),
            db(),
        ),
        ty(
            "redis",
            "Redis",
            Direct,
            "datastore",
            open_inputs(),
            outputs(
                &[("host", "string", true), ("port", "integer", true)],
                &[("username", false), ("password", true)],
                false,
            ),
        ),
        ty(
            "s3",
            "S3 bucket",
            Direct,
            "datastore",
            open_inputs(),
            outputs(
                &[
                    ("bucket", "string", true),
                    ("region", "string", true),
                    ("arn", "string", false),
                ],
                &[],
                false,
            ),
        ),
        ty(
            "sqs",
            "SQS queue",
            Direct,
            "messaging",
            open_inputs(),
            outputs(
                &[
                    ("arn", "string", true),
                    ("url", "string", true),
                    ("region", "string", true),
                ],
                &[],
                false,
            ),
        ),
        ty(
            "sns-topic",
            "SNS topic",
            Direct,
            "messaging",
            open_inputs(),
            outputs(
                &[
                    ("arn", "string", true),
                    ("region", "string", true),
                    ("name", "string", false),
                ],
                &[],
                false,
            ),
        ),
        ty(
            "dynamodb-table",
            "DynamoDB table",
            Direct,
            "datastore",
            open_inputs(),
            outputs(
                &[
                    ("name", "string", true),
                    ("arn", "string", true),
                    ("region", "string", true),
                    ("stream_arn", "string", false),
                ],
                &[],
                false,
            ),
        ),
        ty(
            "aws-role",
            "AWS IAM role",
            Indirect,
            "aws",
            open_inputs(),
            outputs(
                &[("arn", "string", true), ("name", "string", false)],
                &[],
                false,
            ),
        ),
        ty(
            "aws-policy",
            "AWS IAM policy",
            Indirect,
            "aws",
            open_inputs(),
            outputs(&[("arn", "string", true)], &[], false),
        ),
    ]
}

/// Looks up a built-in type.
pub fn builtin_type(id: &str) -> Option<ResourceType> {
    builtin_types().into_iter().find(|t| t.id.as_str() == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_type_list() {
        let ids: Vec<String> = builtin_types().iter().map(|t| t.id.to_string()).collect();
        assert_eq!(
            ids,
            [
                "workload",
                "base-env",
                "k8s-cluster",
                "k8s-namespace",
                "k8s-service-account",
                "config",
                "dns",
                "route",
                "postgres",
                "postgres-instance",
                "mysql",
                "redis",
                "s3",
                "sqs",
                "sns-topic",
                "dynamodb-table",
                "aws-role",
                "aws-policy",
            ]
        );
        for t in builtin_types() {
            assert!(
                t.outputs_schema["properties"]["values"].is_object(),
                "{}",
                t.id
            );
            assert!(
                t.outputs_schema["properties"]["secrets"].is_object(),
                "{}",
                t.id
            );
            assert!(!t.id.is_custom());
        }
    }

    #[test]
    fn postgres_schema() {
        let pg = builtin_type("postgres").unwrap();
        assert_eq!(pg.use_, ResourceUse::Direct);
        let values = &pg.outputs_schema["properties"]["values"];
        let secrets = &pg.outputs_schema["properties"]["secrets"];
        assert_eq!(values["required"], json!(["host", "name", "port"]));
        assert_eq!(
            values["properties"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            ["host", "name", "port"]
        );
        assert_eq!(secrets["required"], json!(["username", "password"]));
        assert_eq!(pg.output_kind("host"), Some(OutputKind::Value));
        assert_eq!(pg.output_kind("password"), Some(OutputKind::Secret));
        assert_eq!(pg.output_kind("hostname"), None);
        assert!(!pg.has_open_outputs());
    }

    #[test]
    fn workload_type() {
        let w = builtin_type("workload").unwrap();
        assert_eq!(w.use_, ResourceUse::Implicit);
        assert_eq!(
            w.inputs_schema["required"],
            json!(["id", "class", "spec", "dependencies"])
        );
        assert!(w.has_open_outputs());
    }

    fn secrets(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn output_validation() {
        let pg = builtin_type("postgres").unwrap();
        let mut values: Map<String, Value> =
            serde_json::from_value(json!({"host": "h", "name": "n", "port": 5432, "extra": 1}))
                .unwrap();
        assert!(
            pg.validate_outputs(&values, &secrets(&["username", "password"]))
                .is_ok()
        );

        let e = pg
            .validate_outputs(&values, &secrets(&["username"]))
            .unwrap_err();
        assert_eq!(
            e.to_string(),
            "output `password` of resource type `postgres` is missing"
        );

        values.remove("host");
        let e = pg
            .validate_outputs(&values, &secrets(&["username", "password"]))
            .unwrap_err();
        assert_eq!(e.output, "host");

        let bad: Map<String, Value> =
            serde_json::from_value(json!({"host": "h", "name": "n", "port": "5432"})).unwrap();
        let e = pg
            .validate_outputs(&bad, &secrets(&["username", "password"]))
            .unwrap_err();
        assert!(e.problem.contains("integer"));

        let leaked: Map<String, Value> =
            serde_json::from_value(json!({"host": "h", "name": "n", "port": 1, "password": "x"}))
                .unwrap();
        let e = pg
            .validate_outputs(&leaked, &secrets(&["username"]))
            .unwrap_err();
        assert!(e.problem.contains("must be a secret"));
        // Present as a secret and also leaked as a value.
        let e = pg
            .validate_outputs(&leaked, &secrets(&["username", "password"]))
            .unwrap_err();
        assert_eq!(e.output, "password");

        // Open types reject an undeclared name present in both maps too.
        let config = builtin_type("config").unwrap();
        let both: Map<String, Value> = serde_json::from_value(json!({"token": "x"})).unwrap();
        let e = config
            .validate_outputs(&both, &secrets(&["token"]))
            .unwrap_err();
        assert_eq!(e.output, "token");
        assert!(e.problem.contains("must be a secret"));
    }

    #[test]
    fn classes() {
        let s3: ResourceTypeId = "s3".parse().unwrap();
        let reg = ClassRegistry::new([ResourceClass {
            id: "large".parse().unwrap(),
            resource_type: s3.clone(),
            description: String::new(),
        }]);
        assert!(reg.contains(&s3, &ClassId::default()));
        assert!(reg.contains(&s3, &"large".parse().unwrap()));
        assert!(!reg.contains(&s3, &"sensitive".parse().unwrap()));
        assert!(!reg.contains(&"postgres".parse().unwrap(), &"large".parse().unwrap()));
    }
}
