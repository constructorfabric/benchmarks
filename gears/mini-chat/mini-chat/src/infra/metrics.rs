//! OpenTelemetry metrics (OTLP export; names use the configurable prefix,
//! default `mini_chat`).
//!
//! Only the emitted P1 series are recorded. Labels never carry
//! high-cardinality identifiers.

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram, Meter, UpDownCounter};

/// The gear metrics (`OTel` instruments).
#[derive(Clone)]
pub struct Metrics {
    /// `stream_started` counter.
    pub stream_started: Counter<u64>,
    /// `stream_completed` counter.
    pub stream_completed: Counter<u64>,
    /// `stream_failed` counter.
    pub stream_failed: Counter<u64>,
    /// `stream_incomplete` counter.
    pub stream_incomplete: Counter<u64>,
    /// `stream_disconnected` counter.
    pub stream_disconnected: Counter<u64>,
    /// `active_streams` up-down counter.
    pub active_streams: UpDownCounter<i64>,
    /// `ttft_provider_ms` histogram.
    pub ttft_provider_ms: Histogram<f64>,
    /// `ttft_overhead_ms` histogram.
    pub ttft_overhead_ms: Histogram<f64>,
    /// `stream_total_latency_ms` histogram.
    pub stream_total_latency_ms: Histogram<f64>,
    /// `cancel_requested` counter.
    pub cancel_requested: Counter<u64>,
    /// `cancel_effective` counter.
    pub cancel_effective: Counter<u64>,
    /// `time_to_abort_ms` histogram.
    pub time_to_abort_ms: Histogram<f64>,
    /// `streams_aborted` counter.
    pub streams_aborted: Counter<u64>,
    /// `orphan_detected` counter.
    pub orphan_detected: Counter<u64>,
    /// `orphan_finalized` counter.
    pub orphan_finalized: Counter<u64>,
    /// `orphan_scan_duration_seconds` histogram.
    pub orphan_scan_duration_seconds: Histogram<f64>,
    /// `attachment_upload_abandoned` counter.
    pub attachment_upload_abandoned: Counter<u64>,
    /// `upload_reaper_scan_duration_seconds` histogram.
    pub upload_reaper_scan_duration_seconds: Histogram<f64>,
    /// `attachment_background_indexing` counter.
    pub attachment_background_indexing: Counter<u64>,
    /// `quota_preflight` counter.
    pub quota_preflight: Counter<u64>,
    /// `quota_reserve` counter.
    pub quota_reserve: Counter<u64>,
    /// `quota_commit` counter.
    pub quota_commit: Counter<u64>,
    /// `quota_overshoot` counter.
    pub quota_overshoot: Counter<u64>,
    /// `quota_estimated_tokens` histogram.
    pub quota_estimated_tokens: Histogram<f64>,
    /// `quota_actual_tokens` histogram.
    pub quota_actual_tokens: Histogram<f64>,
    /// `code_interpreter_calls` counter.
    pub code_interpreter_calls: Counter<u64>,
    /// `knowledge_search` counter.
    pub knowledge_search: Counter<u64>,
    /// `knowledge_search_latency_ms` histogram.
    pub knowledge_search_latency_ms: Histogram<f64>,
    /// `knowledge_search_chunks` histogram.
    pub knowledge_search_chunks: Histogram<f64>,
    /// `thread_summary_trigger` counter.
    pub thread_summary_trigger: Counter<u64>,
    /// `thread_summary_execution` counter.
    pub thread_summary_execution: Counter<u64>,
    /// `thread_summary_cas_conflicts` counter.
    pub thread_summary_cas_conflicts: Counter<u64>,
    /// `summary_fallback` counter.
    pub summary_fallback: Counter<u64>,
    /// `turn_mutation` counter.
    pub turn_mutation: Counter<u64>,
    /// `turn_mutation_latency_ms` histogram.
    pub turn_mutation_latency_ms: Histogram<f64>,
    /// `attachment_upload` counter.
    pub attachment_upload: Counter<u64>,
    /// `attachment_upload_bytes` histogram.
    pub attachment_upload_bytes: Histogram<f64>,
    /// `attachments_pending` up-down counter.
    pub attachments_pending: UpDownCounter<i64>,
    /// `image_inputs_per_turn` histogram.
    pub image_inputs_per_turn: Histogram<f64>,
    /// `cleanup_completed` counter.
    pub cleanup_completed: Counter<u64>,
    /// `cleanup_failed` counter.
    pub cleanup_failed: Counter<u64>,
    /// `cleanup_retry` counter.
    pub cleanup_retry: Counter<u64>,
    /// `cleanup_vector_store_with_failed_attachments` counter.
    pub cleanup_vector_store_with_failed_attachments: Counter<u64>,
    /// `secondary_cleanup_skipped` counter.
    pub secondary_cleanup_skipped: Counter<u64>,
    /// `audit_emit` counter.
    pub audit_emit: Counter<u64>,
    /// `finalization_latency_ms` histogram.
    pub finalization_latency_ms: Histogram<f64>,
}

impl Metrics {
    /// Build all instruments from the global meter with the given prefix
    /// (empty = `mini_chat`).
    #[must_use]
    pub fn new(prefix: &str) -> Self {
        let prefix = if prefix.is_empty() { "mini_chat" } else { prefix };
        let meter: Meter = opentelemetry::global::meter("mini-chat");
        let counter = |name: &str| meter.u64_counter(format!("{prefix}_{name}")).build();
        let histogram =
            |name: &str| meter.f64_histogram(format!("{prefix}_{name}")).build();
        let updown = |name: &str| meter.i64_up_down_counter(format!("{prefix}_{name}")).build();
        Self {
            stream_started: counter("stream_started"),
            stream_completed: counter("stream_completed"),
            stream_failed: counter("stream_failed"),
            stream_incomplete: counter("stream_incomplete"),
            stream_disconnected: counter("stream_disconnected"),
            active_streams: updown("active_streams"),
            ttft_provider_ms: histogram("ttft_provider_ms"),
            ttft_overhead_ms: histogram("ttft_overhead_ms"),
            stream_total_latency_ms: histogram("stream_total_latency_ms"),
            cancel_requested: counter("cancel_requested"),
            cancel_effective: counter("cancel_effective"),
            time_to_abort_ms: histogram("time_to_abort_ms"),
            streams_aborted: counter("streams_aborted"),
            orphan_detected: counter("orphan_detected"),
            orphan_finalized: counter("orphan_finalized"),
            orphan_scan_duration_seconds: histogram("orphan_scan_duration_seconds"),
            attachment_upload_abandoned: counter("attachment_upload_abandoned"),
            upload_reaper_scan_duration_seconds: histogram("upload_reaper_scan_duration_seconds"),
            attachment_background_indexing: counter("attachment_background_indexing"),
            quota_preflight: counter("quota_preflight"),
            quota_reserve: counter("quota_reserve"),
            quota_commit: counter("quota_commit"),
            quota_overshoot: counter("quota_overshoot"),
            quota_estimated_tokens: histogram("quota_estimated_tokens"),
            quota_actual_tokens: histogram("quota_actual_tokens"),
            code_interpreter_calls: counter("code_interpreter_calls"),
            knowledge_search: counter("knowledge_search"),
            knowledge_search_latency_ms: histogram("knowledge_search_latency_ms"),
            knowledge_search_chunks: histogram("knowledge_search_chunks"),
            thread_summary_trigger: counter("thread_summary_trigger"),
            thread_summary_execution: counter("thread_summary_execution"),
            thread_summary_cas_conflicts: counter("thread_summary_cas_conflicts"),
            summary_fallback: counter("summary_fallback"),
            turn_mutation: counter("turn_mutation"),
            turn_mutation_latency_ms: histogram("turn_mutation_latency_ms"),
            attachment_upload: counter("attachment_upload"),
            attachment_upload_bytes: histogram("attachment_upload_bytes"),
            attachments_pending: updown("attachments_pending"),
            image_inputs_per_turn: histogram("image_inputs_per_turn"),
            cleanup_completed: counter("cleanup_completed"),
            cleanup_failed: counter("cleanup_failed"),
            cleanup_retry: counter("cleanup_retry"),
            cleanup_vector_store_with_failed_attachments: counter(
                "cleanup_vector_store_with_failed_attachments",
            ),
            secondary_cleanup_skipped: counter("secondary_cleanup_skipped"),
            audit_emit: counter("audit_emit"),
            finalization_latency_ms: histogram("finalization_latency_ms"),
        }
    }
}

/// Build a `KeyValue` label.
#[must_use]
pub fn label(key: &'static str, value: impl Into<String>) -> KeyValue {
    KeyValue::new(key, value.into())
}
