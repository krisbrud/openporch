//! Deployment logs: the single redaction layer every log line passes through
//! before it is persisted or streamed.
//!
//! Each resource gets a [`JobLog`]. The executor registers every secret value
//! it resolves for the resource with [`JobLog::secret`], and every line written
//! afterwards, by the executor or the planner, has those values replaced with
//! `[REDACTED]`. The planner also passes driver errors through the same log
//! before recording them. Lines go to one writer per deployment, which
//! persists them in order and stops at the deployment's log quota with an
//! explicit truncation line; only the final status lines are written past it.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use openporch_core::ids::Descriptor;
use openporch_core::redact::Redactor;
use openporch_store::{LogLine, Store, now_ms};
use tokio::sync::{mpsc, oneshot};

use super::Deployment;

/// Lines written at once.
const BATCH: usize = 256;

/// Attempts to write a batch before its lines are given up.
const WRITE_ATTEMPTS: u32 = 3;

enum Message {
    Line(LogLine),
    /// Everything sent before has been persisted once the reply arrives, and
    /// nothing after it is.
    Close(oneshot::Sender<()>),
}

/// The sending side of a deployment's log, shared by all of its logs.
struct Writer {
    tx: mpsc::UnboundedSender<Message>,
    quota: usize,
    /// Bytes of messages accepted so far.
    used: AtomicUsize,
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
        let used = self.used.fetch_add(line.message.len(), Ordering::SeqCst) + line.message.len();
        if used > self.quota && !exempt {
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
            let mut closed = None;
            while closed.is_none() {
                let Some(first) = rx.recv().await else { break };
                let mut lines = Vec::new();
                let mut next = Some(first);
                while let Some(message) = next.take() {
                    match message {
                        Message::Line(line) => lines.push(line),
                        Message::Close(done) => {
                            closed = Some(done);
                            break;
                        }
                    }
                    if lines.len() < BATCH {
                        next = rx.try_recv().ok();
                    }
                }
                write(&*store, [&org, &app, &env, &id], lines).await;
            }
            if let Some(done) = closed {
                let _ = done.send(());
            }
        });
        Self {
            writer: Arc::new(Writer {
                tx,
                quota,
                used: AtomicUsize::new(0),
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

    /// Waits until every line sent so far is persisted, and drops every line
    /// sent later, so that the log is complete once the deployment finishes.
    pub async fn close(&self) {
        let writer = &self.writer;
        writer.closed.store(true, Ordering::SeqCst);
        let (done, wait) = oneshot::channel();
        if writer.tx.send(Message::Close(done)).is_ok() {
            let _ = wait.await;
        }
    }
}

/// Persists `lines`, retrying a failed write a few times.
async fn write<S: Store>(store: &S, [org, app, env, id]: [&str; 4], lines: Vec<LogLine>) {
    if lines.is_empty() {
        return;
    }
    for attempt in 1..=WRITE_ATTEMPTS {
        match store.append_logs(org, app, env, id, lines.clone()).await {
            Ok(()) => return,
            Err(e) if attempt == WRITE_ATTEMPTS => {
                tracing::error!(
                    deployment = %id, error = %e, lines = lines.len(),
                    "could not write log lines"
                );
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(50 * u64::from(attempt))).await,
        }
    }
}

struct Shared {
    redactor: Mutex<Redactor>,
    /// The resource's latest operation and GUResID, which later lines about
    /// the resource name.
    latest: Mutex<Option<(String, String)>>,
    writer: Arc<Writer>,
}

impl Shared {
    fn new(writer: Arc<Writer>) -> Self {
        Self {
            redactor: Mutex::new(Redactor::default()),
            latest: Mutex::default(),
            writer,
        }
    }
}

/// Where the log lines about one resource go. Clones share the values to
/// redact.
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
            used: AtomicUsize::new(0),
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

    /// Registers secret values: from now on they never appear in this
    /// resource's log lines or in the errors the planner records for it.
    pub fn secret(&self, values: impl IntoIterator<Item = impl AsRef<str>>) {
        self.redactor().add(values);
    }

    /// `text` with every registered secret value replaced.
    pub fn redact(&self, text: &str) -> String {
        self.redactor().redact(text)
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
        // A line that would be dropped is not redacted first.
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
            message: self.redact(message),
        };
        writer.send(line, exempt);
    }

    fn redactor(&self) -> std::sync::MutexGuard<'_, Redactor> {
        self.shared
            .redactor
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }
}

/// Registered values are never shown.
impl std::fmt::Debug for JobLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobLog")
            .field("descriptor", &self.descriptor)
            .field("operation_id", &self.operation_id)
            .finish_non_exhaustive()
    }
}
