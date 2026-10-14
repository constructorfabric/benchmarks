//! The label sets of every streaming series that the gear records, over the in-memory OTLP
//! pipeline: each helper of the streaming metrics is called with realistic values and the exported
//! points carry only low-cardinality label keys, never `tenant_id`, `user_id`, `chat_id`,
//! `request_id`, `provider_response_id` or free-form error text, under the default and a custom
//! `metrics.prefix`.
//!
//! Slice S3-completion of `cpt-cf-mini-chat-feature-message-streaming`: flow
//! `cpt-cf-mini-chat-flow-message-streaming-admin-observability` (`inst-obs-read`,
//! `inst-obs-return`); algorithm `cpt-cf-mini-chat-algo-message-streaming-streaming-metrics`
//! (`inst-met-cardinality`, `inst-met-return`); definition of done
//! `cpt-cf-mini-chat-dod-message-streaming-observability`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::time::Duration;

use uuid::Uuid;

use crate::domain::quota::{PeriodType, QuotaMetrics};
use crate::test_support::s2::{MetricsHarness, metrics_harness};

const ALLOWED_LABELS: [&str; 16] = [
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
];

const FORBIDDEN_LABELS: [&str; 8] = [
    "tenant_id",
    "user_id",
    "chat_id",
    "request_id",
    "provider_response_id",
    "message_id",
    "turn_id",
    "error_message",
];

/// The streaming series (names without the prefix) that every run of the helpers must record.
const STREAMING_SERIES: [&str; 18] = [
    "stream_started",
    "stream_completed",
    "stream_incomplete",
    "stream_failed",
    "stream_disconnected",
    "active_streams",
    "ttft_provider_ms",
    "ttft_overhead_ms",
    "stream_total_latency_ms",
    "cancel_requested",
    "cancel_effective",
    "time_to_abort_ms",
    "streams_aborted",
    "orphan_detected",
    "orphan_finalized",
    "orphan_scan_duration_seconds",
    "finalization_latency_ms",
    "image_inputs_per_turn",
];

/// Record every streaming series once, the way the provider task, the finalizer, the relay and
/// the orphan watchdog do.
fn record_every_streaming_series(harness: &MetricsHarness) {
    let metrics = &harness.metrics;
    let ms = Duration::from_millis;
    metrics.stream_started("azure_openai", "gpt-5.2");
    metrics.stream_completed("azure_openai", "gpt-5.2");
    metrics.stream_incomplete("azure_openai", "gpt-5.2", "max_output_tokens");
    metrics.stream_failed("azure_openai", "gpt-5.2", "provider_error");
    metrics.stream_failed("azure_openai", "gpt-5.2", "web_search_calls_exceeded");
    metrics.stream_disconnected("before_first_token");
    metrics.stream_disconnected("mid_stream");
    metrics.active_streams(1);
    metrics.active_streams(-1);
    metrics.ttft_provider("azure_openai", "gpt-5.2", ms(120));
    metrics.ttft_overhead("azure_openai", "gpt-5.2", ms(3));
    metrics.stream_total_latency("azure_openai", "gpt-5.2", ms(900));
    metrics.cancel_requested();
    metrics.cancel_effective();
    metrics.time_to_abort(ms(40));
    metrics.streams_aborted("client_disconnect");
    metrics.streams_aborted("orphan_timeout");
    metrics.image_inputs(3);
    metrics.finalization_latency(ms(12));
    metrics.thread_summary_trigger("scheduled");
    metrics.thread_summary_trigger("not_needed");
    metrics.orphan_detected();
    metrics.orphan_finalized();
    metrics.orphan_scan_duration(ms(250));
    // the quota series of the same turn
    metrics.preflight("allow", "gpt-5.2", "premium");
    metrics.reserve(PeriodType::Daily);
    metrics.commit(PeriodType::Monthly);
    metrics.overshoot(PeriodType::Daily);
    metrics.estimated_tokens(1000);
    metrics.actual_tokens(900);
    metrics.code_interpreter_calls("gpt-5.2", 1);
}

#[test]
fn every_recorded_label_key_is_low_cardinality_and_no_identifier_is_a_label() {
    for prefix in ["mini_chat", "acme_chat"] {
        let harness = metrics_harness(prefix);
        record_every_streaming_series(&harness);
        let points = harness.all_points();
        assert!(!points.is_empty());
        for (name, attributes, _) in points {
            assert!(
                name.starts_with(&format!("{prefix}_")),
                "`{name}` does not use the prefix `{prefix}`"
            );
            for (key, value) in &attributes {
                assert!(
                    ALLOWED_LABELS.contains(&key.as_str()),
                    "label `{key}` on {name} is outside the low-cardinality set"
                );
                assert!(!FORBIDDEN_LABELS.contains(&key.as_str()), "{name}: {key}");
                assert!(
                    Uuid::parse_str(value).is_err(),
                    "an identifier is the value of `{key}` on {name}"
                );
                for marker in ["resp_", "req_", "file-", "vs_", "http", "sk-", "Bearer"] {
                    assert!(
                        !value.contains(marker),
                        "{name}: `{key}` carries provider or free-form text: {value}"
                    );
                }
                assert!(
                    value.len() <= 40,
                    "{name}: `{key}` looks free-form: {value}"
                );
            }
        }
    }
}

#[test]
fn every_streaming_series_is_recorded_under_the_configured_prefix() {
    for prefix in ["mini_chat", "acme_chat"] {
        let harness = metrics_harness(prefix);
        record_every_streaming_series(&harness);
        let recorded = harness.recorded();
        for name in STREAMING_SERIES {
            assert!(
                recorded.contains(name),
                "`{prefix}_{name}` was not recorded"
            );
        }
        // nothing was recorded under another prefix
        for (name, _, _) in harness.all_points() {
            assert!(name.starts_with(&format!("{prefix}_")), "{name}");
        }
    }
}

#[test]
fn the_labelled_streaming_series_carry_exactly_their_specified_label_keys() {
    let harness = metrics_harness("mini_chat");
    record_every_streaming_series(&harness);
    let keys_of = |name: &str| -> BTreeSet<String> {
        let series = harness.series(name);
        assert!(!series.is_empty(), "{name} was not recorded");
        series
            .into_iter()
            .flat_map(|(attributes, _)| attributes.into_keys())
            .collect()
    };
    let set =
        |keys: &[&str]| -> BTreeSet<String> { keys.iter().map(|k| (*k).to_owned()).collect() };
    for name in ["stream_started", "stream_completed"] {
        assert_eq!(keys_of(name), set(&["provider", "model"]), "{name}");
    }
    assert_eq!(
        keys_of("stream_incomplete"),
        set(&["provider", "model", "reason"])
    );
    assert_eq!(
        keys_of("stream_failed"),
        set(&["provider", "model", "error_code"])
    );
    assert_eq!(keys_of("stream_disconnected"), set(&["stage"]));
    for name in [
        "ttft_provider_ms",
        "ttft_overhead_ms",
        "stream_total_latency_ms",
    ] {
        assert_eq!(keys_of(name), set(&["provider", "model"]), "{name}");
    }
    for name in [
        "cancel_requested",
        "cancel_effective",
        "time_to_abort_ms",
        "streams_aborted",
    ] {
        assert_eq!(keys_of(name), set(&["trigger"]), "{name}");
    }
    for name in ["orphan_detected", "orphan_finalized"] {
        assert_eq!(keys_of(name), set(&["reason"]), "{name}");
    }
    assert_eq!(keys_of("thread_summary_trigger"), set(&["result"]));
    for name in [
        "active_streams",
        "orphan_scan_duration_seconds",
        "finalization_latency_ms",
        "image_inputs_per_turn",
    ] {
        assert!(
            harness.series(name).iter().all(|(a, _)| a.is_empty()),
            "{name}"
        );
    }
}

#[test]
fn the_orphan_series_carry_the_label_value_that_the_orphan_alert_matches() {
    let harness = metrics_harness("mini_chat");
    harness.metrics.orphan_detected();
    harness.metrics.orphan_finalized();
    assert_eq!(
        harness.total_where("orphan_detected", "reason", "stale_progress"),
        1
    );
    assert_eq!(
        harness.total_where("orphan_finalized", "reason", "stale_progress"),
        1
    );
}

#[test]
fn nothing_is_recorded_for_the_declared_but_unrecorded_streaming_series() {
    let harness = metrics_harness("mini_chat");
    record_every_streaming_series(&harness);
    let recorded = harness.recorded();
    for never in [
        "stream_replay",
        "tool_calls",
        "tool_call_limited",
        "web_search_latency_ms",
        "citations_count",
        "citations_by_source",
        "context_truncation",
        "provider_requests",
        "provider_errors",
        "provider_latency_ms",
        "tokens_after_cancel",
        "time_from_ui_disconnect_to_cancel_ms",
        "cancel_orphan",
    ] {
        assert!(!recorded.contains(never), "`{never}` was recorded");
    }
}
