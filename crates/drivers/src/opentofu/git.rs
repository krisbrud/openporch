//! Fetching a module's Git source at one revision and resolving it to a commit.

use std::collections::BTreeMap;
use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use super::exec::{self, RunLog};
use super::files;
use super::{Config, Failure};

/// Where to fetch a module from.
pub(super) struct Source {
    pub url: String,
    /// A branch, tag or commit.
    pub rev: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub ssh_key: Option<String>,
}

/// Checks a source URL: `https`, `ssh` or scp-like `user@host:path`, and `file`
/// only when the server allows it. URLs carry no credentials.
pub(super) fn check_url(url: &str, allow_file: bool) -> Result<(), String> {
    if let Some((scheme, rest)) = url.split_once("://") {
        let allowed = matches!(scheme, "https" | "ssh") || (scheme == "file" && allow_file);
        if !allowed {
            return Err(format!(
                "`source.url` must use https or ssh, not `{scheme}`"
            ));
        }
        let authority = rest.split('/').next().unwrap_or_default();
        if scheme == "https" && authority.contains('@') {
            return Err("`source.url` must not contain credentials; use `source.username` and the `source.password` secret".into());
        }
        Ok(())
    } else if url.contains('@') && url.contains(':') && !url.starts_with('-') {
        Ok(())
    } else {
        Err("`source.url` must be an https, ssh or `user@host:path` URL".into())
    }
}

/// Fetches `source` at `rev` into `dir` and returns the commit it resolved to.
pub(super) async fn fetch(
    config: &Config,
    log: &mut RunLog,
    source: &Source,
    rev: &str,
    dir: &Path,
    home: &Path,
) -> Result<String, Failure> {
    files::private_dir(dir).map_err(Failure::io)?;
    let mut env = BTreeMap::from([
        ("HOME".to_owned(), home.display().to_string()),
        ("GIT_TERMINAL_PROMPT".to_owned(), "0".to_owned()),
        ("GIT_CONFIG_NOSYSTEM".to_owned(), "1".to_owned()),
        (
            "GIT_ALLOW_PROTOCOL".to_owned(),
            if config.allow_file_sources {
                "https:ssh:file"
            } else {
                "https:ssh"
            }
            .to_owned(),
        ),
    ]);
    // Credentials go through the environment, never the command line.
    if let Some(password) = &source.password {
        let user = source.username.as_deref().unwrap_or("git");
        let basic = STANDARD.encode(format!("{user}:{password}"));
        env.insert("GIT_CONFIG_COUNT".into(), "1".into());
        env.insert("GIT_CONFIG_KEY_0".into(), "http.extraHeader".into());
        env.insert(
            "GIT_CONFIG_VALUE_0".into(),
            format!("Authorization: Basic {basic}"),
        );
        log.redact_values([basic]);
    }
    if let Some(key) = &source.ssh_key {
        let key_file = home.join("ssh_key");
        let mut text = key.clone();
        if !text.ends_with('\n') {
            text.push('\n');
        }
        files::write_private(&key_file, text.as_bytes()).map_err(Failure::io)?;
        env.insert(
            "GIT_SSH_COMMAND".into(),
            format!(
                "ssh -i {} -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile={}",
                key_file.display(),
                home.join("known_hosts").display()
            ),
        );
    }
    let git = &config.git;
    for args in [
        vec!["init", "-q"],
        vec!["remote", "add", "origin", source.url.as_str()],
        vec!["fetch", "-q", "--depth", "1", "origin", rev],
        vec![
            "-c",
            "advice.detachedHead=false",
            "checkout",
            "-q",
            "FETCH_HEAD",
        ],
    ] {
        let out = exec::run(log, git, &args, dir, &env)
            .await
            .map_err(Failure::io)?;
        if !out.success {
            return Err(Failure::command(
                "git",
                &format!("fetching `{}` at `{rev}` failed", source.url),
                &out.stderr,
            ));
        }
    }
    let out = exec::run(log, git, &["rev-parse", "HEAD"], dir, &env)
        .await
        .map_err(Failure::io)?;
    let commit = out.stdout.trim().to_owned();
    if !out.success || commit.len() < 40 || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Failure::command(
            "git",
            "cannot resolve the fetched commit",
            &out.stderr,
        ));
    }
    Ok(commit)
}
