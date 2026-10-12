//! Server configuration: a TOML file with environment overrides.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Server configuration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The single organization this server serves.
    pub org: String,
    /// The data directory: database, lock, local state and, by default, keys.
    pub data_dir: PathBuf,
    /// The listen address.
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    /// The master key file. Defaults to `<data_dir>/master.key`. The environment
    /// variable `OPENPORCH_MASTER_KEY` takes precedence.
    #[serde(default)]
    pub master_key_file: Option<PathBuf>,
    /// Where the bootstrap token is written on first start. Defaults to
    /// `<data_dir>/bootstrap-token`.
    #[serde(default)]
    pub bootstrap_token_file: Option<PathBuf>,
    /// Lifetime of the bootstrap token in hours.
    #[serde(default = "default_bootstrap_hours")]
    pub bootstrap_token_hours: u32,
    /// Accept plain HTTP on a non-loopback address because a trusted reverse proxy
    /// terminates TLS in front of the server.
    #[serde(default)]
    pub tls_terminated_by_proxy: bool,
    /// Maximum request body size in bytes.
    #[serde(default = "default_body_limit")]
    pub max_body_bytes: usize,
    /// The GitHub identity provider, when GitHub login is enabled.
    #[serde(default)]
    pub github: Option<GithubConfig>,
    /// Request rate limits.
    #[serde(default)]
    pub rate_limits: RateLimits,
    /// The `openporch/opentofu` driver.
    #[serde(default)]
    pub opentofu: OpenTofuSettings,
}

/// The `openporch/opentofu` driver's settings.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenTofuSettings {
    /// The `tofu` binary.
    #[serde(default = "default_tofu")]
    pub tofu: PathBuf,
    /// The `git` binary.
    #[serde(default = "default_git")]
    pub git: PathBuf,
    /// Run directories, local state and the provider cache. Defaults to
    /// `<data_dir>/opentofu`.
    #[serde(default)]
    pub root: Option<PathBuf>,
    /// Host keys that `ssh` module sources are checked against.
    #[serde(default)]
    pub ssh_known_hosts: Option<PathBuf>,
    /// Where resources' state is kept.
    #[serde(default)]
    pub state: StateSettings,
}

impl Default for OpenTofuSettings {
    fn default() -> Self {
        Self {
            tofu: default_tofu(),
            git: default_git(),
            root: None,
            ssh_known_hosts: None,
            state: StateSettings::default(),
        }
    }
}

fn default_tofu() -> PathBuf {
    "tofu".into()
}

fn default_git() -> PathBuf {
    "git".into()
}

/// Where OpenTofu state is kept.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "backend", rename_all = "lowercase", deny_unknown_fields)]
pub enum StateSettings {
    /// Files under the driver's root.
    Local {},
    /// One S3 object per resource. The backend authenticates with the server's
    /// `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and `AWS_SESSION_TOKEN`.
    S3 {
        /// The bucket.
        bucket: String,
        /// The bucket's region.
        region: String,
        /// The key prefix; state is `<prefix>/<guresid>/terraform.tfstate`.
        #[serde(default = "default_key_prefix")]
        key_prefix: String,
        /// A DynamoDB table for locks; without it, S3 lock files.
        #[serde(default)]
        lock_table: Option<String>,
        /// An S3-compatible endpoint instead of AWS, such as an emulator.
        #[serde(default)]
        endpoint: Option<String>,
        /// A role the backend assumes.
        #[serde(default)]
        role_arn: Option<String>,
    },
}

impl Default for StateSettings {
    fn default() -> Self {
        Self::Local {}
    }
}

/// An S3 endpoint in canonical form, so that spelling it differently is not
/// a state migration. It names where state is in API responses and logs, so
/// it must not carry credentials.
fn state_endpoint(endpoint: &str) -> Result<String, ConfigError> {
    let url = crate::identity::check_base_url("opentofu.state.endpoint", endpoint)
        .map_err(ConfigError)?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ConfigError(
            "`opentofu.state.endpoint` must not contain credentials, a query or a fragment".into(),
        ));
    }
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

fn default_key_prefix() -> String {
    "openporch".into()
}

/// Per-minute request limits; zero disables one.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimits {
    /// Login requests (device login start and poll) per client address.
    #[serde(default = "default_login_limit")]
    pub login_per_minute: u32,
    /// API tokens created per calling principal.
    #[serde(default = "default_token_limit")]
    pub tokens_per_minute: u32,
}

impl Default for RateLimits {
    fn default() -> Self {
        Self {
            login_per_minute: default_login_limit(),
            tokens_per_minute: default_token_limit(),
        }
    }
}

fn default_login_limit() -> u32 {
    // A device login polls about every five seconds, so this leaves room for
    // several people behind one address.
    60
}

fn default_token_limit() -> u32 {
    20
}

/// The GitHub identity provider: an OAuth app (or GitHub App) with the device flow
/// enabled. The device flow and login lookups need only the public client ID.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubConfig {
    /// The OAuth client ID.
    pub client_id: String,
    /// The REST API base URL.
    #[serde(default = "default_github_api")]
    pub api_url: String,
    /// The web base URL that serves the OAuth endpoints.
    #[serde(default = "default_github_web")]
    pub web_url: String,
}

impl GithubConfig {
    /// GitHub.com with `client_id`.
    pub fn new(client_id: &str) -> Self {
        Self {
            client_id: client_id.to_owned(),
            api_url: default_github_api(),
            web_url: default_github_web(),
        }
    }
}

fn default_github_api() -> String {
    "https://api.github.com/".into()
}

fn default_github_web() -> String {
    "https://github.com/".into()
}

fn default_listen() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 8080))
}

fn default_bootstrap_hours() -> u32 {
    24
}

fn default_body_limit() -> usize {
    1024 * 1024
}

/// Configuration errors.
#[derive(Debug, thiserror::Error)]
#[error("configuration: {0}")]
pub struct ConfigError(String);

impl Config {
    /// A configuration with defaults for `org` and `data_dir`.
    pub fn new(org: &str, data_dir: &Path) -> Self {
        Self {
            org: org.to_owned(),
            data_dir: data_dir.to_owned(),
            listen: default_listen(),
            master_key_file: None,
            bootstrap_token_file: None,
            bootstrap_token_hours: default_bootstrap_hours(),
            tls_terminated_by_proxy: false,
            max_body_bytes: default_body_limit(),
            github: None,
            rate_limits: RateLimits::default(),
            opentofu: OpenTofuSettings::default(),
        }
    }

    /// Loads `path` (if given), then applies `OPENPORCH_ORG`, `OPENPORCH_DATA_DIR` and
    /// `OPENPORCH_LISTEN` from `env`, then validates.
    pub fn load(
        path: Option<&Path>,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, ConfigError> {
        let mut table: toml::Table = match path {
            Some(p) => {
                let text = std::fs::read_to_string(p)
                    .map_err(|e| ConfigError(format!("reading {}: {e}", p.display())))?;
                text.parse()
                    .map_err(|e| ConfigError(format!("{}: {e}", p.display())))?
            }
            None => toml::Table::new(),
        };
        for (var, key) in [
            ("OPENPORCH_ORG", "org"),
            ("OPENPORCH_DATA_DIR", "data_dir"),
            ("OPENPORCH_LISTEN", "listen"),
        ] {
            if let Some(v) = env(var) {
                table.insert(key.to_owned(), toml::Value::String(v));
            }
        }
        let config: Config = toml::Value::Table(table)
            .try_into()
            .map_err(|e: toml::de::Error| ConfigError(e.message().to_owned()))?;
        config.validate()?;
        Ok(config)
    }

    /// Checks the organization ID and the transport rule: plain HTTP only on loopback,
    /// unless a trusted proxy terminates TLS.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !openporch_core::ids::is_valid_object_id(&self.org) {
            return Err(ConfigError(format!(
                "org `{}` is invalid: {}",
                self.org,
                openporch_core::ids::OBJECT_ID_RULE
            )));
        }
        if !self.listen.ip().is_loopback() && !self.tls_terminated_by_proxy {
            return Err(ConfigError(format!(
                "listening on {} without TLS; plain HTTP is only allowed on loopback addresses. \
                 Put a TLS-terminating reverse proxy in front and set \
                 `tls_terminated_by_proxy = true`",
                self.listen
            )));
        }
        if let Some(github) = &self.github {
            if github.client_id.trim().is_empty() {
                return Err(ConfigError("`github.client_id` is empty".into()));
            }
            crate::identity::check_base_url("github.api_url", &github.api_url)
                .map_err(ConfigError)?;
            crate::identity::check_base_url("github.web_url", &github.web_url)
                .map_err(ConfigError)?;
        }
        if let StateSettings::S3 {
            bucket,
            region,
            key_prefix,
            lock_table,
            endpoint,
            role_arn,
        } = &self.opentofu.state
        {
            for (name, value) in [
                ("bucket", Some(bucket)),
                ("region", Some(region)),
                ("key_prefix", Some(key_prefix)),
                ("lock_table", lock_table.as_ref()),
                ("role_arn", role_arn.as_ref()),
            ] {
                if value.is_some_and(|v| v.trim().is_empty()) {
                    return Err(ConfigError(format!("`opentofu.state.{name}` is empty")));
                }
            }
            if key_prefix.starts_with('/') || key_prefix.ends_with('/') {
                return Err(ConfigError(
                    "`opentofu.state.key_prefix` must not start or end with `/`".into(),
                ));
            }
            if let Some(endpoint) = endpoint {
                state_endpoint(endpoint)?;
            }
        }
        Ok(())
    }

    /// The `openporch/opentofu` driver's configuration.
    pub fn opentofu(&self) -> openporch_drivers::OpenTofuConfig {
        let settings = &self.opentofu;
        let mut config = openporch_drivers::OpenTofuConfig::new(
            settings
                .root
                .clone()
                .unwrap_or_else(|| self.data_dir.join("opentofu")),
        );
        config.tofu = settings.tofu.clone();
        config.git = settings.git.clone();
        config.ssh_known_hosts = settings.ssh_known_hosts.clone();
        config.state = match &settings.state {
            StateSettings::Local {} => openporch_drivers::OpenTofuStateBackend::Local,
            StateSettings::S3 {
                bucket,
                region,
                key_prefix,
                lock_table,
                endpoint,
                role_arn,
            } => {
                let mut s3 = openporch_drivers::OpenTofuS3State::new(bucket, region);
                s3.key_prefix = key_prefix.clone();
                s3.lock_table = lock_table.clone();
                s3.endpoint = endpoint
                    .as_deref()
                    .map(|e| state_endpoint(e).unwrap_or_else(|_| e.to_owned()));
                s3.role_arn = role_arn.clone();
                openporch_drivers::OpenTofuStateBackend::S3(Box::new(s3))
            }
        };
        config
    }

    /// The master key file.
    pub fn master_key_path(&self) -> PathBuf {
        self.master_key_file
            .clone()
            .unwrap_or_else(|| self.data_dir.join("master.key"))
    }

    /// The bootstrap token file.
    pub fn bootstrap_token_path(&self) -> PathBuf {
        self.bootstrap_token_file
            .clone()
            .unwrap_or_else(|| self.data_dir.join("bootstrap-token"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_then_environment() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("openporch.toml");
        std::fs::write(&path, "org = \"acme\"\ndata_dir = \"/var/lib/openporch\"\n").unwrap();
        let c = Config::load(Some(&path), |k| {
            (k == "OPENPORCH_LISTEN").then(|| "127.0.0.1:9000".to_owned())
        })
        .unwrap();
        assert_eq!(c.org, "acme");
        assert_eq!(c.listen.port(), 9000);
        assert_eq!(
            c.master_key_path(),
            Path::new("/var/lib/openporch/master.key")
        );
        assert_eq!(c.bootstrap_token_hours, 24);
    }

    #[test]
    fn plain_http_only_on_loopback() {
        let e = Config::load(None, |k| match k {
            "OPENPORCH_ORG" => Some("acme".into()),
            "OPENPORCH_DATA_DIR" => Some("/tmp/x".into()),
            "OPENPORCH_LISTEN" => Some("0.0.0.0:8080".into()),
            _ => None,
        })
        .unwrap_err();
        assert!(e.to_string().contains("only allowed on loopback"), "{e}");
        let e = Config::load(None, |k| match k {
            "OPENPORCH_ORG" => Some("Acme Corp".into()),
            "OPENPORCH_DATA_DIR" => Some("/tmp/x".into()),
            _ => None,
        })
        .unwrap_err();
        assert!(e.to_string().contains("org `Acme Corp` is invalid"), "{e}");
    }

    #[test]
    fn opentofu_state_backend() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("openporch.toml");
        let load = |text: &str| {
            std::fs::write(
                &path,
                format!("org = \"acme\"\ndata_dir = \"/srv/op\"\n{text}"),
            )
            .unwrap();
            Config::load(Some(&path), |_| None)
        };
        let local = load("").unwrap().opentofu();
        assert_eq!(local.root, Path::new("/srv/op/opentofu"));
        assert_eq!(local.state.location(), "local state");

        let s3 = load(
            "[opentofu]\ntofu = \"/usr/bin/tofu\"\n[opentofu.state]\nbackend = \"s3\"\n\
             bucket = \"tf\"\nregion = \"eu-north-1\"\nendpoint = \"http://localhost:4566\"\n",
        )
        .unwrap()
        .opentofu();
        assert_eq!(s3.tofu, Path::new("/usr/bin/tofu"));
        assert_eq!(
            s3.state.location(),
            "s3://tf/openporch at http://localhost:4566"
        );

        let e = load(
            "[opentofu.state]\nbackend = \"s3\"\nbucket = \"tf\"\nregion = \"eu-north-1\"\n\
             endpoint = \"http://s3.example.com\"\n",
        )
        .unwrap_err();
        assert!(e.to_string().contains("must use https"), "{e}");
        for endpoint in [
            "https://AKIA:secret@s3.example.com",
            "https://s3.example.com/?X-Amz-Credential=AKIA",
        ] {
            let e = load(&format!(
                "[opentofu.state]\nbackend = \"s3\"\nbucket = \"tf\"\nregion = \"r\"\n\
                 endpoint = \"{endpoint}\"\n"
            ))
            .unwrap_err();
            assert!(
                e.to_string().contains("must not contain credentials"),
                "{e}"
            );
            assert!(!e.to_string().contains("secret"), "{e}");
        }
        // Spelling the same endpoint differently is not a move.
        let same = load(
            "[opentofu.state]\nbackend = \"s3\"\nbucket = \"tf\"\nregion = \"r\"\n\
             endpoint = \"HTTP://LOCALHOST:4566/\"\n",
        )
        .unwrap()
        .opentofu();
        assert_eq!(
            same.state.location(),
            "s3://tf/openporch at http://localhost:4566"
        );
        for bad in ["lock_table = \"\"", "key_prefix = \"/x/\""] {
            let e = load(&format!(
                "[opentofu.state]\nbackend = \"s3\"\nbucket = \"tf\"\nregion = \"r\"\n{bad}\n"
            ))
            .unwrap_err();
            assert!(e.to_string().contains("opentofu.state."), "{e}");
        }
        let e = load("[opentofu.state]\nbackend = \"s3\"\nbucket = \"tf\"\n").unwrap_err();
        assert!(e.to_string().contains("region"), "{e}");
        let e = load("[opentofu.state]\nbackend = \"local\"\nbucket = \"tf\"\n").unwrap_err();
        assert!(e.to_string().contains("bucket"), "{e}");
    }

    #[test]
    fn github_provider() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("openporch.toml");
        std::fs::write(
            &path,
            "org = \"acme\"\ndata_dir = \"/d\"\n[github]\nclient_id = \"Iv1.abc\"\n",
        )
        .unwrap();
        let c = Config::load(Some(&path), |_| None).unwrap();
        assert_eq!(c.github, Some(GithubConfig::new("Iv1.abc")));
        std::fs::write(
            &path,
            "org = \"acme\"\ndata_dir = \"/d\"\n[github]\nclient_id = \"x\"\n\
             api_url = \"http://ghe.example.com/api\"\n",
        )
        .unwrap();
        let e = Config::load(Some(&path), |_| None).unwrap_err();
        assert!(e.to_string().contains("must use https"), "{e}");
    }
}
