//! Metrics in the Prometheus text format, for a scraper that asks. Nothing is
//! sent anywhere on its own.

use std::fmt::Write;

use axum::extract::State;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use openporch_engine::metrics::{ACTIONS, METRICS, OUTCOMES};

use crate::error::ApiResult;
use crate::state::AppState;

/// Resource statuses that need an operator's recovery.
const NEEDS_RECOVERY: [&str; 2] = ["unknown", "recovery_required"];

/// One metric family in the text format.
struct Out(String);

impl Out {
    fn family(&mut self, name: &str, kind: &str, help: &str) {
        let _ = writeln!(self.0, "# HELP {name} {help}\n# TYPE {name} {kind}");
    }

    fn sample(&mut self, name: &str, labels: &[(&str, &str)], value: impl std::fmt::Display) {
        let labels: Vec<String> = labels
            .iter()
            .map(|(k, v)| format!("{k}=\"{}\"", escape(v)))
            .collect();
        if labels.is_empty() {
            let _ = writeln!(self.0, "{name} {value}");
        } else {
            let _ = writeln!(self.0, "{name}{{{}}} {value}", labels.join(","));
        }
    }

    fn gauge(&mut self, name: &str, help: &str, value: impl std::fmt::Display) {
        self.family(name, "gauge", help);
        self.sample(name, &[], value);
    }
}

/// A label value with `\`, `"` and newlines escaped.
fn escape(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

pub async fn get(State(state): State<AppState>) -> ApiResult<Response> {
    let stats = state.store.stats().await?;
    let mut out = Out(String::new());

    out.family(
        "openporch_build_info",
        "gauge",
        "The server's version; always 1.",
    );
    out.sample("openporch_build_info", &[("version", crate::VERSION)], 1);
    out.gauge(
        "openporch_ready",
        "1 when the server is ready, 0 when it is not.",
        u8::from(state.not_ready_reasons().is_empty()),
    );

    // Deployments.
    let queued = state.runners.queued();
    let running = state.runners.running().len().saturating_sub(queued);
    out.gauge(
        "openporch_deployments_queued",
        "Accepted deployments waiting for room to run.",
        queued,
    );
    out.gauge(
        "openporch_deployments_running",
        "Deployments running now.",
        running,
    );
    out.gauge(
        "openporch_deployments_capacity",
        "How many deployments may run or wait at once.",
        state.runners.capacity(),
    );
    out.family(
        "openporch_deployments_finished_total",
        "counter",
        "Finished deployments by outcome.",
    );
    for outcome in ["succeeded", "failed"] {
        out.sample(
            "openporch_deployments_finished_total",
            &[("outcome", outcome)],
            METRICS.deployments(outcome),
        );
    }
    let runs = METRICS.deployment_runs();
    out.family(
        "openporch_deployment_run_seconds",
        "summary",
        "How long deployments ran, from start to finish.",
    );
    out.sample("openporch_deployment_run_seconds_sum", &[], runs.seconds());
    out.sample("openporch_deployment_run_seconds_count", &[], runs.count());

    // Driver calls. A retryable outcome is followed by a retry while the
    // deployment has attempts and time left.
    out.gauge(
        "openporch_driver_calls_running",
        "Driver calls running now.",
        METRICS.driver_active(),
    );
    out.gauge(
        "openporch_driver_concurrency",
        "How many driver calls may run at once.",
        state.config.limits.driver_concurrency,
    );
    out.family(
        "openporch_driver_call_seconds",
        "summary",
        "Finished driver calls by action and outcome, and how long they took. \
         `retryable` calls are retried.",
    );
    for action in ACTIONS {
        for outcome in OUTCOMES {
            let Some(calls) = METRICS.driver_calls(action, outcome) else {
                continue;
            };
            let labels = [("action", action), ("outcome", outcome)];
            out.sample(
                "openporch_driver_call_seconds_sum",
                &labels,
                calls.seconds(),
            );
            out.sample(
                "openporch_driver_call_seconds_count",
                &labels,
                calls.count(),
            );
        }
    }

    // Resources.
    out.family(
        "openporch_resources",
        "gauge",
        "Active resources by status.",
    );
    for (status, n) in &stats.resources {
        out.sample("openporch_resources", &[("status", status)], n);
    }
    out.gauge(
        "openporch_resources_needing_recovery",
        "Resources whose last operation has an unknown outcome and that need recovery.",
        NEEDS_RECOVERY
            .iter()
            .filter_map(|s| stats.resources.get(*s))
            .sum::<i64>(),
    );

    // Storage.
    out.gauge(
        "openporch_database_bytes",
        "The database's size, free pages included.",
        stats.database_bytes,
    );
    out.gauge(
        "openporch_database_free_bytes",
        "The database's free pages, which new data reuses.",
        stats.free_bytes,
    );
    out.gauge(
        "openporch_audit_entries",
        "Audit log entries stored.",
        stats.audit_entries,
    );
    out.gauge(
        "openporch_deployment_log_lines",
        "Deployment log lines stored.",
        stats.log_lines,
    );

    Ok((
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        out.0,
    )
        .into_response())
}
