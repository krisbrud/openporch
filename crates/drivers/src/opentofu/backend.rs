//! Where OpenTofu state lives: owner-only files under the driver's root, or an
//! S3 bucket with one workspace per resource. The module's own backend, local
//! or not, is always overridden.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::json;

/// The orchestrator-managed state backend.
#[derive(Debug, Clone, Default)]
pub enum StateBackend {
    /// `<root>/state/<guresid>/terraform.tfstate`, owner-only. For single-host
    /// installations; keep the root on an encrypted volume.
    #[default]
    Local,
    /// One S3 object per resource.
    S3(Box<S3State>),
}

/// State in S3. Each resource is an OpenTofu workspace named by its GUResID,
/// so its state is `s3://<bucket>/<key_prefix>/<guresid>/terraform.tfstate`.
#[derive(Clone)]
pub struct S3State {
    pub bucket: String,
    pub region: String,
    pub key_prefix: String,
    /// A DynamoDB table for state locks; without it, S3 lock files.
    pub lock_table: Option<String>,
    /// An S3-compatible endpoint instead of AWS, such as an emulator.
    pub endpoint: Option<String>,
    /// A role the backend assumes before reading or writing state.
    pub role_arn: Option<String>,
    /// The backend's own identity, kept apart from the accounts resources are
    /// provisioned with. Without it the server's `AWS_ACCESS_KEY_ID`,
    /// `AWS_SECRET_ACCESS_KEY` and `AWS_SESSION_TOKEN` are used.
    pub credentials: Option<S3Credentials>,
}

impl S3State {
    /// State in `bucket` under the `openporch` prefix, with S3 lock files.
    pub fn new(bucket: impl Into<String>, region: impl Into<String>) -> Self {
        Self {
            bucket: bucket.into(),
            region: region.into(),
            key_prefix: "openporch".into(),
            lock_table: None,
            endpoint: None,
            role_arn: None,
            credentials: None,
        }
    }
}

impl std::fmt::Debug for S3State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3State")
            .field("bucket", &self.bucket)
            .field("region", &self.region)
            .field("key_prefix", &self.key_prefix)
            .field("lock_table", &self.lock_table)
            .field("endpoint", &self.endpoint)
            .field("role_arn", &self.role_arn)
            .field(
                "credentials",
                &self.credentials.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

/// Static AWS keys for the state backend.
#[derive(Clone)]
pub struct S3Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

impl S3Credentials {
    /// The server's own keys from its environment, if it has any.
    pub(super) fn from_env() -> Option<Self> {
        let var = |name| std::env::var(name).ok().filter(|v: &String| !v.is_empty());
        Some(Self {
            access_key_id: var("AWS_ACCESS_KEY_ID")?,
            secret_access_key: var("AWS_SECRET_ACCESS_KEY")?,
            session_token: var("AWS_SESSION_TOKEN"),
        })
    }
}

/// Where a resource's state is, retained so that a configuration change cannot
/// silently point a resource at different state.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub(super) enum BackendId {
    Local,
    S3 {
        bucket: String,
        key_prefix: String,
        endpoint: Option<String>,
    },
}

impl std::fmt::Display for BackendId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local => f.write_str("local state"),
            Self::S3 {
                bucket, key_prefix, ..
            } => write!(f, "s3://{bucket}/{key_prefix}"),
        }
    }
}

impl StateBackend {
    pub(super) fn id(&self) -> BackendId {
        match self {
            Self::Local => BackendId::Local,
            Self::S3(s) => BackendId::S3 {
                bucket: s.bucket.clone(),
                key_prefix: s.key_prefix.clone(),
                endpoint: s.endpoint.clone(),
            },
        }
    }

    /// Checks the configuration once, when the driver is built.
    pub(super) fn check(&self) -> Result<(), String> {
        let Self::S3(s) = self else {
            return Ok(());
        };
        let plain = |v: &str| {
            !v.is_empty()
                && v.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_./".contains(&b))
        };
        if !plain(&s.bucket) || !plain(&s.region) || !plain(&s.key_prefix) {
            return Err("the S3 state bucket, region and key prefix must be plain names".into());
        }
        if s.key_prefix.starts_with('/') || s.key_prefix.ends_with('/') {
            return Err("the S3 state key prefix must not start or end with `/`".into());
        }
        Ok(())
    }

    /// The override file that replaces the module's backend. It holds no
    /// credentials; those go into [`StateBackend::credentials_file`].
    pub(super) fn override_file(&self, state_file: &Path) -> String {
        match self {
            Self::Local => format!(
                "terraform {{\n  backend \"local\" {{\n    path = {}\n  }}\n}}\n",
                hcl(&state_file.display().to_string())
            ),
            Self::S3(s) => {
                let mut lines = vec![
                    format!("bucket = {}", hcl(&s.bucket)),
                    "key = \"terraform.tfstate\"".into(),
                    format!("workspace_key_prefix = {}", hcl(&s.key_prefix)),
                    format!("region = {}", hcl(&s.region)),
                    "encrypt = true".into(),
                    "skip_metadata_api_check = true".into(),
                ];
                match &s.lock_table {
                    Some(table) => lines.push(format!("dynamodb_table = {}", hcl(table))),
                    None => lines.push("use_lockfile = true".into()),
                }
                if let Some(role) = &s.role_arn {
                    lines.push(format!("assume_role = {{ role_arn = {} }}", hcl(role)));
                }
                if let Some(endpoint) = &s.endpoint {
                    lines.push(format!(
                        "endpoints = {{ s3 = {e}, dynamodb = {e}, sts = {e} }}",
                        e = hcl(endpoint)
                    ));
                    lines.push("use_path_style = true".into());
                    lines.push("skip_credentials_validation = true".into());
                    lines.push("skip_requesting_account_id = true".into());
                }
                let body: String = lines.iter().map(|l| format!("    {l}\n")).collect();
                format!("terraform {{\n  backend \"s3\" {{\n{body}  }}\n}}\n")
            }
        }
    }

    /// The backend's credentials as a `-backend-config` file, and the values
    /// to redact; `None` for local state.
    pub(super) fn credentials_file(&self) -> Result<Option<(String, Vec<String>)>, String> {
        let Self::S3(s) = self else {
            return Ok(None);
        };
        let creds = s
            .credentials
            .clone()
            .or_else(S3Credentials::from_env)
            .ok_or("the S3 state backend has no credentials")?;
        let mut text = format!(
            "access_key = {}\nsecret_key = {}\n",
            hcl(&creds.access_key_id),
            hcl(&creds.secret_access_key)
        );
        let mut secrets = vec![creds.secret_access_key.clone()];
        if let Some(token) = &creds.session_token {
            text.push_str(&format!("token = {}\n", hcl(token)));
            secrets.push(token.clone());
        }
        Ok(Some((text, secrets)))
    }

    /// Whether each resource is a workspace of its own.
    pub(super) fn workspaces(&self) -> bool {
        matches!(self, Self::S3(_))
    }
}

/// `s` as an HCL string literal: JSON escaping, plus escaped template
/// sequences.
fn hcl(s: &str) -> String {
    json!(s)
        .to_string()
        .replace("${", "$${")
        .replace("%{", "%%{")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s3_override_has_no_credentials() {
        let mut s3 = S3State::new("state-bucket", "eu-north-1");
        s3.lock_table = Some("locks".into());
        s3.credentials = Some(S3Credentials {
            access_key_id: "AKIASTATE".into(),
            secret_access_key: "state-secret".into(),
            session_token: Some("state-token".into()),
        });
        let backend = StateBackend::S3(Box::new(s3));
        let text = backend.override_file(Path::new("/unused"));
        assert!(text.contains(r#"backend "s3""#), "{text}");
        assert!(text.contains(r#"bucket = "state-bucket""#), "{text}");
        assert!(
            text.contains(r#"workspace_key_prefix = "openporch""#),
            "{text}"
        );
        assert!(text.contains(r#"dynamodb_table = "locks""#), "{text}");
        assert!(!text.contains("use_lockfile"), "{text}");
        assert!(!text.contains("state-secret"), "{text}");
        let (file, redact) = backend.credentials_file().unwrap().unwrap();
        assert!(file.contains(r#"secret_key = "state-secret""#), "{file}");
        assert!(file.contains(r#"token = "state-token""#), "{file}");
        assert_eq!(redact, ["state-secret", "state-token"]);
        assert!(!format!("{backend:?}").contains("state-secret"));
    }

    #[test]
    fn local_override_escapes_the_path() {
        let text = StateBackend::Local.override_file(Path::new("/srv/${x}/\"q\"/state"));
        assert!(
            text.contains(r#"path = "/srv/$${x}/\"q\"/state""#),
            "{text}"
        );
    }

    #[test]
    fn configuration_is_checked() {
        assert!(StateBackend::Local.check().is_ok());
        assert!(
            StateBackend::S3(Box::new(S3State::new("b", "r")))
                .check()
                .is_ok()
        );
        for (bucket, prefix) in [("b\"", "p"), ("b", "/p"), ("b", "p/"), ("", "p")] {
            let mut s = S3State::new(bucket, "r");
            s.key_prefix = prefix.into();
            assert!(
                StateBackend::S3(Box::new(s)).check().is_err(),
                "{bucket} {prefix}"
            );
        }
    }
}
