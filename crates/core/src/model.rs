//! Domain objects shared by the API, the store and the engine.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::{
    AccountId, AppId, ClassId, CriterionId, DefinitionId, Descriptor, EnvId, EnvTypeId, OrgId,
    ResId, ResourceTypeId,
};

/// A reference to a secret held in a secret store. References never contain placeholders.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretRef {
    /// The secret store ID, such as `openporch` for the built-in store.
    pub store: String,
    /// The store-specific name of the secret.
    #[serde(rename = "ref")]
    pub reference: String,
    /// A pinned version; the latest version is read when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// One matching criterion of a resource definition. Every field is optional.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Criterion {
    /// Generated ID, present once stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<CriterionId>,
    /// Matches the environment type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_type: Option<EnvTypeId>,
    /// Matches the application.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<AppId>,
    /// Matches the environment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_id: Option<EnvId>,
    /// Matches the resource ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub res_id: Option<ResId>,
    /// Matches the resource class. When absent, only the `default` class matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<ClassId>,
}

/// Driver inputs of a resource definition.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DriverInputs {
    /// Non-secret inputs. May contain placeholders, but no secret outputs.
    #[serde(default)]
    pub values: serde_json::Map<String, Value>,
    /// Secret inputs given as placeholder templates, such as
    /// `${resources.postgres-instance.outputs.password}`. Literal secret values are
    /// converted to `secret_refs` on write and never stored here.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub secrets: serde_json::Map<String, Value>,
    /// Secret inputs as references, mirroring the structure of `values`.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub secret_refs: serde_json::Map<String, Value>,
}

/// A co-provisioning entry of a resource definition.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provision {
    /// The source resource waits for the co-provisioned one when true.
    #[serde(default)]
    pub is_dependent: bool,
    /// Consumers of the source also depend on the co-provisioned resource when true.
    #[serde(default)]
    pub match_dependents: bool,
    /// Parameters passed to the co-provisioned resource. May contain placeholders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// A resource definition: how to provision one resource type, and when it applies.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceDefinition {
    /// Definition ID.
    pub id: DefinitionId,
    /// Display name.
    #[serde(default)]
    pub name: String,
    /// The resource type this definition provisions.
    #[serde(rename = "type")]
    pub ty: ResourceTypeId,
    /// The driver, such as `openporch/opentofu`.
    pub driver_type: String,
    /// A cloud account ID or a `${resources....account}` placeholder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver_account: Option<String>,
    /// Inputs passed to the driver.
    #[serde(default)]
    pub driver_inputs: DriverInputs,
    /// Co-provisioned resources keyed by `<type>[.<class>][#<id>]`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub provision: BTreeMap<String, Provision>,
    /// Matching criteria. A definition without criteria never matches.
    #[serde(default)]
    pub criteria: Vec<Criterion>,
}

/// Cloud account types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CloudAccountType {
    /// Static AWS credentials from secret references.
    Aws,
    /// AssumeRole from the orchestrator host's identity, with an external ID.
    AwsRole,
    /// Keyless access through openporch's OIDC provider.
    AwsIdentity,
}

/// A cloud account that drivers can receive credentials for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloudAccount {
    /// Account ID.
    pub id: AccountId,
    /// Display name.
    #[serde(default)]
    pub name: String,
    /// Account type.
    #[serde(rename = "type")]
    pub ty: CloudAccountType,
    /// Non-secret configuration, such as a role ARN.
    #[serde(default)]
    pub config: serde_json::Map<String, Value>,
    /// Secret configuration as references.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub secret_refs: BTreeMap<String, SecretRef>,
}

/// The context a resource is resolved in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceContext {
    /// Organization.
    pub org: OrgId,
    /// Application.
    pub app: AppId,
    /// Environment.
    pub env: EnvId,
    /// Environment type.
    pub env_type: EnvTypeId,
    /// Resource type, class and ID.
    pub res: Descriptor,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definition_from_json() {
        let def: ResourceDefinition = serde_json::from_value(serde_json::json!({
            "id": "stg-redis",
            "name": "Staging Redis",
            "type": "redis",
            "driver_type": "openporch/opentofu",
            "driver_inputs": {
                "values": {"source": {"url": "https://example.com/m.git"}},
                "secret_refs": {"variables": {"db_password": {"store": "prod-sm", "ref": "shop/db-password"}}}
            },
            "criteria": [{"env_type": "staging"}]
        }))
        .unwrap();
        assert_eq!(
            def.criteria[0].env_type.as_ref().unwrap().as_str(),
            "staging"
        );
        let r: SecretRef = serde_json::from_value(
            def.driver_inputs.secret_refs["variables"]["db_password"].clone(),
        )
        .unwrap();
        assert_eq!(r.reference, "shop/db-password");
        assert_eq!(r.version, None);
    }

    #[test]
    fn criterion_rejects_unknown_fields() {
        assert!(serde_json::from_str::<Criterion>(r#"{"environment": "dev"}"#).is_err());
    }

    #[test]
    fn cloud_account_types() {
        let t: CloudAccountType = serde_json::from_str("\"aws-role\"").unwrap();
        assert_eq!(t, CloudAccountType::AwsRole);
    }
}
