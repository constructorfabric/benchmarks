//! Declarative alert-rule catalogue of the streaming metrics (DESIGN 4 `Alerting and SLO
//! contract (P1)`).
//!
//! The gear cannot fire alerts itself: it only records the series. This catalogue is the
//! testable, declarative form of the alert conditions that an operator loads into the metrics
//! backend. Every expression is built from the configured metric prefix and carries
//! low-cardinality labels only.
//!
//! Counter series carry the `_total` suffix that the OTLP to Prometheus conversion appends
//! (DESIGN 4 names them `{prefix}_orphan_detected_total`). Only recorded series are read: an
//! alert on a declared-but-unrecorded series could never fire.

use std::time::Duration;

/// The severity of an alert rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertSeverity {
    /// Non-paging: notifies the team channel.
    Warning,
    /// Paging: notifies the on-call rotation.
    Critical,
}

/// One alert rule: a `PromQL` expression that must hold for `for_duration` to raise the alert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlertRule {
    /// The unique rule name.
    pub name: String,
    /// The `PromQL` condition, built from the configured metric prefix.
    pub expr: String,
    /// How long the condition must hold before the alert is raised.
    pub for_duration: Duration,
    /// The severity of the raised alert.
    pub severity: AlertSeverity,
}

/// The 99th percentile of the histogram `{series}` over the 5 minute window, compared with
/// `threshold_ms`.
fn p99_above(series: &str, threshold_ms: u32) -> String {
    format!("histogram_quantile(0.99, sum by (le) (rate({series}_bucket[5m]))) > {threshold_ms}")
}

/// The alert rules of the streaming metrics for the effective metric prefix `prefix` (for
/// example `mini_chat`; `MetricsConfig::effective_prefix` resolves an empty configured value).
#[must_use]
pub fn alert_rules(prefix: &str) -> Vec<AlertRule> {
    let five_minutes = Duration::from_mins(5);
    let fifteen_minutes = Duration::from_mins(15);
    let mut rules = Vec::with_capacity(3);

    // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-admin-observability:p1:inst-obs-latency-alert
    // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-admin-observability:p1:inst-obs-raise-latency
    rules.push(AlertRule {
        name: "MiniChatTtftOverheadHigh".to_owned(),
        expr: p99_above(&format!("{prefix}_ttft_overhead_ms"), 50),
        for_duration: five_minutes,
        severity: AlertSeverity::Warning,
    });
    rules.push(AlertRule {
        name: "MiniChatTimeToAbortHigh".to_owned(),
        expr: p99_above(&format!("{prefix}_time_to_abort_ms"), 200),
        for_duration: five_minutes,
        severity: AlertSeverity::Critical,
    });
    // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-admin-observability:p1:inst-obs-raise-latency
    // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-admin-observability:p1:inst-obs-latency-alert

    // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-admin-observability:p1:inst-obs-orphan-alert
    // Detected rises over 15 minutes while finalized does not. The condition is absence
    // tolerant: a counter series exists only after its first add, so a stall that never
    // finalized anything leaves the finalized series absent, and `detected and (finalized
    // == 0)` would then be empty. `unless` keeps the detected side when the finalized side
    // is empty, and `sum` aggregates away the per-instance labels so the two sides match.
    // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-admin-observability:p1:inst-obs-raise-orphan
    rules.push(AlertRule {
        name: "MiniChatOrphanWatchdogStalled".to_owned(),
        expr: format!(
            "(sum(rate({prefix}_orphan_detected_total{{reason=\"stale_progress\"}}[15m])) > 0) \
             unless \
             (sum(rate({prefix}_orphan_finalized_total{{reason=\"stale_progress\"}}[15m])) > 0)"
        ),
        for_duration: fifteen_minutes,
        severity: AlertSeverity::Critical,
    });
    // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-admin-observability:p1:inst-obs-raise-orphan
    // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-admin-observability:p1:inst-obs-orphan-alert

    // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-admin-observability:p1:inst-obs-return
    // The aggregated view: every expression groups by `le` at most and matches on `reason`
    // only, never on `tenant_id`, `user_id`, `chat_id`, `request_id`, `provider_response_id`
    // or free-form error text.
    rules
    // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-admin-observability:p1:inst-obs-return
}

#[cfg(test)]
#[path = "alerts_tests.rs"]
mod alerts_tests;
