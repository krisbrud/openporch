//! Where `porch` finds the server URL, organization and token: flags first, then the
//! `OPENPORCH_*` environment variables, then the config and credentials files.
//!
//! The config file (`config.toml`) holds `url`, `org` and the default `app` and
//! `env`. Tokens live in a separate credentials file (`credentials.toml`) next to
//! it, keyed by server URL so that a token is only ever sent to the server that
//! issued it. The credentials file is owner-only: it is written with mode `0600`
//! and refused when others can read it.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use openporch_client::{Token, server_url, url_key};
use serde::{Deserialize, Serialize};

/// Looks up an environment variable.
pub type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

/// The config file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    /// The server URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The organization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org: Option<String>,
    /// The default application.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    /// The default environment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
}

/// A stored token.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credential {
    /// The token value.
    pub token: String,
    /// `api` for an API token, `session` for a login session.
    pub kind: String,
    /// The member the token belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// When a session expires, in Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential")
            .field("token", &"<redacted>")
            .field("kind", &self.kind)
            .field("user", &self.user)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// The credentials file: tokens by server URL.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credentials {
    /// Tokens by normalized server URL.
    #[serde(default)]
    pub servers: BTreeMap<String, Credential>,
}

/// The config and credentials file locations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// The config file.
    pub config: PathBuf,
    /// The credentials file.
    pub credentials: PathBuf,
}

impl Paths {
    /// The files for `--config` or `OPENPORCH_CONFIG`, else the user's config
    /// directory: `$XDG_CONFIG_HOME/openporch`, `~/.config/openporch` or, on
    /// Windows, `%APPDATA%\openporch`.
    pub fn locate(flag: Option<&Path>, env: Env<'_>) -> Result<Self, String> {
        let config = match flag.map(Path::to_path_buf).or_else(|| {
            env("OPENPORCH_CONFIG")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        }) {
            Some(path) => path,
            None => {
                let dir = env("XDG_CONFIG_HOME")
                    .filter(|v| !v.is_empty())
                    .map(PathBuf::from)
                    .or_else(|| {
                        env("HOME")
                            .filter(|v| !v.is_empty())
                            .map(|h| Path::new(&h).join(".config"))
                    })
                    .or_else(|| env("APPDATA").filter(|v| !v.is_empty()).map(PathBuf::from))
                    .ok_or("cannot find a config directory: set HOME or OPENPORCH_CONFIG")?;
                dir.join("openporch").join("config.toml")
            }
        };
        let credentials = config.with_file_name("credentials.toml");
        Ok(Self {
            config,
            credentials,
        })
    }

    /// Reads the config file; a missing file is empty.
    pub fn read_config(&self) -> Result<FileConfig, String> {
        match std::fs::read_to_string(&self.config) {
            // Only the parser's message and line: its snippet would quote the file.
            Ok(text) => toml::from_str(&text).map_err(|e| {
                let line = e
                    .span()
                    .map(|s| text[..s.start.min(text.len())].matches('\n').count() + 1);
                let at = line.map(|l| format!(" line {l}")).unwrap_or_default();
                format!("config file {}{at}: {}", self.config.display(), e.message())
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(FileConfig::default()),
            Err(e) => Err(format!("reading {}: {e}", self.config.display())),
        }
    }

    /// Writes the config file.
    pub fn write_config(&self, config: &FileConfig) -> Result<(), String> {
        let text = toml::to_string(config).map_err(|e| format!("config file: {e}"))?;
        write_file(&self.config, text.as_bytes(), false)
    }

    /// Reads the credentials file; a missing file is empty, and one others can read
    /// is refused.
    pub fn read_credentials(&self) -> Result<Credentials, String> {
        let path = &self.credentials;
        match std::fs::metadata(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Credentials::default());
            }
            Err(e) => return Err(format!("reading {}: {e}", path.display())),
            Ok(meta) => check_owner_only(path, &meta)?,
        }
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("reading {}: {e}", path.display()))?;
        // The parser's message could quote a line of the file, and so a token.
        toml::from_str(&text).map_err(|_| {
            format!(
                "credentials file {} is not valid; remove it and log in again",
                path.display()
            )
        })
    }

    /// Writes the credentials file, owner-only.
    pub fn write_credentials(&self, credentials: &Credentials) -> Result<(), String> {
        let text =
            toml::to_string(credentials).map_err(|_| "cannot encode the credentials".to_owned())?;
        write_file(&self.credentials, text.as_bytes(), true)
    }
}

#[cfg(unix)]
fn check_owner_only(path: &Path, meta: &std::fs::Metadata) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = meta.permissions().mode();
    if mode & 0o077 != 0 {
        return Err(format!(
            "credentials file {} can be read by other users (mode {:o}); run `chmod 600 {}`",
            path.display(),
            mode & 0o777,
            path.display()
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_owner_only(_path: &Path, _meta: &std::fs::Metadata) -> Result<(), String> {
    Ok(())
}

/// Writes `path` through a new, randomly named temporary file in the same
/// directory, so a crash leaves the old or the new file and nobody can plant the
/// temporary file in advance. Private files are owner-only, and their directory is
/// created owner-only and refused when others can write to it.
fn write_file(path: &Path, contents: &[u8], private: bool) -> Result<(), String> {
    let display = path.display();
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder
        .create(dir)
        .map_err(|e| format!("creating {}: {e}", dir.display()))?;
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(dir)
            .map_err(|e| format!("reading {}: {e}", dir.display()))?
            .permissions()
            .mode();
        if mode & 0o022 != 0 {
            return Err(format!(
                "{} can be written by other users (mode {:o}); keep the credentials file in an owner-only directory",
                dir.display(),
                mode & 0o777
            ));
        }
    }
    let result = (|| {
        // Created with `O_EXCL` and mode `0600`.
        let mut file = tempfile::NamedTempFile::new_in(dir)?;
        #[cfg(unix)]
        if !private {
            use std::os::unix::fs::PermissionsExt as _;
            file.as_file()
                .set_permissions(std::fs::Permissions::from_mode(0o644))?;
        }
        file.write_all(contents)?;
        file.as_file().sync_all()?;
        file.persist(path).map_err(|e| e.error)?;
        Ok::<_, std::io::Error>(())
    })();
    result.map_err(|e| format!("writing {display}: {e}"))
}

/// Settings given as flags.
#[derive(Debug, Clone, Default)]
pub struct Flags {
    /// `--url`.
    pub url: Option<String>,
    /// `--org`.
    pub org: Option<String>,
    /// `--app`.
    pub app: Option<String>,
    /// `--env`.
    pub env: Option<String>,
    /// `--token`.
    pub token: Option<Token>,
}

/// Where the token came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSource {
    /// `--token`.
    Flag,
    /// `OPENPORCH_TOKEN`.
    Env,
    /// The credentials file.
    File,
}

/// The resolved settings.
#[derive(Debug, Clone, Default)]
pub struct Settings {
    /// The server URL.
    pub url: Option<String>,
    /// The organization.
    pub org: Option<String>,
    /// The token and where it came from.
    pub token: Option<(Token, TokenSource)>,
    /// The default application.
    pub app: Option<String>,
    /// The default environment.
    pub env: Option<String>,
}

fn non_empty(v: Option<String>) -> Option<String> {
    v.filter(|v| !v.trim().is_empty())
}

/// Resolves flags, environment variables and files, in that order.
pub fn resolve(flags: &Flags, env: Env<'_>, file: &FileConfig, creds: &Credentials) -> Settings {
    let url = non_empty(flags.url.clone())
        .or_else(|| non_empty(env("OPENPORCH_URL")))
        .or_else(|| non_empty(file.url.clone()));
    let org = non_empty(flags.org.clone())
        .or_else(|| non_empty(env("OPENPORCH_ORG")))
        .or_else(|| non_empty(file.org.clone()));
    let token = flags
        .token
        .clone()
        .map(|t| (t, TokenSource::Flag))
        .or_else(|| {
            env("OPENPORCH_TOKEN")
                .and_then(Token::new)
                .map(|t| (t, TokenSource::Env))
        })
        .or_else(|| {
            let key = url_key(&server_url(url.as_deref()?).ok()?);
            let stored = creds.servers.get(&key)?;
            Token::new(stored.token.clone()).map(|t| (t, TokenSource::File))
        });
    Settings {
        url,
        org,
        token,
        app: non_empty(flags.app.clone())
            .or_else(|| non_empty(env("OPENPORCH_APP")))
            .or_else(|| non_empty(file.app.clone())),
        env: non_empty(flags.env.clone())
            .or_else(|| non_empty(env("OPENPORCH_ENV")))
            .or_else(|| non_empty(file.env.clone())),
    }
}
