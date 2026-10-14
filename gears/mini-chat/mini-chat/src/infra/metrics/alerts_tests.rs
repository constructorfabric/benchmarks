//! The alert-rule catalogue of the streaming metrics: the three alert conditions of DESIGN 4
//! `Alerting and SLO contract (P1)` that this feature owns (thresholds, windows, severities),
//! their metric names under the configured prefix, and the low-cardinality label rule.
//!
//! Slice S3-completion of `cpt-cf-mini-chat-feature-message-streaming`: flow
//! `cpt-cf-mini-chat-flow-message-streaming-admin-observability` (`inst-obs-latency-alert`,
//! `inst-obs-raise-latency`, `inst-obs-orphan-alert`, `inst-obs-raise-orphan`, `inst-obs-return`);
//! algorithm `cpt-cf-mini-chat-algo-message-streaming-streaming-metrics`; definition of done
//! `cpt-cf-mini-chat-dod-message-streaming-observability`.
//!
//! The orphan-watchdog rule must be absence tolerant: the finalized counter series is exported
//! only after its first add, so the rule never relies on `== 0` of a possibly-absent series.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::time::Duration;

use super::{AlertRule, AlertSeverity, alert_rules};

const PREFIX: &str = "mini_chat";

/// The label keys allowed by DESIGN 4 `Naming and cardinality rules`, plus `le`, the bucket
/// boundary that `histogram_quantile` needs.
const ALLOWED_LABELS: [&str; 17] = [
    "provider",
    "model",
    "provider_kind",
    "resource_type",
    "kind",
    "tier",
    "state",
    "stage",
    "op",
    "reason",
    "error_code",
    "decision",
    "period",
    "trigger",
    "result",
    "from_status",
    "le",
];

/// The identifiers that must never be a label.
const FORBIDDEN_LABELS: [&str; 6] = [
    "tenant_id",
    "user_id",
    "chat_id",
    "request_id",
    "provider_response_id",
    "error_message",
];

/// The series of the declared-but-unrecorded list: an alert on one of them cannot fire.
const UNRECORDED: [&str; 10] = [
    "stream_replay",
    "tool_calls",
    "tool_call_limited",
    "web_search_latency_ms",
    "citations_count",
    "provider_errors",
    "provider_requests",
    "tokens_after_cancel",
    "time_from_ui_disconnect_to_cancel_ms",
    "cancel_orphan",
];

fn minutes(count: u64) -> Duration {
    Duration::from_secs(count * 60)
}

/// The rules whose expression reads the series `{prefix}_{name}`.
fn rules_on<'a>(rules: &'a [AlertRule], prefix: &str, name: &str) -> Vec<&'a AlertRule> {
    let series = format!("{prefix}_{name}");
    rules
        .iter()
        .filter(|rule| rule.expr.contains(&series))
        .collect()
}

/// The only rule reading `{prefix}_{name}`; fails when there is none or more than one.
fn only_rule_on<'a>(rules: &'a [AlertRule], prefix: &str, name: &str) -> &'a AlertRule {
    let found = rules_on(rules, prefix, name);
    assert_eq!(
        found.len(),
        1,
        "exactly one rule must read `{prefix}_{name}`: {rules:#?}"
    );
    found[0]
}

/// Whether `expr` ends in the comparison `> {threshold}` (the number is not a prefix of a
/// larger one, `> 50` is not `> 500`).
fn compares_above(expr: &str, threshold: &str) -> bool {
    let needle = format!("> {threshold}");
    expr.match_indices(&needle).any(|(at, _)| {
        let rest = &expr[at + needle.len()..];
        !rest.starts_with(|c: char| c.is_ascii_digit())
            && rest
                .trim_start_matches(".0")
                .chars()
                .next()
                .is_none_or(|c| !c.is_ascii_digit())
    })
}

/// The label keys an expression uses: matcher keys inside `{...}` and the keys of a
/// `by (...)` or `without (...)` clause.
fn label_keys(expr: &str) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    let mut rest = expr;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        let close = after.find('}').expect("an unclosed label matcher");
        for matcher in after[..close].split(',') {
            let key: String = matcher
                .trim()
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !key.is_empty() {
                keys.insert(key);
            }
        }
        rest = &after[close + 1..];
    }
    for keyword in ["by", "without"] {
        let mut rest = expr;
        while let Some(at) = rest.find(&format!("{keyword} (")) {
            let after = &rest[at + keyword.len() + 2..];
            let close = after.find(')').expect("an unclosed grouping clause");
            for key in after[..close].split(',') {
                let key = key.trim();
                if !key.is_empty() {
                    keys.insert(key.to_owned());
                }
            }
            rest = &after[close + 1..];
        }
    }
    keys
}

// ── the catalogue ────────────────────────────────────────────────────────────

#[test]
fn the_catalogue_names_every_rule_once() {
    let rules = alert_rules(PREFIX);
    assert!(rules.len() >= 3, "the three streaming alerts: {rules:#?}");
    let names: BTreeSet<&str> = rules.iter().map(|rule| rule.name.as_str()).collect();
    assert_eq!(names.len(), rules.len(), "duplicate rule names: {rules:#?}");
    for rule in &rules {
        assert!(!rule.name.trim().is_empty(), "{rule:?}");
        assert!(!rule.expr.trim().is_empty(), "{rule:?}");
        assert!(
            rule.name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "a rule name is a plain identifier: {}",
            rule.name
        );
    }
}

#[test]
fn the_catalogue_is_deterministic() {
    assert!(!alert_rules(PREFIX).is_empty(), "an empty catalogue");
    assert_eq!(alert_rules(PREFIX), alert_rules(PREFIX));
}

// ── inst-obs-latency-alert / inst-obs-raise-latency ──────────────────────────

#[test]
fn ttft_overhead_p99_above_50_ms_for_5_minutes_is_a_warning() {
    let rules = alert_rules(PREFIX);
    let rule = only_rule_on(&rules, PREFIX, "ttft_overhead_ms");
    assert!(
        rule.expr.contains("histogram_quantile(0.99"),
        "{}",
        rule.expr
    );
    assert!(compares_above(&rule.expr, "50"), "{}", rule.expr);
    assert_eq!(rule.for_duration, minutes(5));
    assert_eq!(rule.severity, AlertSeverity::Warning);
}

#[test]
fn time_to_abort_p99_above_200_ms_for_5_minutes_is_critical() {
    let rules = alert_rules(PREFIX);
    let rule = only_rule_on(&rules, PREFIX, "time_to_abort_ms");
    assert!(
        rule.expr.contains("histogram_quantile(0.99"),
        "{}",
        rule.expr
    );
    assert!(compares_above(&rule.expr, "200"), "{}", rule.expr);
    assert_eq!(rule.for_duration, minutes(5));
    assert_eq!(rule.severity, AlertSeverity::Critical);
}

#[test]
fn the_latency_alerts_read_a_histogram_over_a_rate_window() {
    let rules = alert_rules(PREFIX);
    for name in ["ttft_overhead_ms", "time_to_abort_ms"] {
        let rule = only_rule_on(&rules, PREFIX, name);
        assert!(
            rule.expr.contains(&format!("{PREFIX}_{name}_bucket")),
            "p99 reads the histogram buckets: {}",
            rule.expr
        );
        assert!(
            rule.expr.contains("rate(") && rule.expr.contains("[5m]"),
            "a rate over the 5 minute window: {}",
            rule.expr
        );
    }
}

#[test]
fn the_overhead_and_abort_alerts_have_different_severities_and_thresholds() {
    let rules = alert_rules(PREFIX);
    let overhead = only_rule_on(&rules, PREFIX, "ttft_overhead_ms");
    let abort = only_rule_on(&rules, PREFIX, "time_to_abort_ms");
    assert_ne!(overhead.name, abort.name);
    assert_ne!(overhead.severity, abort.severity);
    assert_ne!(overhead.expr, abort.expr);
}

// ── inst-obs-orphan-alert / inst-obs-raise-orphan ────────────────────────────

#[test]
fn orphans_detected_without_any_finalized_over_15_minutes_is_critical() {
    let rules = alert_rules(PREFIX);
    let detected = only_rule_on(&rules, PREFIX, "orphan_detected");
    let finalized = only_rule_on(&rules, PREFIX, "orphan_finalized");
    assert_eq!(detected, finalized, "one rule reads both series");
    assert_eq!(detected.severity, AlertSeverity::Critical);
    assert_eq!(detected.for_duration, minutes(15));
    let expr = &detected.expr;
    // sum(rate(detected[15m])) > 0 UNLESS sum(rate(finalized[15m])) > 0: absence tolerant
    assert!(expr.contains("[15m]"), "{expr}");
    assert!(expr.contains("rate("), "{expr}");
    assert!(expr.contains("> 0"), "{expr}");
    assert!(expr.contains(" unless "), "{expr}");
    assert!(
        !expr.contains("== 0") && !expr.contains(" and "),
        "an absent finalized series would silence the alert: {expr}"
    );
    // both sides are aggregated, so per-instance labels never split the match
    assert_eq!(expr.matches("sum(rate(").count(), 2, "{expr}");
    let detected_at = expr.find("orphan_detected").unwrap();
    let finalized_at = expr.find("orphan_finalized").unwrap();
    assert!(
        detected_at < finalized_at,
        "detected rises, finalized does not: {expr}"
    );
    // the condition is about the stale-progress orphans of the watchdog
    assert_eq!(
        expr.matches("reason=\"stale_progress\"").count(),
        2,
        "{expr}"
    );
}

// ── prefix ───────────────────────────────────────────────────────────────────

#[test]
fn every_metric_name_uses_the_configured_prefix() {
    for prefix in ["mini_chat", "acme_chat"] {
        let rules = alert_rules(prefix);
        assert!(rules.len() >= 3, "{prefix}: {rules:#?}");
        for name in [
            "ttft_overhead_ms",
            "time_to_abort_ms",
            "orphan_detected",
            "orphan_finalized",
        ] {
            assert!(
                !rules_on(&rules, prefix, name).is_empty(),
                "no rule reads `{prefix}_{name}`: {rules:#?}"
            );
        }
        if prefix != "mini_chat" {
            for rule in &rules {
                assert!(
                    !rule.expr.contains("mini_chat"),
                    "the default prefix leaked into {}: {}",
                    rule.name,
                    rule.expr
                );
            }
        }
    }
}

#[test]
fn a_custom_prefix_keeps_the_thresholds_windows_and_severities() {
    let default = alert_rules("mini_chat");
    let custom = alert_rules("acme_chat");
    assert!(default.len() >= 3, "{default:#?}");
    assert_eq!(default.len(), custom.len());
    for (left, right) in default.iter().zip(&custom) {
        assert_eq!(left.name, right.name);
        assert_eq!(left.for_duration, right.for_duration);
        assert_eq!(left.severity, right.severity);
        assert_eq!(left.expr.replace("mini_chat", "acme_chat"), right.expr);
    }
}

// ── inst-obs-return: label cardinality ───────────────────────────────────────

#[test]
fn no_expression_uses_a_high_cardinality_label() {
    for prefix in ["mini_chat", "acme_chat"] {
        let rules = alert_rules(prefix);
        assert!(!rules.is_empty(), "{prefix}: an empty catalogue");
        for rule in &rules {
            for forbidden in FORBIDDEN_LABELS {
                assert!(
                    !rule.expr.contains(forbidden),
                    "{} uses `{forbidden}`: {}",
                    rule.name,
                    rule.expr
                );
            }
            for key in label_keys(&rule.expr) {
                assert!(
                    ALLOWED_LABELS.contains(&key.as_str()),
                    "{} uses the label `{key}` outside the low-cardinality set: {}",
                    rule.name,
                    rule.expr
                );
            }
        }
    }
}

#[test]
fn the_label_scanner_sees_matchers_and_groupings() {
    let keys = label_keys(
        "histogram_quantile(0.99, sum by (le, tenant_id) (rate(x_bucket{reason=\"a\", model!=\"b\"}[5m])))",
    );
    assert_eq!(
        keys,
        BTreeSet::from(["le", "tenant_id", "reason", "model"].map(str::to_owned))
    );
}

// ── declared-but-unrecorded series cannot fire ───────────────────────────────

#[test]
fn no_rule_is_built_on_a_declared_but_unrecorded_series() {
    let rules = alert_rules(PREFIX);
    assert!(!rules.is_empty(), "an empty catalogue");
    for rule in &rules {
        for name in UNRECORDED {
            let series = format!("{PREFIX}_{name}");
            // `{prefix}_cancel_requested` must not be mistaken for a longer name: match on a
            // word boundary
            let hit = rule.expr.match_indices(&series).any(|(at, _)| {
                !rule.expr[at + series.len()..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_')
                    || rule.expr[at + series.len()..].starts_with("_bucket")
                    || rule.expr[at + series.len()..].starts_with("_total")
            });
            assert!(!hit, "{} reads the unrecorded `{series}`", rule.name);
        }
    }
}

#[test]
fn every_series_read_by_a_rule_is_registered_by_the_gear() {
    use crate::test_support::s2::metrics_harness;
    let harness = metrics_harness(PREFIX);
    let registered: BTreeSet<String> = harness.metrics.instrument_names().into_iter().collect();
    let rules = alert_rules(PREFIX);
    assert!(!rules.is_empty(), "an empty catalogue");
    for rule in &rules {
        let used = registered
            .iter()
            .filter(|name| rule.expr.contains(name.as_str()))
            .count();
        assert!(
            used >= 1,
            "{} reads no series registered by the gear: {}",
            rule.name,
            rule.expr
        );
    }
}
