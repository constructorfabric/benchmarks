//! OpenTelemetry instruments of the gear (DESIGN §4 "Metrics").
//!
//! Labels are low-cardinality only (provider, model, tier, period, result,
//! ...); identifiers never appear in labels.

use opentelemetry::metrics::{Counter, Histogram, Meter, UpDownCounter};
use opentelemetry::{InstrumentationScope, KeyValue};

/// Instrumentation scope of the gear.
pub const SCOPE: &str = "cf-gears-mini-chat";

/// The gear's instruments.
#[allow(clippy::struct_field_names)]
pub struct Metrics {
    pub stream_started: Counter<u64>,
    pub stream_completed: Counter<u64>,
    pub stream_failed: Counter<u64>,
    pub stream_incomplete: Counter<u64>,
    pub stream_disconnected: Counter<u64>,
    pub active_streams: UpDownCounter<i64>,
    pub ttft_provider_ms: Histogram<f64>,
    pub ttft_overhead_ms: Histogram<f64>,
    pub stream_total_latency_ms: Histogram<f64>,
    pub cancel_requested: Counter<u64>,
    pub cancel_effective: Counter<u64>,
    pub time_to_abort_ms: Histogram<f64>,
    pub streams_aborted: Counter<u64>,
    pub orphan_detected: Counter<u64>,
    pub orphan_finalized: Counter<u64>,
    pub orphan_scan_duration_seconds: Histogram<f64>,
    pub attachment_upload_abandoned: Counter<u64>,
    pub upload_reaper_scan_duration_seconds: Histogram<f64>,
    pub attachment_background_indexing: Counter<u64>,
    pub quota_preflight: Counter<u64>,
    pub quota_reserve: Counter<u64>,
    pub quota_commit: Counter<u64>,
    pub quota_overshoot: Counter<u64>,
    pub quota_estimated_tokens: Histogram<f64>,
    pub quota_actual_tokens: Histogram<f64>,
    pub code_interpreter_calls: Counter<u64>,
    pub knowledge_search: Counter<u64>,
    pub knowledge_search_latency_ms: Histogram<f64>,
    pub knowledge_search_chunks: Histogram<f64>,
    pub thread_summary_trigger: Counter<u64>,
    pub thread_summary_execution: Counter<u64>,
    pub thread_summary_cas_conflicts: Counter<u64>,
    pub summary_fallback: Counter<u64>,
    pub turn_mutation: Counter<u64>,
    pub turn_mutation_latency_ms: Histogram<f64>,
    pub attachment_upload: Counter<u64>,
    pub attachment_upload_bytes: Histogram<f64>,
    pub attachments_pending: UpDownCounter<i64>,
    pub image_inputs_per_turn: Histogram<f64>,
    pub cleanup_completed: Counter<u64>,
    pub cleanup_failed: Counter<u64>,
    pub cleanup_retry: Counter<u64>,
    pub cleanup_vector_store_with_failed_attachments: Counter<u64>,
    pub secondary_cleanup_skipped: Counter<u64>,
    pub audit_emit: Counter<u64>,
    pub finalization_latency_ms: Histogram<f64>,
}

impl Metrics {
    /// Instruments on the global meter provider.
    #[must_use]
    pub fn global(prefix: &str) -> Self {
        let meter =
            opentelemetry::global::meter_with_scope(InstrumentationScope::builder(SCOPE).build());
        Self::new(&meter, prefix)
    }

    /// Declare every instrument on `meter`.
    #[must_use]
    pub fn new(meter: &Meter, prefix: &str) -> Self {
        let c = |name: &str| meter.u64_counter(format!("{prefix}_{name}")).build();
        let h = |name: &str| meter.f64_histogram(format!("{prefix}_{name}")).build();
        let u = |name: &str| {
            meter
                .i64_up_down_counter(format!("{prefix}_{name}"))
                .build()
        };
        Self {
            stream_started: c("stream_started"),
            stream_completed: c("stream_completed"),
            stream_failed: c("stream_failed"),
            stream_incomplete: c("stream_incomplete"),
            stream_disconnected: c("stream_disconnected"),
            active_streams: u("active_streams"),
            ttft_provider_ms: h("ttft_provider_ms"),
            ttft_overhead_ms: h("ttft_overhead_ms"),
            stream_total_latency_ms: h("stream_total_latency_ms"),
            cancel_requested: c("cancel_requested"),
            cancel_effective: c("cancel_effective"),
            time_to_abort_ms: h("time_to_abort_ms"),
            streams_aborted: c("streams_aborted"),
            orphan_detected: c("orphan_detected"),
            orphan_finalized: c("orphan_finalized"),
            orphan_scan_duration_seconds: h("orphan_scan_duration_seconds"),
            attachment_upload_abandoned: c("attachment_upload_abandoned"),
            upload_reaper_scan_duration_seconds: h("upload_reaper_scan_duration_seconds"),
            attachment_background_indexing: c("attachment_background_indexing"),
            quota_preflight: c("quota_preflight"),
            quota_reserve: c("quota_reserve"),
            quota_commit: c("quota_commit"),
            quota_overshoot: c("quota_overshoot"),
            quota_estimated_tokens: h("quota_estimated_tokens"),
            quota_actual_tokens: h("quota_actual_tokens"),
            code_interpreter_calls: c("code_interpreter_calls"),
            knowledge_search: c("knowledge_search"),
            knowledge_search_latency_ms: h("knowledge_search_latency_ms"),
            knowledge_search_chunks: h("knowledge_search_chunks"),
            thread_summary_trigger: c("thread_summary_trigger"),
            thread_summary_execution: c("thread_summary_execution"),
            thread_summary_cas_conflicts: c("thread_summary_cas_conflicts"),
            summary_fallback: c("summary_fallback"),
            turn_mutation: c("turn_mutation"),
            turn_mutation_latency_ms: h("turn_mutation_latency_ms"),
            attachment_upload: c("attachment_upload"),
            attachment_upload_bytes: h("attachment_upload_bytes"),
            attachments_pending: u("attachments_pending"),
            image_inputs_per_turn: h("image_inputs_per_turn"),
            cleanup_completed: c("cleanup_completed"),
            cleanup_failed: c("cleanup_failed"),
            cleanup_retry: c("cleanup_retry"),
            cleanup_vector_store_with_failed_attachments: c(
                "cleanup_vector_store_with_failed_attachments",
            ),
            secondary_cleanup_skipped: c("secondary_cleanup_skipped"),
            audit_emit: c("audit_emit"),
            finalization_latency_ms: h("finalization_latency_ms"),
        }
    }
}

/// `[(key, value)]` → `Vec<KeyValue>`.
#[must_use]
pub fn labels(pairs: &[(&'static str, &str)]) -> Vec<KeyValue> {
    pairs
        .iter()
        .map(|(k, v)| KeyValue::new(*k, (*v).to_owned()))
        .collect()
}
