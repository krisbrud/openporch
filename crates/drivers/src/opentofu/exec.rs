//! Running `tofu` and `git` with a sanitized environment, collecting their output
//! into one run log that is redacted before anyone sees it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};

use std::sync::Arc;
use std::time::Duration;

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use openporch_core::redact::Redactor;
use tokio::io::AsyncReadExt as _;
use tokio::process::{Child, ChildStderr, ChildStdout, Command};

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
    secrets: Redactor,
}

impl RunLog {
    /// Adds values that must never appear in the log or in errors, together
    /// with the escaped forms `tofu` prints them in.
    pub fn redact_values(&mut self, values: impl IntoIterator<Item = String>) {
        self.secrets.add(values);
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
        self.secrets.redact(text)
    }

    /// The whole log, redacted.
    pub fn redacted(&self) -> String {
        self.redact(&self.text)
    }
}

/// Whether a command's standard output goes into the log.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Stdout {
    Logged,
    /// Machine-readable output that can hold secrets in any encoding, such as
    /// `tofu show -json`; only its size is logged.
    Withheld,
}

/// How a command's process group is stopped when its call is abandoned, such
/// as on timeout.
#[derive(Clone)]
pub(super) struct Stop {
    /// How long the group has to finish after `SIGINT`, which lets OpenTofu
    /// write state and release its lock, before `SIGKILL`.
    pub grace: Duration,
    /// Held until the group is gone, so that the next call for the same
    /// resource waits for it.
    pub hold: Arc<dyn Send + Sync>,
    /// The run directory, removed once the group is gone.
    pub dir: PathBuf,
    /// Set when a dropped call leaves the group to be stopped in the
    /// background, which then also removes `dir`.
    pub abandoned: Arc<AtomicBool>,
}

/// A running process group, stopped and reaped if dropped before it exits.
struct Running {
    child: Option<Child>,
    /// The command's output, drained until it exits even when abandoned:
    /// closing the pipes would kill OpenTofu with `SIGPIPE` the moment it
    /// reports the interrupt, before it writes state.
    pipes: Option<(ChildStdout, ChildStderr)>,
    stop: Stop,
}

/// Kills what is left of an abandoned group and removes its run directory when
/// dropped, so that even a cancelled reaper cleans up.
struct Reap {
    group: Pid,
    dir: PathBuf,
}

impl Drop for Reap {
    fn drop(&mut self) {
        let _ = killpg(self.group, Signal::SIGKILL);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        // `None` once the command has been reaped.
        let Some(group) = child.id().and_then(|id| i32::try_from(id).ok()) else {
            return;
        };
        let group = Pid::from_raw(group);
        let _ = killpg(group, Signal::SIGINT);
        let stop = self.stop.clone();
        let pipes = self.pipes.take();
        stop.abandoned.store(true, Ordering::SeqCst);
        let reap = Reap {
            group,
            dir: stop.dir.clone(),
        };
        // Every caller runs in a runtime; without one there is no grace
        // period, and `reap` kills the group at once.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _hold = stop.hold;
                let _reap = reap;
                let exited = async { tokio::join!(drain(pipes), wait_and_kill(&mut child, group)) };
                if tokio::time::timeout(stop.grace, exited).await.is_err() {
                    let _ = killpg(group, Signal::SIGKILL);
                    let _ = child.wait().await;
                }
            });
        }
    }
}

/// Reads and discards a command's output until every writer closes it.
async fn drain(pipes: Option<(ChildStdout, ChildStderr)>) {
    if let Some((mut out, mut err)) = pipes {
        let (mut sink_out, mut sink_err) = (tokio::io::sink(), tokio::io::sink());
        let _ = tokio::join!(
            tokio::io::copy(&mut out, &mut sink_out),
            tokio::io::copy(&mut err, &mut sink_err),
        );
    }
}

/// Waits for the command, then kills whatever it left running in its group,
/// which also closes any output pipes those processes held.
async fn wait_and_kill(child: &mut Child, group: Pid) -> std::io::Result<std::process::ExitStatus> {
    let status = child.wait().await;
    let _ = killpg(group, Signal::SIGKILL);
    status
}

/// Runs `program` with exactly `env` (plus [`PASSTHROUGH`]) in `dir`, in a
/// process group of its own, and appends what it printed to `log` under a
/// header naming the command. If the call is dropped, the group is stopped as
/// `stop` says.
pub(super) async fn run(
    log: &mut RunLog,
    program: &Path,
    args: &[&str],
    dir: &Path,
    env: &BTreeMap<String, String>,
    stdout_mode: Stdout,
    stop: &Stop,
) -> std::io::Result<Output> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(dir)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
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
    let mut child = cmd.spawn()?;
    let (Some(out), Some(err)) = (child.stdout.take(), child.stderr.take()) else {
        return Err(std::io::Error::other(
            "the command's output is not captured",
        ));
    };
    let group = child.id().and_then(|id| i32::try_from(id).ok());
    let mut running = Running {
        child: Some(child),
        pipes: Some((out, err)),
        stop: stop.clone(),
    };
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let status = {
        let (Some(child), Some((out, err))) = (running.child.as_mut(), running.pipes.as_mut())
        else {
            return Err(std::io::Error::other("the command is gone"));
        };
        let Some(group) = group else {
            return Err(std::io::Error::other("the command has no process ID"));
        };
        // Nothing the command started outlives it, nor keeps its output open.
        let (o, e, status) = tokio::join!(
            out.read_to_end(&mut stdout),
            err.read_to_end(&mut stderr),
            wait_and_kill(child, Pid::from_raw(group))
        );
        o?;
        e?;
        status?
    };
    running.child = None;
    running.pipes = None;
    let stdout = String::from_utf8_lossy(&stdout).into_owned();
    let stderr = String::from_utf8_lossy(&stderr).into_owned();
    match stdout_mode {
        Stdout::Logged => log.push(&stdout),
        Stdout::Withheld => log.push(&format!("({} bytes of output not logged)", stdout.len())),
    }
    log.push(&stderr);
    Ok(Output {
        success: status.success(),
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
