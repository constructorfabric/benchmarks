//! OpenTelemetry instruments of the gear (DESIGN section 4, "Metric series").
//!
//! Instrument names are `{prefix}_{name}` where the prefix is `metrics.prefix` (default
//! `mini_chat`). Counters carry no `_total` suffix; the OTLP-to-Prometheus conversion adds it.
//! Labels are limited to the low-cardinality keys listed in DESIGN (no tenant/user/chat ids).

use opentelemetry::metrics::{Counter, Histogram, Meter, UpDownCounter};

/// Default metric name prefix.
pub const DEFAULT_PREFIX: &str = "mini_chat";

/// Declares the `Metrics` struct (one public field per instrument) and its constructor.
macro_rules! instruments {
    (
        counters { $($counter:ident),* $(,)? }
        histograms { $($histogram:ident),* $(,)? }
        up_down_counters { $($up_down:ident),* $(,)? }
    ) => {
        /// All instruments of the gear. Fields are named after the instrument (without prefix).
        #[derive(Debug, Clone)]
        pub struct Metrics {
            $(pub $counter: Counter<u64>,)*
            $(pub $histogram: Histogram<f64>,)*
            $(pub $up_down: UpDownCounter<i64>,)*
        }

        impl Metrics {
            /// Registers every instrument on `meter` under `{prefix}_{name}`.
            #[must_use]
            pub fn with_meter(meter: &Meter, prefix: &str) -> Self {
                let prefix = resolve_prefix(prefix);
                Self {
                    $($counter: meter
                        .u64_counter(format!("{prefix}_{}", stringify!($counter)))
                        .build(),)*
                    $($histogram: meter
                        .f64_histogram(format!("{prefix}_{}", stringify!($histogram)))
                        .build(),)*
                    $($up_down: meter
                        .i64_up_down_counter(format!("{prefix}_{}", stringify!($up_down)))
                        .build(),)*
                }
            }
        }
    };
}

instruments! {
    counters {
        // Emitted.
        stream_started,
        stream_completed,
        stream_failed,
        stream_incomplete,
        stream_disconnected,
        cancel_requested,
        cancel_effective,
        streams_aborted,
        orphan_detected,
        orphan_finalized,
        attachment_upload_abandoned,
        attachment_background_indexing,
        quota_preflight,
        quota_reserve,
        quota_commit,
        quota_overshoot,
        code_interpreter_calls,
        knowledge_search,
        thread_summary_trigger,
        thread_summary_execution,
        thread_summary_cas_conflicts,
        summary_fallback,
        turn_mutation,
        attachment_upload,
        cleanup_completed,
        cleanup_failed,
        cleanup_retry,
        cleanup_vector_store_with_failed_attachments,
        secondary_cleanup_skipped,
        audit_emit,
        // Declared, not recorded (deferred).
        stream_replay,
        quota_preflight_v2,
        quota_tier_downgrade,
        quota_negative,
        quota_image_commit,
        cancel_orphan,
        tool_calls,
        tool_call_limited,
        web_search_disabled,
        citations_by_source,
        retrieval_zero_hit,
        upload_rejected,
        context_truncation,
        provider_requests,
        provider_errors,
        oagw_retries,
        oagw_circuit_open,
        attachment_index,
        attachment_summary,
        attachments_failed,
        image_turns,
        media_rejected,
        audit_redaction_hits,
        summary_regen,
        outbox_enqueue,
        outbox_dispatch,
        outbox_dead,
        db_errors,
        unknown_error_code,
        credits_overflow,
    }
    histograms {
        // Emitted.
        ttft_provider_ms,
        ttft_overhead_ms,
        stream_total_latency_ms,
        time_to_abort_ms,
        orphan_scan_duration_seconds,
        upload_reaper_scan_duration_seconds,
        quota_estimated_tokens,
        quota_actual_tokens,
        knowledge_search_latency_ms,
        knowledge_search_chunks,
        turn_mutation_latency_ms,
        attachment_upload_bytes,
        image_inputs_per_turn,
        finalization_latency_ms,
        // Declared, not recorded (deferred).
        quota_overshoot_tokens,
        tokens_after_cancel,
        time_from_ui_disconnect_to_cancel_ms,
        file_search_latency_ms,
        web_search_latency_ms,
        citations_count,
        retrieval_latency_ms,
        retrieval_chunks_returned,
        indexed_chunks_per_chat,
        vector_stores_per_user,
        provider_latency_ms,
        oagw_upstream_latency_ms,
        attachment_index_latency_ms,
        outbox_pending_age_seconds,
        outbox_oldest_pending_age_seconds,
        db_query_latency_ms,
    }
    up_down_counters {
        // Emitted.
        active_streams,
        attachments_pending,
        // Declared, not recorded (deferred): gauges.
        cleanup_backlog,
        outbox_dead_rows,
    }
}

impl Metrics {
    /// Registers every instrument on the global `mini_chat` meter. An empty `prefix` selects
    /// [`DEFAULT_PREFIX`].
    #[must_use]
    pub fn new(prefix: &str) -> Self {
        Self::with_meter(&opentelemetry::global::meter("mini_chat"), prefix)
    }
}

fn resolve_prefix(prefix: &str) -> &str {
    let prefix = prefix.trim().trim_end_matches('_');
    if prefix.is_empty() {
        DEFAULT_PREFIX
    } else {
        prefix
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::KeyValue;
    use opentelemetry_sdk::metrics::data::{ResourceMetrics, ScopeMetrics};
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

    fn exported_names(prefix: &str) -> Vec<String> {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .build();
        let m = Metrics::with_meter(
            &opentelemetry::metrics::MeterProvider::meter(&provider, "t"),
            prefix,
        );
        m.stream_started.add(1, &[KeyValue::new("provider", "p")]);
        m.ttft_provider_ms.record(1.0, &[]);
        m.active_streams.add(1, &[]);
        m.credits_overflow.add(1, &[]);
        m.cleanup_backlog.add(1, &[]);
        provider.force_flush().unwrap();
        exporter
            .get_finished_metrics()
            .unwrap()
            .iter()
            .flat_map(ResourceMetrics::scope_metrics)
            .flat_map(ScopeMetrics::metrics)
            .map(|m| m.name().to_owned())
            .collect()
    }

    #[test]
    fn default_prefix_is_mini_chat() {
        let names = exported_names("");
        for expected in [
            "mini_chat_stream_started",
            "mini_chat_ttft_provider_ms",
            "mini_chat_active_streams",
            "mini_chat_credits_overflow",
            "mini_chat_cleanup_backlog",
        ] {
            assert!(
                names.iter().any(|n| n == expected),
                "{expected} missing in {names:?}"
            );
        }
    }

    #[test]
    fn custom_prefix_replaces_default() {
        let names = exported_names("acme_");
        assert!(
            names.iter().any(|n| n == "acme_stream_started"),
            "{names:?}"
        );
        assert!(
            !names.iter().any(|n| n.starts_with("mini_chat_")),
            "{names:?}"
        );
    }

    #[test]
    fn new_uses_global_meter_without_panicking() {
        let metrics = Metrics::new("");
        metrics.stream_started.add(1, &[]);
    }
}
