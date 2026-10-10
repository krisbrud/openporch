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
        Ok(())
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
}
