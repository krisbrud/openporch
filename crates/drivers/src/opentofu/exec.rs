//! Running `tofu` and `git` with a sanitized environment, collecting their output
//! into one run log that is redacted before anyone sees it.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;

use tokio::process::Command;

/// Environment variables passed through from the server: the program search
/// path, and proxy and CA settings the network needs. Nothing else, so the
/// server's own credentials never reach a module.
const PASSTHROUGH: [&str; 9] = [
    "PATH",
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "NO_PROXY",
    "https_proxy",
    "http_proxy",
    "no_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
];

/// The most log kept per run, in bytes; later output is dropped.
const MAX_LOG_BYTES: usize = 4 * 1024 * 1024;

/// What one command printed.
pub(super) struct Output {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// The output of every command in a run, and the values to redact from it.
#[derive(Default)]
pub(super) struct RunLog {
    text: String,
    secrets: Vec<String>,
}

impl RunLog {
    /// Adds values that must never appear in the log or in errors.
    pub fn redact_values(&mut self, values: impl IntoIterator<Item = String>) {
        self.secrets
            .extend(values.into_iter().filter(|v| v.chars().count() >= 4));
        self.secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        self.secrets.dedup();
    }

    fn push(&mut self, text: &str) {
        let room = MAX_LOG_BYTES.saturating_sub(self.text.len());
        if room == 0 {
            return;
        }
        let mut end = text.len().min(room);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        self.text.push_str(&text[..end]);
        if !self.text.ends_with('\n') {
            self.text.push('\n');
        }
    }

    /// `text` with every secret value replaced.
    pub fn redact(&self, text: &str) -> String {
        self.secrets
            .iter()
            .fold(text.to_owned(), |t, s| t.replace(s.as_str(), "[REDACTED]"))
    }

    /// The whole log, redacted.
    pub fn redacted(&self) -> String {
        self.redact(&self.text)
    }
}

/// Runs `program` with exactly `env` (plus [`PASSTHROUGH`]) in `dir`, and
/// appends what it printed to `log` under a header naming the command.
pub(super) async fn run(
    log: &mut RunLog,
    program: &Path,
    args: &[&str],
    dir: &Path,
    env: &BTreeMap<String, String>,
) -> std::io::Result<Output> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(dir)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // A cancelled call kills the process; reaping its whole group on
        // timeout is task 4.8.
        .kill_on_drop(true);
    for key in PASSTHROUGH {
        if let Ok(v) = std::env::var(key) {
            cmd.env(key, v);
        }
    }
    cmd.envs(env);
    let name = program
        .file_name()
        .map_or_else(|| "?".into(), |n| n.to_string_lossy().into_owned());
    log.push(&format!("$ {name} {}", args.join(" ")));
    let out = cmd.output().await?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    log.push(&stdout);
    log.push(&stderr);
    Ok(Output {
        success: out.status.success(),
        stdout,
        stderr,
    })
}
