//! Calling drivers within deadlines: per-call timeouts, and polling an asynchronous
//! `put` or `delete` through `get` until it settles.

use std::future::Future;
use std::time::Duration;

use serde_json::{Map, Value};
use tokio::time::Instant;

use crate::{
    Cookie, DeleteOutcome, Driver, DriverError, DriverOutputs, DriverRequest, Observation,
    PutOutcome,
};

/// The timeout of one driver call when the definition sets none.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// The shortest wait between polls, whatever the driver asks for.
const MIN_POLL: Duration = Duration::from_secs(1);

/// The timeout for a call: `driver_inputs.values.timeout` (whole seconds, or a
/// string such as `90s`, `15m` or `1h`), else [`DEFAULT_TIMEOUT`], bounded by the
/// time left before the deployment's deadline.
pub fn call_timeout(
    values: &Map<String, Value>,
    remaining: Option<Duration>,
) -> Result<Duration, DriverError> {
    let configured = match values.get("timeout") {
        None | Some(Value::Null) => DEFAULT_TIMEOUT,
        Some(v) => parse_timeout(v).ok_or_else(|| DriverError::Invalid {
            code: None,
            message: format!(
                "`timeout` must be a positive number of seconds or a duration such as \
                 `90s`, `15m` or `1h`, not {v}"
            ),
        })?,
    };
    Ok(remaining.map_or(configured, |r| configured.min(r)))
}

fn parse_timeout(v: &Value) -> Option<Duration> {
    let secs = match v {
        Value::Number(n) => n.as_u64()?,
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

/// Runs one driver call, failing with [`DriverError::Timeout`] when it does not
/// finish within `timeout`. The call's future is dropped on timeout, which cancels
/// an in-process driver; the outcome of what it already did is uncertain.
pub async fn with_timeout<T>(
    timeout: Duration,
    call: impl Future<Output = Result<T, DriverError>>,
) -> Result<T, DriverError> {
    tokio::time::timeout(timeout, call)
        .await
        .map_err(|_| DriverError::Timeout(timeout))?
}

/// Converges a resource: calls `put` and, while it reports `in_progress`, polls
/// `get` until the resource is ready, all within `timeout`. Returns the outputs
/// and the latest cookie, which the caller checkpoints for the next call. Each
/// call receives the latest cookie; a call that returns none leaves it unchanged.
pub async fn converge(
    driver: &dyn Driver,
    req: &DriverRequest,
    timeout: Duration,
) -> Result<(DriverOutputs, Option<Cookie>), DriverError> {
    let deadline = Instant::now() + timeout;
    let mut req = req.clone();
    let left = remaining(deadline, timeout)?;
    let mut wait = match with_timeout(left, driver.put(&req)).await? {
        PutOutcome::Ready { outputs, cookie } => {
            keep(driver, &mut req, cookie)?;
            return Ok((outputs, req.cookie));
        }
        PutOutcome::InProgress { poll_after, cookie } => {
            keep(driver, &mut req, cookie)?;
            poll_after
        }
    };
    loop {
        pause(wait, deadline, timeout).await?;
        let left = remaining(deadline, timeout)?;
        match with_timeout(left, driver.get(&req)).await? {
            Observation::Ready { outputs, cookie } => {
                keep(driver, &mut req, cookie)?;
                return Ok((outputs, req.cookie));
            }
            Observation::InProgress { poll_after } => wait = poll_after,
            Observation::Failed { error } => return Err(error),
            Observation::Absent => {
                return Err(DriverError::Uncertain(
                    "`put` reported an operation in progress, but `get` found no resource".into(),
                ));
            }
            Observation::Unknown { reason } => return Err(DriverError::Uncertain(reason)),
        }
    }
}

/// Removes a resource: calls `delete` and, while it reports `in_progress`, polls
/// `get` until the resource is absent, all within `timeout`.
pub async fn remove(
    driver: &dyn Driver,
    req: &DriverRequest,
    timeout: Duration,
) -> Result<(), DriverError> {
    let deadline = Instant::now() + timeout;
    let left = remaining(deadline, timeout)?;
    let mut wait = match with_timeout(left, driver.delete(req)).await? {
        DeleteOutcome::Deleted => return Ok(()),
        DeleteOutcome::InProgress { poll_after } => poll_after,
    };
    loop {
        pause(wait, deadline, timeout).await?;
        let left = remaining(deadline, timeout)?;
        match with_timeout(left, driver.get(req)).await? {
            Observation::Absent => return Ok(()),
            Observation::InProgress { poll_after } => wait = poll_after,
            // Still there while the deletion runs.
            Observation::Ready { .. } => wait = MIN_POLL,
            Observation::Failed { error } => return Err(error),
            Observation::Unknown { reason } => return Err(DriverError::Uncertain(reason)),
        }
    }
}

/// Records a cookie the driver returned, refusing an oversized one or one in a
/// version the driver does not declare.
fn keep(
    driver: &dyn Driver,
    req: &mut DriverRequest,
    cookie: Option<Cookie>,
) -> Result<(), DriverError> {
    let Some(cookie) = cookie else {
        return Ok(());
    };
    cookie.check()?;
    let declared = driver.info().cookie_version;
    if cookie.version != declared {
        return Err(DriverError::Invalid {
            code: None,
            message: format!(
                "the driver returned a version {} cookie but declares version {declared}",
                cookie.version
            ),
        });
    }
    req.cookie = Some(cookie);
    Ok(())
}

fn remaining(deadline: Instant, timeout: Duration) -> Result<Duration, DriverError> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        Err(DriverError::Timeout(timeout))
    } else {
        Ok(left)
    }
}

/// Waits before the next poll, failing when the deadline passes first.
async fn pause(wait: Duration, deadline: Instant, timeout: Duration) -> Result<(), DriverError> {
    let until = Instant::now() + wait.max(MIN_POLL);
    if until >= deadline {
        tokio::time::sleep_until(deadline).await;
        return Err(DriverError::Timeout(timeout));
    }
    tokio::time::sleep_until(until).await;
    Ok(())
}
