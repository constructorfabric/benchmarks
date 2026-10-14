//! Observability tests over an in-memory OTLP pipeline (`opentelemetry_sdk` testing
//! exporter): instrument names under the default and a custom prefix, the declared and
//! undefined series, the label rules and the `mini_chat_audit_emit` counter.
//!
//! Slice S2: algorithm `observability`, flow `operator-observability`, definition of done
//! `cpt-cf-mini-chat-dod-message-streaming-observability`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;

use super::{AuditEmitResult, current_trace_id};
use crate::config::MetricsConfig;
use crate::test_support::s2::metrics_harness;

/// The allowed label keys (DESIGN 4, `Naming and cardinality rules`).
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

/// The `Emitted` table of DESIGN 4 (names without the prefix).
const EMITTED: [&str; 46] = [
    "stream_started",
    "stream_completed",
    "stream_failed",
    "stream_incomplete",
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
    "attachment_upload_abandoned",
    "upload_reaper_scan_duration_seconds",
    "attachment_background_indexing",
    "quota_preflight",
    "quota_reserve",
    "quota_commit",
    "quota_overshoot",
    "quota_estimated_tokens",
    "quota_actual_tokens",
    "code_interpreter_calls",
    "knowledge_search",
    "knowledge_search_latency_ms",
    "knowledge_search_chunks",
    "thread_summary_trigger",
    "thread_summary_execution",
    "thread_summary_cas_conflicts",
    "summary_fallback",
    "turn_mutation",
    "turn_mutation_latency_ms",
    "attachment_upload",
    "attachment_upload_bytes",
    "attachments_pending",
    "image_inputs_per_turn",
    "cleanup_completed",
    "cleanup_failed",
    "cleanup_retry",
    "cleanup_vector_store_with_failed_attachments",
    "secondary_cleanup_skipped",
    "audit_emit",
    "finalization_latency_ms",
];

/// `Declared but not recorded (deferred)` (names without the prefix).
const DECLARED_NOT_RECORDED: [&str; 48] = [
    "stream_replay",
    "quota_preflight_v2",
    "quota_tier_downgrade",
    "quota_negative",
    "quota_overshoot_tokens",
    "quota_image_commit",
    "tokens_after_cancel",
    "time_from_ui_disconnect_to_cancel_ms",
    "cancel_orphan",
    "tool_calls",
    "tool_call_limited",
    "file_search_latency_ms",
    "web_search_latency_ms",
    "web_search_disabled",
    "citations_count",
    "citations_by_source",
    "retrieval_latency_ms",
    "retrieval_chunks_returned",
    "retrieval_zero_hit",
    "indexed_chunks_per_chat",
    "upload_rejected",
    "vector_stores_per_user",
    "context_truncation",
    "provider_requests",
    "provider_errors",
    "provider_latency_ms",
    "oagw_retries",
    "oagw_upstream_latency_ms",
    "oagw_circuit_open",
    "attachment_index",
    "attachment_summary",
    "attachments_failed",
    "attachment_index_latency_ms",
    "image_turns",
    "media_rejected",
    "cleanup_backlog",
    "audit_redaction_hits",
    "summary_regen",
    "outbox_enqueue",
    "outbox_dispatch",
    "outbox_dead",
    "outbox_dead_rows",
    "outbox_pending_age_seconds",
    "outbox_oldest_pending_age_seconds",
    "db_query_latency_ms",
    "db_errors",
    "unknown_error_code",
    "credits_overflow",
];

/// `Not defined`: series of earlier revisions that have no instrument.
const NOT_DEFINED: [&str; 7] = [
    "quota_reserved_tokens",
    "quota_overshoot_exceeded_total",
    "code_interpreter_disabled_total",
    "cleanup_latency_ms",
    "outbox_handler_total",
    "outbox_dead_letter_backlog",
    "outbox_dead_letter_oldest_age_seconds",
];

fn expected_names(prefix: &str) -> BTreeSet<String> {
    EMITTED
        .iter()
        .chain(DECLARED_NOT_RECORDED.iter())
        .map(|name| format!("{prefix}_{name}"))
        .collect()
}

// ── prefix ───────────────────────────────────────────────────────────────────

#[test]
fn an_empty_metrics_prefix_means_mini_chat() {
    assert_eq!(MetricsConfig::default().effective_prefix(), "mini_chat");
}

#[test]
fn the_instruments_exist_under_the_default_prefix() {
    let harness = metrics_harness(&MetricsConfig::default().effective_prefix());
    let names: BTreeSet<String> = harness.metrics.instrument_names().into_iter().collect();
    assert_eq!(names, expected_names("mini_chat"));
}

#[test]
fn the_instruments_exist_under_a_custom_prefix_and_none_keeps_the_default_one() {
    let prefix = MetricsConfig {
        prefix: "acme_chat".to_owned(),
    }
    .effective_prefix();
    assert_eq!(prefix, "acme_chat");
    let harness = metrics_harness(&prefix);
    let names: BTreeSet<String> = harness.metrics.instrument_names().into_iter().collect();
    assert_eq!(names, expected_names("acme_chat"));
    assert!(names.iter().all(|name| !name.starts_with("mini_chat_")));
}

#[test]
fn no_series_of_the_not_defined_list_and_no_mcp_series_is_registered() {
    let harness = metrics_harness("mini_chat");
    let names = harness.metrics.instrument_names();
    for undefined in NOT_DEFINED {
        let full = format!("mini_chat_{undefined}");
        assert!(!names.contains(&full), "{full} must not be defined");
    }
    assert!(names.iter().all(|name| !name.contains("mcp")), "{names:?}");
}

#[test]
fn instrument_names_are_unique_and_carry_no_total_suffix() {
    let harness = metrics_harness("mini_chat");
    let names = harness.metrics.instrument_names();
    let unique: BTreeSet<&String> = names.iter().collect();
    assert_eq!(unique.len(), names.len(), "duplicate instrument names");
    assert!(
        names.iter().all(|name| !name.ends_with("_total")),
        "whether `_total` is appended is decided downstream, not by the gear"
    );
}

// ── mini_chat_audit_emit ─────────────────────────────────────────────────────

#[test]
fn audit_emit_counts_each_result_under_the_result_label_only() {
    let harness = metrics_harness("mini_chat");
    for (result, times) in [
        (AuditEmitResult::Ok, 3),
        (AuditEmitResult::Retry, 2),
        (AuditEmitResult::Reject, 1),
        (AuditEmitResult::Dropped, 4),
    ] {
        for _ in 0..times {
            harness.metrics.record_audit_emit(result);
        }
    }
    assert_eq!(harness.audit_emit("ok"), 3);
    assert_eq!(harness.audit_emit("retry"), 2);
    assert_eq!(harness.audit_emit("reject"), 1);
    assert_eq!(harness.audit_emit("dropped"), 4);
    assert_eq!(harness.audit_emit_total(), 10);

    for (attributes, _) in harness.counter_points("mini_chat_audit_emit") {
        let keys: Vec<&str> = attributes.keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["result"], "only the `result` label is attached");
    }
}

#[test]
fn audit_emit_is_exported_under_the_configured_prefix() {
    let harness = metrics_harness("acme_chat");
    harness.metrics.record_audit_emit(AuditEmitResult::Dropped);
    assert_eq!(harness.audit_emit("dropped"), 1);
    assert!(harness.counter_points("mini_chat_audit_emit").is_empty());
    assert_eq!(harness.counter_points("acme_chat_audit_emit").len(), 1);
}

#[test]
fn nothing_is_recorded_until_a_delivery_outcome_is_reported() {
    let harness = metrics_harness("mini_chat");
    assert_eq!(harness.audit_emit_total(), 0, "no counter at enqueue time");
}

#[test]
fn every_exported_label_key_is_in_the_allowed_low_cardinality_set() {
    let harness = metrics_harness("mini_chat");
    harness.metrics.record_audit_emit(AuditEmitResult::Ok);
    harness.metrics.record_audit_emit(AuditEmitResult::Retry);
    for (attributes, _) in harness.counter_points("mini_chat_audit_emit") {
        for key in attributes.keys() {
            assert!(
                ALLOWED_LABELS.contains(&key.as_str()),
                "forbidden label `{key}`"
            );
        }
        for forbidden in [
            "tenant_id",
            "user_id",
            "chat_id",
            "request_id",
            "provider_response_id",
            "filename",
        ] {
            assert!(!attributes.contains_key(forbidden), "{forbidden}");
        }
    }
}

// ── correlation ──────────────────────────────────────────────────────────────

#[test]
fn the_trace_id_is_absent_when_no_span_is_active() {
    assert_eq!(current_trace_id(), None);
}

// ── the orphan finalized series ──────────────────────────────────────────────

#[test]
fn the_orphan_finalized_series_exists_at_zero_before_any_finalization() {
    let harness = metrics_harness("mini_chat");
    let points = harness.counter_points("mini_chat_orphan_finalized");
    assert_eq!(points.len(), 1, "{points:?}");
    assert_eq!(
        points[0].0.get("reason").map(String::as_str),
        Some("stale_progress")
    );
    assert_eq!(points[0].1, 0);
    // the first finalization adds to the same series
    harness.metrics.orphan_finalized();
    let points = harness.counter_points("mini_chat_orphan_finalized");
    assert_eq!(points.len(), 1, "{points:?}");
    assert_eq!(points[0].1, 1);
}
