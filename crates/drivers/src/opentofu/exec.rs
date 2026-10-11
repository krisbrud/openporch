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
    /// Adds values that must never appear in the log or in errors, together
    /// with the escaped forms `tofu` prints them in.
    pub fn redact_values(&mut self, values: impl IntoIterator<Item = String>) {
        for v in values {
            if v.chars().count() < 4 {
                continue;
            }
            self.secrets.extend(escaped_forms(&v));
            self.secrets.push(v);
        }
        self.secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        self.secrets.dedup();
    }

    /// Appends `text`, redacted first so truncation cannot split a secret.
    fn push(&mut self, text: &str) {
        let text = &self.redact(text);
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

/// The forms a secret takes in JSON and HCL output: escaped quotes, backslashes
/// and control characters, Go's HTML-safe `\u003c`, `\u003e` and `\u0026`, and
/// HCL's `$${` and `%%{`.
fn escaped_forms(v: &str) -> Vec<String> {
    let json = serde_json::to_string(v).unwrap_or_default();
    let json = json
        .strip_prefix('"')
        .and_then(|j| j.strip_suffix('"'))
        .unwrap_or_default()
        .to_owned();
    let html = json
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026");
    let hcl = json.replace("${", "$${").replace("%{", "%%{");
    [json, html, hcl].into_iter().filter(|f| f != v).collect()
}

/// Whether a command's standard output goes into the log.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Stdout {
    Logged,
    /// Machine-readable output that can hold secrets in any encoding, such as
    /// `tofu show -json`; only its size is logged.
    Withheld,
}

/// Runs `program` with exactly `env` (plus [`PASSTHROUGH`]) in `dir`, and
/// appends what it printed to `log` under a header naming the command.
pub(super) async fn run(
    log: &mut RunLog,
    program: &Path,
    args: &[&str],
    dir: &Path,
    env: &BTreeMap<String, String>,
    stdout_mode: Stdout,
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
    // Never fall back to the server's own cloud identity.
    cmd.env("AWS_EC2_METADATA_DISABLED", "true");
    let name = program
        .file_name()
        .map_or_else(|| "?".into(), |n| n.to_string_lossy().into_owned());
    log.push(&format!("$ {name} {}", args.join(" ")));
    let out = cmd.output().await?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    match stdout_mode {
        Stdout::Logged => log.push(&stdout),
        Stdout::Withheld => log.push(&format!("({} bytes of output not logged)", stdout.len())),
    }
    log.push(&stderr);
    Ok(Output {
        success: out.status.success(),
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_escaped_forms() {
        let mut log = RunLog::default();
        log.redact_values(["a<b&c\"d\\e".to_owned()]);
        for text in ["a<b&c\"d\\e", r#"a<b&c\"d\\e"#, r#"a\u003cb\u0026c\"d\\e"#] {
            assert_eq!(log.redact(text), "[REDACTED]", "{text}");
        }
    }

    #[test]
    fn redacts_before_truncating() {
        let mut log = RunLog::default();
        let secret = "line one\nline two".to_owned();
        log.redact_values([secret.clone()]);
        log.push(&format!("{}{secret}", "x".repeat(MAX_LOG_BYTES - 10)));
        assert!(!log.text.contains("line one"));
    }
}
