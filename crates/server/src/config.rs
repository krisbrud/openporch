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
    /// How long shutdown waits for running deployments before it exits.
    #[serde(default = "default_shutdown_grace")]
    pub shutdown_grace_seconds: u64,
    /// The GitHub identity provider, when GitHub login is enabled.
    #[serde(default)]
    pub github: Option<GithubConfig>,
    /// Request rate limits.
    #[serde(default)]
    pub rate_limits: RateLimits,
    /// The `openporch/opentofu` driver.
    #[serde(default)]
    pub opentofu: OpenTofuSettings,
    /// Bounds on work and storage.
    #[serde(default)]
    pub limits: WorkLimits,
    /// Audit log retention.
    #[serde(default)]
    pub audit: AuditSettings,
    /// Container registries other than ECR.
    #[serde(default)]
    pub images: ImageSettings,
}

/// How images in registries other than ECR are pinned and checked.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageSettings {
    /// Pin images named by tag to their digests at acceptance, and check
    /// pinned digests before a deployment runs, reading the registries
    /// without credentials.
    #[serde(default = "default_true")]
    pub pin_other_registries: bool,
    /// Registries (`host[:port]`) reached over plain HTTP rather than HTTPS,
    /// such as a local test registry. They may be on private addresses.
    #[serde(default)]
    pub insecure_registries: Vec<String>,
    /// Registries (`host[:port]`) read over HTTPS on loopback, private or
    /// link-local addresses, which no other registry is read on.
    #[serde(default)]
    pub private_registries: Vec<String>,
}

impl Default for ImageSettings {
    fn default() -> Self {
        Self {
            pin_other_registries: true,
            insecure_registries: Vec::new(),
            private_registries: Vec::new(),
        }
    }
}

fn default_true() -> bool {
    true
}

/// The longest audit retention a server accepts: a hundred years.
const MAX_RETENTION_DAYS: u32 = 36_500;

/// Audit log retention. Nothing expires on its own: entries older than
/// `retention_days` are removed only when an administrator expires them.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditSettings {
    /// How many days of entries expiry keeps.
    #[serde(default = "default_retention_days")]
    pub retention_days: u32,
}

impl Default for AuditSettings {
    fn default() -> Self {
        Self {
            retention_days: default_retention_days(),
        }
    }
}

fn default_retention_days() -> u32 {
    30
}

/// The longest deployment time a server accepts: a week.
const MAX_DEPLOYMENT_MINUTES: u64 = 7 * 24 * 60;
/// The most driver operations or deployments a server runs at once.
const MAX_SLOTS: usize = 100_000;

/// Bounds on work and storage.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkLimits {
    /// Driver operations at once across every deployment, OpenTofu runs
    /// included.
    #[serde(default = "default_driver_concurrency")]
    pub driver_concurrency: usize,
    /// Deployments and deletions running or waiting for a driver slot at
    /// once; more are refused with `429`.
    #[serde(default = "default_running_deployments")]
    pub running_deployments: usize,
    /// How long one deployment may run, in minutes.
    #[serde(default = "default_deployment_minutes")]
    pub deployment_minutes: u64,
    /// The most log text kept per deployment, in bytes.
    #[serde(default = "default_deployment_log_bytes")]
    pub deployment_log_bytes: usize,
    /// The most resources in one deployment's graph.
    #[serde(default = "default_graph_nodes")]
    pub graph_nodes: usize,
    /// The most dependencies in one deployment's graph.
    #[serde(default = "default_graph_edges")]
    pub graph_edges: usize,
}

impl Default for WorkLimits {
    fn default() -> Self {
        Self {
            driver_concurrency: default_driver_concurrency(),
            running_deployments: default_running_deployments(),
            deployment_minutes: default_deployment_minutes(),
            deployment_log_bytes: default_deployment_log_bytes(),
            graph_nodes: default_graph_nodes(),
            graph_edges: default_graph_edges(),
        }
    }
}

fn default_driver_concurrency() -> usize {
    4
}

fn default_running_deployments() -> usize {
    64
}

fn default_deployment_minutes() -> u64 {
    30
}

fn default_deployment_log_bytes() -> usize {
    16 * 1024 * 1024
}

fn default_graph_nodes() -> usize {
    500
}

fn default_graph_edges() -> usize {
    5000
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
    /// Browser login with the authorization-code flow, for a web portal.
    #[serde(default)]
    pub browser: Option<GithubBrowserConfig>,
}

impl GithubConfig {
    /// GitHub.com with `client_id`.
    pub fn new(client_id: &str) -> Self {
        Self {
            client_id: client_id.to_owned(),
            api_url: default_github_api(),
            web_url: default_github_web(),
            browser: None,
        }
    }
}

/// Browser login through GitHub's authorization-code flow. It needs the OAuth app's
/// client secret, which stays in a file of its own rather than the configuration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubBrowserConfig {
    /// A file holding the OAuth app's client secret, readable only by the server's
    /// user.
    pub client_secret_file: PathBuf,
    /// This server's `/auth/github/callback` URL, as registered with the OAuth app.
    pub callback_url: String,
    /// The pages a finished login may return the browser to. A login names one of
    /// them exactly; no other address is accepted.
    pub return_urls: Vec<String>,
}

/// The path of the browser login's callback.
pub const GITHUB_CALLBACK_PATH: &str = "/auth/github/callback";

impl GithubBrowserConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        let callback =
            crate::identity::check_url("github.browser.callback_url", &self.callback_url)
                .map_err(ConfigError)?;
        if callback.path() != GITHUB_CALLBACK_PATH || callback.query().is_some() {
            return Err(ConfigError(format!(
                "`github.browser.callback_url` must be this server's `{GITHUB_CALLBACK_PATH}`, \
                 with no query"
            )));
        }
        if self.return_urls.is_empty() {
            return Err(ConfigError(
                "`github.browser.return_urls` needs at least one page to return to".into(),
            ));
        }
        for url in &self.return_urls {
            crate::identity::check_url("github.browser.return_urls", url).map_err(ConfigError)?;
        }
        Ok(())
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

fn default_shutdown_grace() -> u64 {
    60
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
            shutdown_grace_seconds: default_shutdown_grace(),
            github: None,
            rate_limits: RateLimits::default(),
            opentofu: OpenTofuSettings::default(),
            limits: WorkLimits::default(),
            audit: AuditSettings::default(),
            images: ImageSettings::default(),
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
        let l = &self.limits;
        for (name, value) in [
            ("driver_concurrency", l.driver_concurrency),
            ("running_deployments", l.running_deployments),
            ("deployment_log_bytes", l.deployment_log_bytes),
            ("graph_nodes", l.graph_nodes),
            ("graph_edges", l.graph_edges),
        ] {
            if value == 0 {
                return Err(ConfigError(format!("`limits.{name}` must be at least 1")));
            }
        }
        if l.deployment_minutes == 0 || l.deployment_minutes > MAX_DEPLOYMENT_MINUTES {
            return Err(ConfigError(format!(
                "`limits.deployment_minutes` must be between 1 and {MAX_DEPLOYMENT_MINUTES}"
            )));
        }
        for (name, value) in [
            ("driver_concurrency", l.driver_concurrency),
            ("running_deployments", l.running_deployments),
        ] {
            if value > MAX_SLOTS {
                return Err(ConfigError(format!(
                    "`limits.{name}` must be at most {MAX_SLOTS}"
                )));
            }
        }
        if !(1..=MAX_RETENTION_DAYS).contains(&self.audit.retention_days) {
            return Err(ConfigError(format!(
                "`audit.retention_days` must be between 1 and {MAX_RETENTION_DAYS}"
            )));
        }
        for (name, hosts) in [
            ("insecure_registries", &self.images.insecure_registries),
            ("private_registries", &self.images.private_registries),
        ] {
            for host in hosts {
                if !openporch_engine::registry::valid_registry_host(host) {
                    return Err(ConfigError(format!(
                        "`images.{name}` entry `{host}` is not `host[:port]`"
                    )));
                }
            }
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
            if let Some(browser) = &github.browser {
                browser.validate()?;
            }
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
    fn audit_retention() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("openporch.toml");
        let load = |text: &str| {
            std::fs::write(&path, format!("org = \"acme\"\ndata_dir = \"/x\"\n{text}")).unwrap();
            Config::load(Some(&path), |_| None)
        };
        assert_eq!(load("").unwrap().audit.retention_days, 30);
        let c = load("[audit]\nretention_days = 365\n").unwrap();
        assert_eq!(c.audit.retention_days, 365);
        let e = load("[audit]\nretention_days = 0\n").unwrap_err();
        assert!(e.to_string().contains("audit.retention_days"), "{e}");
        assert!(load("[audit]\nkeep = true\n").is_err());
    }

    #[test]
    fn image_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("openporch.toml");
        let load = |text: &str| {
            std::fs::write(&path, format!("org = \"acme\"\ndata_dir = \"/x\"\n{text}")).unwrap();
            Config::load(Some(&path), |_| None)
        };
        let c = load("").unwrap();
        assert!(c.images.pin_other_registries);
        assert!(c.images.insecure_registries.is_empty());
        let c = load(
            "[images]\npin_other_registries = false\ninsecure_registries = [\"localhost:5000\"]\n",
        )
        .unwrap();
        assert!(!c.images.pin_other_registries);
        assert_eq!(c.images.insecure_registries, ["localhost:5000"]);
        let e = load("[images]\ninsecure_registries = [\"http://r\"]\n").unwrap_err();
        assert!(e.to_string().contains("images.insecure_registries"), "{e}");
        let c = load("[images]\nprivate_registries = [\"r.internal:5000\", \"[fd00::1]:443\"]\n");
        assert_eq!(
            c.unwrap().images.private_registries,
            ["r.internal:5000", "[fd00::1]:443"]
        );
        for bad in [
            "r:port",
            "r:70000",
            "r:0",
            "[fd00::1",
            "[nope]:5000",
            ":5000",
        ] {
            let e = load(&format!("[images]\nprivate_registries = [\"{bad}\"]\n")).unwrap_err();
            assert!(
                e.to_string().contains("images.private_registries"),
                "{bad}: {e}"
            );
        }
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

    #[test]
    fn work_limits() {
        let env = |_: &str| None;
        let load = |text: &str| {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("c.toml");
            std::fs::write(&path, format!("org = \"acme\"\ndata_dir = \"/d\"\n{text}")).unwrap();
            Config::load(Some(&path), env)
        };
        let c = load("").unwrap();
        assert_eq!(c.limits, WorkLimits::default());
        assert_eq!(c.limits.driver_concurrency, 4);
        let c = load("[limits]\ndriver_concurrency = 8\ngraph_nodes = 100\n").unwrap();
        assert_eq!(c.limits.driver_concurrency, 8);
        assert_eq!(c.limits.graph_nodes, 100);
        let e = load("[limits]\nrunning_deployments = 0\n").unwrap_err();
        assert!(e.to_string().contains("limits.running_deployments"), "{e}");
        assert!(load("[limits]\nqueue = 1\n").is_err());
        let e = load("[limits]\ndeployment_minutes = 100000000000000000\n").unwrap_err();
        assert!(e.to_string().contains("limits.deployment_minutes"), "{e}");
        let e = load("[limits]\ndriver_concurrency = 1000000\n").unwrap_err();
        assert!(e.to_string().contains("limits.driver_concurrency"), "{e}");
    }

    #[test]
    fn github_browser_login() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("openporch.toml");
        let load = |browser: &str| {
            std::fs::write(
                &path,
                format!(
                    "org = \"acme\"\ndata_dir = \"/d\"\n[github]\nclient_id = \"x\"\n\
                     [github.browser]\nclient_secret_file = \"/s\"\n{browser}"
                ),
            )
            .unwrap();
            Config::load(Some(&path), |_| None)
        };
        let c = load(
            "callback_url = \"https://porch.example.com/auth/github/callback\"\n\
             return_urls = [\"https://portal.example.com/done\"]\n",
        )
        .unwrap();
        let browser = c.github.unwrap().browser.unwrap();
        assert_eq!(browser.return_urls, ["https://portal.example.com/done"]);
        for (bad, message) in [
            (
                "callback_url = \"https://porch.example.com/callback\"\nreturn_urls = [\"https://p.example.com/\"]\n",
                "/auth/github/callback",
            ),
            (
                "callback_url = \"http://porch.example.com/auth/github/callback\"\nreturn_urls = [\"https://p.example.com/\"]\n",
                "must use https",
            ),
            (
                "callback_url = \"https://porch.example.com/auth/github/callback\"\nreturn_urls = []\n",
                "at least one",
            ),
            (
                "callback_url = \"https://porch.example.com/auth/github/callback\"\nreturn_urls = [\"http://evil.example.com/\"]\n",
                "must use https",
            ),
            (
                "callback_url = \"https://porch.example.com/auth/github/callback\"\nreturn_urls = [\"https://p.example.com/#x\"]\n",
                "fragment",
            ),
        ] {
            let e = load(bad).unwrap_err();
            assert!(e.to_string().contains(message), "{bad}: {e}");
        }
    }
}
