//! Deployment logs: the single redaction layer every log line passes through
//! before it is persisted or streamed.
//!
//! Each resource gets a [`JobLog`]. The executor registers every secret value
//! it resolves for the resource with [`JobLog::secret`], and every line written
//! afterwards, by the executor or the planner, has those values replaced with
//! `[REDACTED]`. The planner also passes driver errors through the same log
//! before recording them. Lines go to one writer per deployment, which
//! persists them in order and stops at the deployment's log quota with an
//! explicit truncation line.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use openporch_core::ids::Descriptor;
use openporch_core::redact::Redactor;
use openporch_store::{LogLine, Store, now_ms};
use tokio::sync::{mpsc, oneshot};

use super::Deployment;

/// Lines written at once.
const BATCH: usize = 256;

enum Message {
    Line(LogLine),
    /// Everything sent before has been persisted once the reply arrives.
    Flush(oneshot::Sender<()>),
}

/// A deployment's log writer.
pub(super) struct DeploymentLog {
    tx: mpsc::UnboundedSender<Message>,
    /// Each resource's values to redact, shared by all of its logs.
    resources: Mutex<BTreeMap<String, Arc<Shared>>>,
}

impl DeploymentLog {
    /// Starts the writer for `deployment`, keeping at most `quota` bytes of
    /// messages.
    pub fn start<S: Store + 'static>(store: Arc<S>, deployment: &Deployment, quota: usize) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        let org = deployment.org.to_string();
        let app = deployment.app.to_string();
        let env = deployment.env.to_string();
        let id = deployment.id.clone();
        tokio::spawn(async move {
            let mut written = 0usize;
            let mut truncated = false;
            while let Some(first) = rx.recv().await {
                let mut lines = Vec::new();
                let mut flushes = Vec::new();
                let mut next = Some(first);
                while let Some(message) = next.take() {
                    match message {
                        Message::Line(line) if truncated => drop(line),
                        Message::Line(line) => {
                            written += line.message.len();
                            if written > quota {
                                truncated = true;
                                lines.push(LogLine {
                                    level: "warn".into(),
                                    message: format!(
                                        "the log reached its quota of {quota} bytes; later \
                                         lines were dropped"
                                    ),
                                    ..line
                                });
                            } else {
                                lines.push(line);
                            }
                        }
                        Message::Flush(done) => flushes.push(done),
                    }
                    if lines.len() < BATCH {
                        next = rx.try_recv().ok();
                    }
                }
                if let Err(e) = store.append_logs(&org, &app, &env, &id, lines).await {
                    tracing::error!(deployment = %id, error = %e, "could not write log lines");
                }
                for done in flushes {
                    let _ = done.send(());
                }
            }
        });
        Self {
            tx,
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
            .or_insert_with(|| {
                Arc::new(Shared {
                    redactor: Mutex::new(Redactor::default()),
                    latest: Mutex::default(),
                    tx: self.tx.clone(),
                })
            })
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
            shared: Arc::new(Shared {
                redactor: Mutex::new(Redactor::default()),
                latest: Mutex::default(),
                tx: self.tx.clone(),
            }),
            descriptor: String::new(),
            operation_id: None,
            guresid: None,
        }
    }

    /// Waits until every line sent so far is persisted.
    pub async fn flush(&self) {
        let (done, wait) = oneshot::channel();
        if self.tx.send(Message::Flush(done)).is_ok() {
            let _ = wait.await;
        }
    }
}

struct Shared {
    redactor: Mutex<Redactor>,
    /// The resource's latest operation and GUResID, which later lines about
    /// the resource name.
    latest: Mutex<Option<(String, String)>>,
    tx: mpsc::UnboundedSender<Message>,
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
        Self {
            shared: Arc::new(Shared {
                redactor: Mutex::new(Redactor::default()),
                latest: Mutex::default(),
                tx,
            }),
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

    fn write(&self, level: &str, message: &str) {
        let line = LogLine {
            seq: 0,
            operation_id: self.operation_id.clone(),
            descriptor: Some(self.descriptor.clone()).filter(|d| !d.is_empty()),
            guresid: self.guresid.clone(),
            at: now_ms(),
            level: level.to_owned(),
            message: self.redact(message),
        };
        // A line sent after the writer stopped is dropped.
        let _ = self.shared.tx.send(Message::Line(line));
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
