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

const CREDENTIALS_IN_URL: &str = "`source.url` must not contain credentials; use `source.username` and the `source.password` or `source.ssh_key` secret";

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
        let userinfo = authority.rsplit_once('@').map(|(u, _)| u);
        if (scheme == "https" && userinfo.is_some()) || userinfo.is_some_and(|u| u.contains(':')) {
            return Err(CREDENTIALS_IN_URL.into());
        }
        Ok(())
    } else if let Some((user, host)) = url.split_once('@')
        && host.contains(':')
        && !url.starts_with('-')
    {
        if user.contains(':') {
            return Err(CREDENTIALS_IN_URL.into());
        }
        Ok(())
    } else {
        Err("`source.url` must be an https, ssh or `user@host:path` URL".into())
    }
}

/// `s` quoted for the shell that runs `GIT_SSH_COMMAND`.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
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
    // SSH checks host keys against the configured known hosts only; without
    // them, SSH sources fail rather than trust whatever host answers.
    let known_hosts = config
        .ssh_known_hosts
        .as_deref()
        .unwrap_or(Path::new("/dev/null"));
    let mut ssh = format!(
        "ssh -o BatchMode=yes -o StrictHostKeyChecking=yes -o UserKnownHostsFile={}",
        shell_quote(&known_hosts.display().to_string())
    );
    if let Some(key) = &source.ssh_key {
        let key_file = home.join("ssh_key");
        let mut text = key.clone();
        if !text.ends_with('\n') {
            text.push('\n');
        }
        files::write_private(&key_file, text.as_bytes()).map_err(Failure::io)?;
        ssh.push_str(&format!(
            " -o IdentitiesOnly=yes -i {}",
            shell_quote(&key_file.display().to_string())
        ));
    }
    env.insert("GIT_SSH_COMMAND".into(), ssh);
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
        let out = exec::run(log, git, &args, dir, &env, exec::Stdout::Logged)
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
    let out = exec::run(
        log,
        git,
        &["rev-parse", "HEAD"],
        dir,
        &env,
        exec::Stdout::Logged,
    )
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

#[cfg(test)]
mod tests {
    use super::check_url;

    #[test]
    fn urls() {
        for ok in [
            "https://github.com/o/r.git",
            "ssh://git@github.com/o/r.git",
            "git@github.com:o/r.git",
        ] {
            assert!(check_url(ok, false).is_ok(), "{ok}");
        }
        for bad in [
            "https://user:pw@github.com/o/r.git",
            "https://token@github.com/o/r.git",
            "ssh://git:pw@github.com/o/r.git",
            "git:pw@github.com:o/r.git",
            "file:///etc",
            "ext::sh -c id",
            "-oProxyCommand=x@h:p",
        ] {
            assert!(check_url(bad, false).is_err(), "{bad}");
        }
        assert!(check_url("file:///tmp/repo", true).is_ok());
    }
}
