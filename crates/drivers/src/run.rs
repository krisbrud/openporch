//! Calling drivers within deadlines: per-call timeouts, cancellation on timeout,
//! and polling an asynchronous `put` or `delete` through `get` until it settles.

use std::future::Future;
use std::time::Duration;

use serde_json::{Map, Value};
use tokio::time::Instant;

use crate::{
    Cookie, DeleteOutcome, Driver, DriverError, DriverFuture, DriverOutputs, DriverRequest,
    Observation, PutOutcome,
};

/// The timeout of one driver call when the definition sets none.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// The longest timeout a definition may set.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(24 * 3600);

/// The shortest wait between polls, whatever the driver asks for.
const MIN_POLL: Duration = Duration::from_secs(1);

/// The most time kept back before the deadline for a last observation.
const FINAL_POLL_MARGIN: Duration = Duration::from_secs(30);

/// How long a driver gets to confirm a cancellation.
const CANCEL_GRACE: Duration = Duration::from_secs(60);

/// The timeout for a call: `driver_inputs.values.timeout` (whole seconds, or a
/// string such as `90s`, `15m` or `1h`, at most a day), else [`DEFAULT_TIMEOUT`],
/// bounded by the time left before the deployment's deadline.
pub fn call_timeout(
    values: &Map<String, Value>,
    remaining: Option<Duration>,
) -> Result<Duration, DriverError> {
    let configured = match values.get("timeout") {
        None | Some(Value::Null) => DEFAULT_TIMEOUT,
        Some(v) => parse_timeout(v)
            .filter(|t| *t <= MAX_TIMEOUT)
            .ok_or_else(|| DriverError::Invalid {
                code: None,
                message: format!(
                    "`timeout` must be a positive number of seconds or a duration such as \
                     `90s`, `15m` or `1h`, at most a day, not {v}"
                ),
            })?,
    };
    match remaining {
        Some(r) if r.is_zero() => Err(DriverError::Invalid {
            code: None,
            message: "the deployment deadline has passed; the driver was not called".into(),
        }),
        Some(r) => Ok(configured.min(r)),
        None => Ok(configured),
    }
}

fn parse_timeout(v: &Value) -> Option<Duration> {
    let secs = match v {
        Value::Number(n) => match n.as_u64() {
            Some(n) => n,
            // Whole numbers written as floats, such as `90.0`.
            None => {
                let f = n.as_f64()?;
                if f.fract() != 0.0 || !(1.0..=MAX_TIMEOUT.as_secs_f64()).contains(&f) {
                    return None;
                }
                // In range and whole, so the cast is exact.
                f as u64
            }
        },
        Value::String(s) => {
            let s = s.trim();
            let (digits, unit) = match s.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
                Some((i, _)) => s.split_at(i),
                None => (s, "s"),
            };
            let n: u64 = digits.parse().ok()?;
            let scale = match unit {
                "s" => 1,
                "m" => 60,
                "h" => 3600,
                _ => return None,
            };
            n.checked_mul(scale)?
        }
        _ => return None,
    };
    (secs > 0).then(|| Duration::from_secs(secs))
}

/// Runs one call, failing with [`DriverError::Timeout`] when it does not finish
/// within `timeout`. The call's future is dropped on timeout.
pub async fn with_timeout<T>(
    timeout: Duration,
    call: impl Future<Output = Result<T, DriverError>>,
) -> Result<T, DriverError> {
    tokio::time::timeout(timeout, call)
        .await
        .map_err(|_| DriverError::Timeout(timeout))?
}

/// How a `put` or `delete` settled, with the latest cookie: the caller checkpoints
/// it whether or not the call succeeded, since recovering a timed-out or uncertain
/// operation needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct Settled<T> {
    pub result: Result<T, DriverError>,
    pub cookie: Option<Cookie>,
}

/// Converges a resource: calls `put` and, while it reports `in_progress`, polls
/// `get` until the resource is ready, all within `timeout`. Each call receives the
/// latest cookie; a call that returns none leaves it unchanged.
///
/// On timeout, a driver with the `cancel` capability is asked to stop the
/// operation; unless it confirms that nothing is running, the outcome is
/// [`DriverError::Uncertain`] and must be recovered before another mutation.
pub async fn converge(
    driver: &dyn Driver,
    req: &DriverRequest,
    timeout: Duration,
) -> Settled<DriverOutputs> {
    let mut run = Run::new(driver, req, timeout);
    let result = run.converge().await;
    Settled {
        result,
        cookie: run.req.cookie,
    }
}

/// Removes a resource: calls `delete` and, while it reports `in_progress`, polls
/// `get` until the resource is absent, all within `timeout`. Timeouts are handled
/// as in [`converge`].
pub async fn remove(driver: &dyn Driver, req: &DriverRequest, timeout: Duration) -> Settled<()> {
    let mut run = Run::new(driver, req, timeout);
    let result = run.remove().await;
    Settled {
        result,
        cookie: run.req.cookie,
    }
}

struct Run<'d> {
    driver: &'d dyn Driver,
    req: DriverRequest,
    timeout: Duration,
    deadline: Instant,
}

impl<'d> Run<'d> {
    fn new(driver: &'d dyn Driver, req: &DriverRequest, timeout: Duration) -> Self {
        let timeout = timeout.min(MAX_TIMEOUT);
        Self {
            driver,
            req: req.clone(),
            timeout,
            deadline: Instant::now() + timeout,
        }
    }

    async fn converge(&mut self) -> Result<DriverOutputs, DriverError> {
        let mut wait = match self.bounded(self.driver.put(&self.req)).await? {
            PutOutcome::Ready { outputs, cookie } => {
                self.keep(cookie)?;
                return Ok(outputs);
            }
            PutOutcome::InProgress { poll_after, cookie } => {
                self.keep(cookie)?;
                poll_after
            }
        };
        loop {
            self.pause(wait).await?;
            match self.bounded(self.driver.get(&self.req)).await? {
                Observation::Ready { outputs, cookie } => {
                    self.keep(cookie)?;
                    return Ok(outputs);
                }
                Observation::InProgress { poll_after, cookie } => {
                    self.keep(cookie)?;
                    wait = poll_after;
                }
                Observation::Failed { error } => return Err(error),
                Observation::Absent => {
                    return Err(DriverError::Uncertain(
                        "`put` reported an operation in progress, but `get` found no resource"
                            .into(),
                    ));
                }
                Observation::Unknown { reason } => return Err(DriverError::Uncertain(reason)),
            }
        }
    }

    async fn remove(&mut self) -> Result<(), DriverError> {
        let mut wait = match self.bounded(self.driver.delete(&self.req)).await? {
            DeleteOutcome::Deleted => return Ok(()),
            DeleteOutcome::InProgress { poll_after, cookie } => {
                self.keep(cookie)?;
                poll_after
            }
        };
        loop {
            self.pause(wait).await?;
            match self.bounded(self.driver.get(&self.req)).await? {
                Observation::Absent => return Ok(()),
                Observation::InProgress { poll_after, cookie } => {
                    self.keep(cookie)?;
                    wait = poll_after;
                }
                // Still there while the deletion runs.
                Observation::Ready { cookie, .. } => {
                    self.keep(cookie)?;
                    wait = MIN_POLL;
                }
                Observation::Failed { error } => return Err(error),
                Observation::Unknown { reason } => return Err(DriverError::Uncertain(reason)),
            }
        }
    }

    /// Runs one call within the time left, cancelling the operation on timeout.
    async fn bounded<T>(&self, call: DriverFuture<'_, T>) -> Result<T, DriverError> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(self.timed_out().await);
        }
        match tokio::time::timeout(left, call).await {
            Ok(result) => result,
            Err(_) => Err(self.timed_out().await),
        }
    }

    /// The error for a timed-out operation, after asking the driver to cancel it
    /// when it can.
    async fn timed_out(&self) -> DriverError {
        if !self.driver.info().capabilities.cancel {
            return DriverError::Timeout(self.timeout);
        }
        match tokio::time::timeout(CANCEL_GRACE, self.driver.cancel(&self.req)).await {
            Ok(Ok(Observation::Absent | Observation::Failed { .. })) => {
                DriverError::Timeout(self.timeout)
            }
            _ => DriverError::Uncertain(format!(
                "the driver did not finish within {:?} and did not confirm that the \
                 operation stopped",
                self.timeout
            )),
        }
    }

    /// Records a cookie the driver returned. An oversized cookie, or one in a
    /// version the driver does not declare, cannot be checkpointed, so the outcome
    /// is uncertain although the driver may have succeeded.
    fn keep(&mut self, cookie: Option<Cookie>) -> Result<(), DriverError> {
        let Some(cookie) = cookie else {
            return Ok(());
        };
        cookie
            .check()
            .map_err(|e| DriverError::Uncertain(e.to_string()))?;
        let declared = self.driver.info().cookie_version;
        if cookie.version != declared {
            return Err(DriverError::Uncertain(format!(
                "the driver returned a version {} cookie but declares version {declared}",
                cookie.version
            )));
        }
        self.req.cookie = Some(cookie);
        Ok(())
    }

    /// Waits before the next poll. When the wait would pass the deadline, it ends
    /// early enough for one last observation; once that is spent, the operation
    /// has timed out.
    async fn pause(&self, wait: Duration) -> Result<(), DriverError> {
        let margin = (self.timeout / 20).min(FINAL_POLL_MARGIN);
        let last = self.deadline.checked_sub(margin).unwrap_or(self.deadline);
        let now = Instant::now();
        if now >= last {
            return Err(self.timed_out().await);
        }
        let until = now
            .checked_add(wait.max(MIN_POLL))
            .map_or(last, |u| u.min(last));
        tokio::time::sleep_until(until).await;
        Ok(())
    }
}
