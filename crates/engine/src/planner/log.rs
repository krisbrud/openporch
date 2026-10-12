//! Deployment logs, and the redaction layer every line an executor writes
//! passes through before it is persisted or streamed.
//!
//! Each resource gets a [`JobLog`], which holds no secret values. The executor
//! writes to it through a [`RedactingLog`] it owns: it registers every secret
//! value it resolves there, and every line it writes, and every failure it
//! returns to the planner, has those values replaced with `[REDACTED]`. Lines
//! go to one writer per deployment, which persists them in order and stops at
//! the deployment's log quota with an explicit truncation line; only the
//! final status lines are written past it.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use openporch_core::ids::Descriptor;
use openporch_core::redact::Redactor;
use openporch_store::{LogLine, Store, now_ms};
use tokio::sync::{mpsc, oneshot};

use super::{Deployment, ExecFailure};

/// Lines written at once.
const BATCH: usize = 256;

/// Attempts to write a batch before its lines are given up.
const WRITE_ATTEMPTS: u32 = 3;

enum Message {
    Line(LogLine),
    /// Everything sent before has been written once the reply arrives, which
    /// counts the lines that could not be written, and nothing after it is.
    Close(oneshot::Sender<usize>),
}

/// The sending side of a deployment's log, shared by all of its logs.
struct Writer {
    tx: mpsc::UnboundedSender<Message>,
    quota: usize,
    /// Bytes of messages accepted so far. Held while a line is checked
    /// against the quota and queued, so no line is queued after the
    /// truncation warning.
    used: Mutex<usize>,
    truncated: AtomicBool,
    closed: AtomicBool,
}

impl Writer {
    /// Sends `line` unless the log is closed, or over its quota and the line
    /// is not `exempt`. Lines over the quota are dropped here, so they never
    /// queue up in memory.
    fn send(&self, line: LogLine, exempt: bool) {
        if self.closed.load(Ordering::SeqCst) {
            return;
        }
        if exempt {
            // Final status lines neither count against the quota nor are
            // dropped by it.
            let _ = self.tx.send(Message::Line(line));
            return;
        }
        let mut used = self.used.lock().unwrap_or_else(|e| e.into_inner());
        if self.truncated.load(Ordering::SeqCst) {
            return;
        }
        *used += line.message.len();
        if *used > self.quota {
            if !self.truncated.swap(true, Ordering::SeqCst) {
                let _ = self.tx.send(Message::Line(LogLine {
                    seq: 0,
                    operation_id: None,
                    descriptor: None,
                    guresid: None,
                    at: line.at,
                    level: "warn".into(),
                    message: format!(
                        "the log reached its quota of {} bytes; later lines were dropped",
                        self.quota
                    ),
                }));
            }
            return;
        }
        let _ = self.tx.send(Message::Line(line));
    }

    /// Whether a line that is not exempt would be dropped.
    fn full(&self) -> bool {
        self.closed.load(Ordering::SeqCst) || self.truncated.load(Ordering::SeqCst)
    }
}

/// A deployment's log writer.
pub(super) struct DeploymentLog {
    writer: Arc<Writer>,
    /// Each resource's values to redact, shared by all of its logs.
    resources: Mutex<BTreeMap<String, Arc<Shared>>>,
}

impl DeploymentLog {
    /// Starts the writer for `deployment`, keeping at most `quota` bytes of
    /// messages besides the final status lines.
    pub fn start<S: Store + 'static>(store: Arc<S>, deployment: &Deployment, quota: usize) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        let org = deployment.org.to_string();
        let app = deployment.app.to_string();
        let env = deployment.env.to_string();
        let id = deployment.id.clone();
        tokio::spawn(async move {
            let mut lost = 0;
            let mut closed = false;
            while !closed {
                let Some(first) = rx.recv().await else { break };
                let mut lines = Vec::new();
                let mut replies = Vec::new();
                let mut next = Some(first);
                while let Some(message) = next.take() {
                    match message {
                        Message::Line(line) => lines.push(line),
                        Message::Close(done) => {
                            replies.push(done);
                            closed = true;
                            break;
                        }
                    }
                    if lines.len() < BATCH {
                        next = rx.try_recv().ok();
                    }
                }
                lost += write(&*store, [&org, &app, &env, &id], lines).await;
                for done in replies {
                    let _ = done.send(lost);
                }
            }
        });
        Self {
            writer: Arc::new(Writer {
                tx,
                quota,
                used: Mutex::new(0),
                truncated: AtomicBool::new(false),
                closed: AtomicBool::new(false),
            }),
            resources: Mutex::default(),
        }
    }

    /// The log of one resource. Every log of the same resource redacts the
    /// values registered with any of them.
    pub fn resource(&self, descriptor: &Descriptor) -> JobLog {
        let descriptor = descriptor.to_string();
        let shared = self
            .resources
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(descriptor.clone())
            .or_insert_with(|| Arc::new(Shared::new(self.writer.clone())))
            .clone();
        let (operation_id, guresid) = shared
            .latest
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unzip();
        JobLog {
            shared,
            descriptor,
            operation_id,
            guresid,
        }
    }

    /// The log for lines about the deployment as a whole.
    pub fn deployment(&self) -> JobLog {
        JobLog {
            shared: Arc::new(Shared::new(self.writer.clone())),
            descriptor: String::new(),
            operation_id: None,
            guresid: None,
        }
    }

    /// Waits until every line sent so far is written, returning how many
    /// lines could not be written, and drops every line sent later.
    pub async fn close(&self) -> usize {
        let writer = &self.writer;
        writer.closed.store(true, Ordering::SeqCst);
        let (done, wait) = oneshot::channel();
        match writer.tx.send(Message::Close(done)) {
            Ok(()) => wait.await.unwrap_or(0),
            Err(_) => 0,
        }
    }
}

/// Persists `lines`, retrying a failed write a few times. Returns how many
/// lines were lost.
async fn write<S: Store>(store: &S, [org, app, env, id]: [&str; 4], lines: Vec<LogLine>) -> usize {
    if lines.is_empty() {
        return 0;
    }
    for attempt in 1..=WRITE_ATTEMPTS {
        match store.append_logs(org, app, env, id, lines.clone()).await {
            Ok(()) => return 0,
            Err(e) if attempt == WRITE_ATTEMPTS => {
                tracing::error!(
                    deployment = %id, error = %e, lines = lines.len(),
                    "could not write log lines"
                );
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(50 * u64::from(attempt))).await,
        }
    }
    lines.len()
}

struct Shared {
    /// The resource's latest operation and GUResID, which later lines about
    /// the resource name.
    latest: Mutex<Option<(String, String)>>,
    writer: Arc<Writer>,
}

impl Shared {
    fn new(writer: Arc<Writer>) -> Self {
        Self {
            latest: Mutex::default(),
            writer,
        }
    }
}

/// Where the log lines about one resource go. It holds no secret values: an
/// executor writes to it only through a [`RedactingLog`], which it owns.
#[derive(Clone)]
pub struct JobLog {
    shared: Arc<Shared>,
    descriptor: String,
    operation_id: Option<String>,
    guresid: Option<String>,
}

impl JobLog {
    /// A log that drops every line, for jobs built outside a deployment.
    pub fn discard() -> Self {
        let (tx, _) = mpsc::unbounded_channel();
        let writer = Writer {
            tx,
            quota: 0,
            used: Mutex::new(0),
            truncated: AtomicBool::new(false),
            closed: AtomicBool::new(true),
        };
        Self {
            shared: Arc::new(Shared::new(Arc::new(writer))),
            descriptor: String::new(),
            operation_id: None,
            guresid: None,
        }
    }

    /// The same log, for lines about operation `operation_id`. Later logs of
    /// the resource name it too.
    pub(super) fn operation(&self, operation_id: &str, guresid: &str) -> Self {
        *self.shared.latest.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((operation_id.to_owned(), guresid.to_owned()));
        Self {
            operation_id: Some(operation_id.to_owned()),
            guresid: Some(guresid.to_owned()),
            ..self.clone()
        }
    }

    /// The executor's side of this log, which redacts the secret values the
    /// executor registers with it.
    pub fn redacting(&self) -> RedactingLog {
        RedactingLog {
            log: self.clone(),
            redactor: Arc::default(),
        }
    }

    /// Writes an informational line.
    pub(super) fn info(&self, message: &str) {
        self.write("info", message);
    }

    /// Writes a warning.
    pub(super) fn warn(&self, message: &str) {
        self.write("warn", message);
    }

    /// Writes an error.
    pub(super) fn error(&self, message: &str) {
        self.write("error", message);
    }

    /// Writes the resource's or the deployment's final status, which the
    /// quota does not drop.
    pub(super) fn status(&self, level: &str, message: &str) {
        self.send(level, message, true);
    }

    fn write(&self, level: &str, message: &str) {
        self.send(level, message, false);
    }

    fn send(&self, level: &str, message: &str, exempt: bool) {
        let writer = &self.shared.writer;
        if writer.full() && !exempt {
            return;
        }
        let line = LogLine {
            seq: 0,
            operation_id: self.operation_id.clone(),
            descriptor: Some(self.descriptor.clone()).filter(|d| !d.is_empty()),
            guresid: self.guresid.clone(),
            at: now_ms(),
            level: level.to_owned(),
            message: message.to_owned(),
        };
        writer.send(line, exempt);
    }
}

impl std::fmt::Debug for JobLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobLog")
            .field("descriptor", &self.descriptor)
            .field("operation_id", &self.operation_id)
            .finish_non_exhaustive()
    }
}

/// The executor's view of a job's log. It keeps the secret values the
/// executor resolved, which never leave the executor: every line it writes,
/// and every failure it hands back to the planner, has them replaced with
/// `[REDACTED]` first.
pub struct RedactingLog {
    log: JobLog,
    /// Shared with the driver's run-log sink, so values registered while a
    /// driver runs are redacted there too.
    redactor: Arc<Mutex<Redactor>>,
}

impl RedactingLog {
    fn redactor(&self) -> std::sync::MutexGuard<'_, Redactor> {
        // A panic while holding the lock leaves only a set of values.
        self.redactor.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Registers secret values to redact from now on.
    pub fn secret(&mut self, values: impl IntoIterator<Item = impl AsRef<str>>) {
        self.redactor().add(values);
    }

    /// `text` with every registered value replaced.
    pub fn redact(&self, text: &str) -> String {
        self.redactor().redact(text)
    }

    /// `failure` with every registered value replaced, for returning to the
    /// planner.
    pub fn failure(&self, failure: ExecFailure) -> ExecFailure {
        match failure {
            ExecFailure::Permanent(e) => ExecFailure::Permanent(self.redact(&e)),
            ExecFailure::Uncertain(e) => ExecFailure::Uncertain(self.redact(&e)),
            ExecFailure::Retryable { error, retry_after } => ExecFailure::Retryable {
                error: self.redact(&error),
                retry_after,
            },
        }
    }

    /// A sink for a driver's run log, redacting the registered values.
    pub fn sink(&self) -> openporch_drivers::LogSink {
        let log = RedactingLog {
            log: self.log.clone(),
            redactor: self.redactor.clone(),
        };
        // Values registered later are redacted from it too.
        openporch_drivers::LogSink::new(move |line| log.info(line))
    }

    /// Writes an informational line.
    pub fn info(&self, message: &str) {
        self.write("info", message);
    }

    /// Writes a warning.
    pub fn warn(&self, message: &str) {
        self.write("warn", message);
    }

    /// Writes an error.
    pub fn error(&self, message: &str) {
        self.write("error", message);
    }

    fn write(&self, level: &str, message: &str) {
        // A line that would be dropped is not redacted first.
        if self.log.shared.writer.full() {
            return;
        }
        self.log.send(level, &self.redact(message), false);
    }
}

/// Registered values are never shown.
impl std::fmt::Debug for RedactingLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedactingLog")
            .field("log", &self.log)
            .field("redactor", &self.redactor)
            .finish()
    }
}
