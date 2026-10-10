//! Identifiers and their validation rules.
//!
//! Object IDs (organizations, applications, environments, environment types,
//! resource definitions, criteria, cloud accounts, classes) follow the
//! `api-access` rule `^[a-z0-9](?:-?[a-z0-9]+)+$` with at most 50 characters.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Maximum length of an object ID.
pub const MAX_ID_LEN: usize = 50;

/// An invalid identifier, with the rule it broke.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid {kind} `{value}`: {reason}")]
pub struct IdError {
    /// What kind of identifier was being parsed, such as `application ID`.
    pub kind: &'static str,
    /// The rejected input.
    pub value: String,
    /// Why it was rejected.
    pub reason: String,
}

impl IdError {
    fn new(kind: &'static str, value: &str, reason: impl Into<String>) -> Self {
        Self {
            kind,
            value: value.to_owned(),
            reason: reason.into(),
        }
    }
}

/// Explains the object ID pattern in error messages.
pub const OBJECT_ID_RULE: &str = "IDs use lowercase letters, digits and single hyphens, start and end \
     with a letter or digit, and are 2 to 50 characters long";

/// Returns whether `s` is a valid object ID.
pub fn is_valid_object_id(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() < 2 || b.len() > MAX_ID_LEN {
        return false;
    }
    let alnum = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    if !alnum(b[0]) || !alnum(b[b.len() - 1]) {
        return false;
    }
    b.windows(2).all(|w| match (w[0], w[1]) {
        (b'-', b'-') => false,
        (x, y) => (alnum(x) || x == b'-') && (alnum(y) || y == b'-'),
    })
}

fn validate_object_id(kind: &'static str, s: &str) -> Result<(), IdError> {
    if is_valid_object_id(s) {
        Ok(())
    } else {
        Err(IdError::new(kind, s, OBJECT_ID_RULE))
    }
}

macro_rules! object_id {
    ($(#[$doc:meta])* $name:ident, $kind:literal) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            /// Validates and wraps an ID.
            pub fn new(s: impl Into<String>) -> Result<Self, IdError> {
                let s = s.into();
                validate_object_id($kind, &s)?;
                Ok(Self(s))
            }

            /// The ID as a string.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        string_newtype_impls!($name, object_id_schema());
    };
}

macro_rules! string_newtype_impls {
    ($name:ident, $schema:expr) => {
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = IdError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::new(s)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = String::deserialize(d)?;
                Self::new(s).map_err(serde::de::Error::custom)
            }
        }

        impl schemars::JsonSchema for $name {
            fn schema_name() -> std::borrow::Cow<'static, str> {
                stringify!($name).into()
            }
            fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
                $schema
                    .try_into()
                    .expect("identifier schemas are objects")
            }
        }
    };
    ($name:ident) => {
        string_newtype_impls!($name, serde_json::json!({"type": "string"}));
    };
}

/// JSON Schema pattern of an object ID. With a minimum length of 2 it is the same rule
/// as [`is_valid_object_id`], written without nested quantifiers so it cannot backtrack
/// exponentially.
pub const OBJECT_ID_PATTERN: &str = "^[a-z0-9]+(?:-[a-z0-9]+)*$";

/// JSON Schema pattern of a resource type ID. The lookahead bounds the organization
/// prefix to the object ID length.
pub const RESOURCE_TYPE_PATTERN: &str =
    "^(?:(?=[a-z0-9-]{2,50}/)[a-z0-9]+(?:-[a-z0-9]+)*/)?[a-z0-9](?:[a-z0-9-]{0,48}[a-z0-9])?$";

fn object_id_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "string",
        "minLength": 2,
        "maxLength": MAX_ID_LEN,
        "pattern": OBJECT_ID_PATTERN,
    })
}

object_id!(
    /// The organization ID configured for the server.
    OrgId,
    "organization ID"
);
object_id!(
    /// An application ID.
    AppId,
    "application ID"
);
object_id!(
    /// An environment ID.
    EnvId,
    "environment ID"
);
object_id!(
    /// An environment type ID.
    EnvTypeId,
    "environment type ID"
);
object_id!(
    /// A resource definition ID.
    DefinitionId,
    "resource definition ID"
);
object_id!(
    /// A matching criterion ID.
    CriterionId,
    "criterion ID"
);
object_id!(
    /// A cloud account ID.
    AccountId,
    "cloud account ID"
);
object_id!(
    /// A workload name, which is also its module ID in a deployment set.
    WorkloadId,
    "workload name"
);

/// A resource class ID. `default` exists for every type.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClassId(String);

impl ClassId {
    /// Validates a class ID. Classes follow the object ID rule, and `default` is always valid.
    pub fn new(s: impl Into<String>) -> Result<Self, IdError> {
        let s = s.into();
        validate_object_id("class", &s)?;
        Ok(Self(s))
    }

    /// The implicit `default` class.
    pub fn default_class() -> Self {
        Self("default".to_owned())
    }

    /// Whether this is the `default` class.
    pub fn is_default(&self) -> bool {
        self.0 == "default"
    }

    /// The class as a string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for ClassId {
    fn default() -> Self {
        Self::default_class()
    }
}

string_newtype_impls!(ClassId, object_id_schema());

/// A resource type ID: a built-in name such as `postgres` or a custom `{org}/{name}`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ResourceTypeId(String);

impl ResourceTypeId {
    /// Validates a resource type ID.
    pub fn new(s: impl Into<String>) -> Result<Self, IdError> {
        let s = s.into();
        let ok = match s.split_once('/') {
            Some((org, name)) => is_valid_object_id(org) && is_type_name(name),
            None => is_type_name(&s),
        };
        if ok {
            Ok(Self(s))
        } else {
            Err(IdError::new(
                "resource type",
                &s,
                "types are lowercase names such as `postgres`, or `{org}/{name}` for custom types",
            ))
        }
    }

    /// Whether this is a custom type with an organization prefix.
    pub fn is_custom(&self) -> bool {
        self.0.contains('/')
    }

    /// The type as a string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn is_type_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_ID_LEN
        && s.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        && !s.starts_with('-')
        && !s.ends_with('-')
}

string_newtype_impls!(
    ResourceTypeId,
    serde_json::json!({
        "type": "string",
        "maxLength": 2 * MAX_ID_LEN + 1,
        "pattern": RESOURCE_TYPE_PATTERN,
    })
);

/// A resource ID within an environment, such as `modules.web.externals.db`,
/// `shared.api-dns` or `k8s-cluster`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ResId(String);

impl ResId {
    /// Validates a resource ID: dot-separated segments of letters, digits, `-` and `_`.
    pub fn new(s: impl Into<String>) -> Result<Self, IdError> {
        let s = s.into();
        let ok = !s.is_empty()
            && s.len() <= 255
            && s.split('.').all(|seg| {
                !seg.is_empty()
                    && seg
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
            });
        if ok {
            Ok(Self(s))
        } else {
            Err(IdError::new(
                "resource ID",
                &s,
                "resource IDs are dot-separated segments of letters, digits, `-` and `_`",
            ))
        }
    }

    /// The resource ID of a workload: `modules.<workload>`.
    pub fn workload(workload: &WorkloadId) -> Self {
        Self(format!("modules.{workload}"))
    }

    /// The resource ID of a private workload dependency: `modules.<workload>.externals.<key>`.
    pub fn private(workload: &WorkloadId, key: &str) -> Result<Self, IdError> {
        Self::new(format!("modules.{workload}.externals.{key}"))
    }

    /// The resource ID of a shared resource: `shared.<id>`.
    pub fn shared(id: &str) -> Result<Self, IdError> {
        Self::new(format!("shared.{id}"))
    }

    /// The resource ID as a string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

string_newtype_impls!(ResId);

/// A resource descriptor `<type>.<class>#<res_id>`, unique within an environment.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Descriptor {
    /// The resource type.
    pub ty: ResourceTypeId,
    /// The resource class.
    pub class: ClassId,
    /// The resource ID.
    pub id: ResId,
}

impl Descriptor {
    /// Builds a descriptor from its parts.
    pub fn new(ty: ResourceTypeId, class: ClassId, id: ResId) -> Self {
        Self { ty, class, id }
    }

    /// Parses `<type>.<class>#<res_id>`.
    pub fn parse(s: &str) -> Result<Self, IdError> {
        let err = || {
            IdError::new(
                "resource descriptor",
                s,
                "expected `<type>.<class>#<res_id>`",
            )
        };
        let (head, id) = s.split_once('#').ok_or_else(err)?;
        let (ty, class) = head.split_once('.').ok_or_else(err)?;
        Ok(Self {
            ty: ResourceTypeId::new(ty)?,
            class: ClassId::new(class)?,
            id: ResId::new(id)?,
        })
    }
}

impl fmt::Display for Descriptor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}#{}", self.ty, self.class, self.id)
    }
}

impl FromStr for Descriptor {
    type Err = IdError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl Serialize for Descriptor {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Descriptor {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// A Globally Unique Resource ID: permanent and non-semantic, so drivers and
/// OpenTofu state can be keyed by it. It is 32 lowercase hex characters (128 bits).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Guresid(String);

impl Guresid {
    /// Validates a GUResID string.
    pub fn new(s: impl Into<String>) -> Result<Self, IdError> {
        let s = s.into();
        if s.len() == 32
            && s.bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        {
            Ok(Self(s))
        } else {
            Err(IdError::new(
                "GUResID",
                &s,
                "expected 32 lowercase hexadecimal characters",
            ))
        }
    }

    /// Builds a GUResID from 16 random bytes supplied by the caller.
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes.iter().map(|b| format!("{b:02x}")).collect())
    }

    /// The GUResID as a string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

string_newtype_impls!(Guresid);

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn object_id_examples() {
        for ok in [
            "shop",
            "my-app",
            "a1",
            "dev",
            "0-0",
            "x".repeat(50).as_str(),
        ] {
            assert!(is_valid_object_id(ok), "{ok}");
        }
        for bad in [
            "",
            "a",
            "My App",
            "my_app",
            "-app",
            "app-",
            "my--app",
            "App",
            "x".repeat(51).as_str(),
        ] {
            assert!(!is_valid_object_id(bad), "{bad}");
        }
    }

    #[test]
    fn invalid_app_id_explains_pattern() {
        let err = AppId::new("My App").unwrap_err();
        assert_eq!(err.kind, "application ID");
        assert!(
            err.to_string()
                .contains("lowercase letters, digits and single hyphens")
        );
    }

    proptest! {
        #[test]
        fn object_id_matches_spec_regex(s in "[a-z0-9_ A-Z-]{0,60}") {
            let re = regex::Regex::new(r"^[a-z0-9](?:-?[a-z0-9]+)+$").unwrap();
            prop_assert_eq!(
                is_valid_object_id(&s),
                re.is_match(&s) && (2..=MAX_ID_LEN).contains(&s.len())
            );
        }

        #[test]
        fn object_id_matches_spec_regex_on_valid_alphabet(s in "[a-z0-9-]{0,55}") {
            let re = regex::Regex::new(OBJECT_ID_PATTERN).unwrap();
            prop_assert_eq!(
                is_valid_object_id(&s),
                re.is_match(&s) && (2..=MAX_ID_LEN).contains(&s.len())
            );
        }
    }

    #[test]
    fn descriptor_parsing() {
        let d = Descriptor::parse("postgres.default#modules.web.externals.db").unwrap();
        assert_eq!(d.ty.as_str(), "postgres");
        assert!(d.class.is_default());
        assert_eq!(d.id.as_str(), "modules.web.externals.db");
        assert_eq!(d.to_string(), "postgres.default#modules.web.externals.db");

        let d = Descriptor::parse("acme/feature-flags.large#shared.flags").unwrap();
        assert!(d.ty.is_custom());
        assert_eq!(d.class.as_str(), "large");

        for bad in [
            "postgres#x",
            "postgres.default",
            ".default#x",
            "postgres.Bad#x",
            "pg.default#a..b",
        ] {
            assert!(Descriptor::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn resource_id_helpers() {
        let web = WorkloadId::new("web").unwrap();
        assert_eq!(ResId::workload(&web).as_str(), "modules.web");
        assert_eq!(
            ResId::private(&web, "db").unwrap().as_str(),
            "modules.web.externals.db"
        );
        assert_eq!(ResId::shared("api-dns").unwrap().as_str(), "shared.api-dns");
    }

    #[test]
    fn guresid_format() {
        let g = Guresid::from_bytes([0xab; 16]);
        assert_eq!(g.as_str(), "ab".repeat(16));
        assert!(Guresid::new(g.as_str()).is_ok());
        assert!(Guresid::new("AB".repeat(16)).is_err());
        assert!(Guresid::new("ab").is_err());
    }

    #[test]
    fn ids_round_trip_through_serde() {
        let d: Descriptor = serde_json::from_str("\"dns.default#shared.api-dns\"").unwrap();
        assert_eq!(
            serde_json::to_string(&d).unwrap(),
            "\"dns.default#shared.api-dns\""
        );
        assert!(serde_json::from_str::<AppId>("\"My App\"").is_err());
    }
}
