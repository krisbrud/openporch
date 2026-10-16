//! Counters of the work the planner does, for the server's metrics.
//!
//! One server runs per process, so the counters are process-wide. Nothing is
//! sent anywhere: the server exposes them to scrapers that ask.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// How a deployment or driver call ended, as counted.
pub const OUTCOMES: [&str; 4] = ["succeeded", "failed", "retryable", "unknown"];

/// The driver actions counted.
pub const ACTIONS: [&str; 3] = ["provision", "delete", "observe"];

/// A count and the total seconds of what was counted.
#[derive(Debug, Default)]
pub struct Timed {
    count: AtomicU64,
    micros: AtomicU64,
}

impl Timed {
    const fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            micros: AtomicU64::new(0),
        }
    }

    fn add(&self, took: Duration) {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.micros.fetch_add(
            u64::try_from(took.as_micros()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }

    /// How many were counted.
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    /// Their total duration in seconds.
    pub fn seconds(&self) -> f64 {
        self.micros.load(Ordering::Relaxed) as f64 / 1e6
    }
}

/// The planner's counters.
#[derive(Debug)]
pub struct Metrics {
    /// Finished deployments by outcome (`succeeded` or `failed`).
    deployments: [AtomicU64; 2],
    /// Deployment runs, from start to finish, whatever their outcome.
    deployment_runs: Timed,
    /// Driver calls by action and outcome.
    driver_calls: [[Timed; 4]; 3],
    /// Driver calls running now.
    driver_active: AtomicU64,
}

/// The process's counters.
pub static METRICS: Metrics = Metrics {
    deployments: [AtomicU64::new(0), AtomicU64::new(0)],
    deployment_runs: Timed::new(),
    driver_calls: [
        [Timed::new(), Timed::new(), Timed::new(), Timed::new()],
        [Timed::new(), Timed::new(), Timed::new(), Timed::new()],
        [Timed::new(), Timed::new(), Timed::new(), Timed::new()],
    ],
    driver_active: AtomicU64::new(0),
};

fn index(all: &[&str], name: &str) -> Option<usize> {
    all.iter().position(|n| *n == name)
}

impl Metrics {
    /// Counts a finished deployment, which failed if `failed`.
    pub fn deployment_finished(&self, failed: bool) {
        self.deployments[usize::from(failed)].fetch_add(1, Ordering::Relaxed);
    }

    /// Finished deployments with `outcome` (`succeeded` or `failed`).
    pub fn deployments(&self, outcome: &str) -> u64 {
        self.deployments[usize::from(outcome != "succeeded")].load(Ordering::Relaxed)
    }

    /// Counts a deployment run that took `took`.
    pub fn deployment_ran(&self, took: Duration) {
        self.deployment_runs.add(took);
    }

    /// Deployment runs.
    pub fn deployment_runs(&self) -> &Timed {
        &self.deployment_runs
    }

    /// Counts a driver call for `action` that ended with `outcome` after `took`.
    pub fn driver_finished(&self, action: &str, outcome: &str, took: Duration) {
        if let (Some(a), Some(o)) = (index(&ACTIONS, action), index(&OUTCOMES, outcome)) {
            self.driver_calls[a][o].add(took);
        }
    }

    /// Driver calls for `action` that ended with `outcome`.
    pub fn driver_calls(&self, action: &str, outcome: &str) -> Option<&Timed> {
        let (a, o) = (index(&ACTIONS, action)?, index(&OUTCOMES, outcome)?);
        Some(&self.driver_calls[a][o])
    }

    /// Driver calls running now.
    pub fn driver_active(&self) -> u64 {
        self.driver_active.load(Ordering::Relaxed)
    }

    /// Counts a driver call as running until the guard is dropped.
    pub(crate) fn driver_started(&'static self) -> Running {
        self.driver_active.fetch_add(1, Ordering::Relaxed);
        Running(self)
    }
}

/// A running driver call, counted until dropped.
#[derive(Debug)]
pub(crate) struct Running(&'static Metrics);

impl Drop for Running {
    fn drop(&mut self) {
        self.0.driver_active.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_by_action_and_outcome() {
        let m = Metrics {
            deployments: Default::default(),
            deployment_runs: Timed::new(),
            driver_calls: Default::default(),
            driver_active: AtomicU64::new(0),
        };
        m.driver_finished("delete", "retryable", Duration::from_millis(1500));
        m.driver_finished("delete", "retryable", Duration::from_millis(500));
        m.driver_finished("nonsense", "failed", Duration::from_secs(1));
        let t = m.driver_calls("delete", "retryable").unwrap();
        assert_eq!((t.count(), t.seconds()), (2, 2.0));
        assert_eq!(m.driver_calls("provision", "retryable").unwrap().count(), 0);
        m.deployment_finished(true);
        assert_eq!(m.deployments("failed"), 1);
        assert_eq!(m.deployments("succeeded"), 0);
    }
}
