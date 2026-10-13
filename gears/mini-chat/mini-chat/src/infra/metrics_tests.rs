#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Tests of `cpt-cf-mini-chat-algo-gear-foundation-register-metrics`: the
//! instrument set of DESIGN "Metric series", the prefix and the audit counter.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use opentelemetry::metrics::noop::NoopMeterProvider;
use opentelemetry::metrics::{
    AsyncInstrumentBuilder, Counter, Gauge, Histogram, HistogramBuilder, InstrumentBuilder,
    InstrumentProvider, Meter, MeterProvider, ObservableCounter, ObservableGauge,
    ObservableUpDownCounter, UpDownCounter,
};
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

use super::{
    AuditEmitMetrics, AuditEmitResult, DEFAULT_METRICS_PREFIX, MiniChatMetrics, effective_prefix,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Counter,
    UpDownCounter,
    Gauge,
    Histogram,
}

/// Records the name and kind of every instrument created on its meter.
struct RecordingProvider {
    noop: Meter,
    created: Mutex<Vec<(String, Kind)>>,
}

impl RecordingProvider {
    fn meter() -> (Meter, Arc<Self>) {
        let provider = Arc::new(Self {
            noop: NoopMeterProvider::new().meter("recording"),
            created: Mutex::new(Vec::new()),
        });
        (
            Meter::new(Arc::clone(&provider) as Arc<dyn InstrumentProvider + Send + Sync>),
            provider,
        )
    }

    fn note(&self, name: &str, kind: Kind) {
        self.created.lock().unwrap().push((name.to_owned(), kind));
    }

    fn registered(&self) -> BTreeMap<String, Kind> {
        let created = self.created.lock().unwrap();
        let mut map = BTreeMap::new();
        for (name, kind) in created.iter() {
            assert!(
                map.insert(name.clone(), *kind).is_none(),
                "instrument `{name}` is registered twice"
            );
        }
        map
    }
}

impl InstrumentProvider for RecordingProvider {
    fn u64_counter(&self, builder: InstrumentBuilder<'_, Counter<u64>>) -> Counter<u64> {
        self.note(&builder.name, Kind::Counter);
        self.noop.u64_counter(builder.name).build()
    }

    fn f64_counter(&self, builder: InstrumentBuilder<'_, Counter<f64>>) -> Counter<f64> {
        self.note(&builder.name, Kind::Counter);
        self.noop.f64_counter(builder.name).build()
    }

    fn u64_observable_counter(
        &self,
        builder: AsyncInstrumentBuilder<'_, ObservableCounter<u64>, u64>,
    ) -> ObservableCounter<u64> {
        self.note(&builder.name, Kind::Counter);
        self.noop.u64_observable_counter(builder.name).build()
    }

    fn f64_observable_counter(
        &self,
        builder: AsyncInstrumentBuilder<'_, ObservableCounter<f64>, f64>,
    ) -> ObservableCounter<f64> {
        self.note(&builder.name, Kind::Counter);
        self.noop.f64_observable_counter(builder.name).build()
    }

    fn i64_up_down_counter(
        &self,
        builder: InstrumentBuilder<'_, UpDownCounter<i64>>,
    ) -> UpDownCounter<i64> {
        self.note(&builder.name, Kind::UpDownCounter);
        self.noop.i64_up_down_counter(builder.name).build()
    }

    fn f64_up_down_counter(
        &self,
        builder: InstrumentBuilder<'_, UpDownCounter<f64>>,
    ) -> UpDownCounter<f64> {
        self.note(&builder.name, Kind::UpDownCounter);
        self.noop.f64_up_down_counter(builder.name).build()
    }

    fn i64_observable_up_down_counter(
        &self,
        builder: AsyncInstrumentBuilder<'_, ObservableUpDownCounter<i64>, i64>,
    ) -> ObservableUpDownCounter<i64> {
        self.note(&builder.name, Kind::UpDownCounter);
        self.noop
            .i64_observable_up_down_counter(builder.name)
            .build()
    }

    fn f64_observable_up_down_counter(
        &self,
        builder: AsyncInstrumentBuilder<'_, ObservableUpDownCounter<f64>, f64>,
    ) -> ObservableUpDownCounter<f64> {
        self.note(&builder.name, Kind::UpDownCounter);
        self.noop
            .f64_observable_up_down_counter(builder.name)
            .build()
    }

    fn u64_gauge(&self, builder: InstrumentBuilder<'_, Gauge<u64>>) -> Gauge<u64> {
        self.note(&builder.name, Kind::Gauge);
        self.noop.u64_gauge(builder.name).build()
    }

    fn f64_gauge(&self, builder: InstrumentBuilder<'_, Gauge<f64>>) -> Gauge<f64> {
        self.note(&builder.name, Kind::Gauge);
        self.noop.f64_gauge(builder.name).build()
    }

    fn i64_gauge(&self, builder: InstrumentBuilder<'_, Gauge<i64>>) -> Gauge<i64> {
        self.note(&builder.name, Kind::Gauge);
        self.noop.i64_gauge(builder.name).build()
    }

    fn u64_observable_gauge(
        &self,
        builder: AsyncInstrumentBuilder<'_, ObservableGauge<u64>, u64>,
    ) -> ObservableGauge<u64> {
        self.note(&builder.name, Kind::Gauge);
        self.noop.u64_observable_gauge(builder.name).build()
    }

    fn i64_observable_gauge(
        &self,
        builder: AsyncInstrumentBuilder<'_, ObservableGauge<i64>, i64>,
    ) -> ObservableGauge<i64> {
        self.note(&builder.name, Kind::Gauge);
        self.noop.i64_observable_gauge(builder.name).build()
    }

    fn f64_observable_gauge(
        &self,
        builder: AsyncInstrumentBuilder<'_, ObservableGauge<f64>, f64>,
    ) -> ObservableGauge<f64> {
        self.note(&builder.name, Kind::Gauge);
        self.noop.f64_observable_gauge(builder.name).build()
    }

    fn f64_histogram(&self, builder: HistogramBuilder<'_, Histogram<f64>>) -> Histogram<f64> {
        self.note(&builder.name, Kind::Histogram);
        self.noop.f64_histogram(builder.name).build()
    }

    fn u64_histogram(&self, builder: HistogramBuilder<'_, Histogram<u64>>) -> Histogram<u64> {
        self.note(&builder.name, Kind::Histogram);
        self.noop.u64_histogram(builder.name).build()
    }
}

fn registered(prefix: &str) -> BTreeMap<String, Kind> {
    let (meter, provider) = RecordingProvider::meter();
    let _metrics = MiniChatMetrics::new(&meter, prefix);
    provider.registered()
}

// ---- DESIGN "Metric series": emitted ----------------------------------------------------------

const EMITTED_COUNTERS: &[&str] = &[
    "stream_started",
    "stream_completed",
    "stream_failed",
    "stream_incomplete",
    "stream_disconnected",
    "cancel_requested",
    "cancel_effective",
    "streams_aborted",
    "orphan_detected",
    "orphan_finalized",
    "attachment_upload_abandoned",
    "attachment_background_indexing",
    "quota_preflight",
    "quota_reserve",
    "quota_commit",
    "quota_overshoot",
    "code_interpreter_calls",
    "knowledge_search",
    "thread_summary_trigger",
    "thread_summary_execution",
    "thread_summary_cas_conflicts",
    "summary_fallback",
    "turn_mutation",
    "attachment_upload",
    "cleanup_completed",
    "cleanup_failed",
    "cleanup_retry",
    "cleanup_vector_store_with_failed_attachments",
    "secondary_cleanup_skipped",
    "audit_emit",
];

const EMITTED_UP_DOWN_COUNTERS: &[&str] = &["active_streams", "attachments_pending"];

const EMITTED_HISTOGRAMS: &[&str] = &[
    "ttft_provider_ms",
    "ttft_overhead_ms",
    "stream_total_latency_ms",
    "time_to_abort_ms",
    "orphan_scan_duration_seconds",
    "upload_reaper_scan_duration_seconds",
    "quota_estimated_tokens",
    "quota_actual_tokens",
    "knowledge_search_latency_ms",
    "knowledge_search_chunks",
    "turn_mutation_latency_ms",
    "attachment_upload_bytes",
    "image_inputs_per_turn",
    "finalization_latency_ms",
];

/// "Declared but not recorded (deferred)": registered, type not specified.
const DECLARED_NOT_RECORDED: &[&str] = &[
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

/// "Not defined": series of earlier revisions that have no instrument.
const NOT_DEFINED: &[&str] = &[
    "quota_reserved_tokens",
    "quota_overshoot_exceeded_total",
    "code_interpreter_disabled_total",
    "cleanup_latency_ms",
    "outbox_handler_total",
    "outbox_dead_letter_backlog",
    "outbox_dead_letter_oldest_age_seconds",
];

fn name(prefix: &str, base: &str) -> String {
    format!("{prefix}_{base}")
}

#[test]
fn the_prefix_defaults_to_mini_chat_when_empty() {
    assert_eq!(DEFAULT_METRICS_PREFIX, "mini_chat");
    assert_eq!(effective_prefix(""), "mini_chat");
    assert_eq!(effective_prefix("acme"), "acme");
}

#[test]
fn every_emitted_counter_is_registered_as_a_counter() {
    let registered = registered("");

    for base in EMITTED_COUNTERS {
        assert_eq!(
            registered.get(&name("mini_chat", base)),
            Some(&Kind::Counter),
            "mini_chat_{base}"
        );
    }
}

#[test]
fn the_emitted_up_down_counters_are_registered() {
    let registered = registered("");

    for base in EMITTED_UP_DOWN_COUNTERS {
        assert_eq!(
            registered.get(&name("mini_chat", base)),
            Some(&Kind::UpDownCounter),
            "mini_chat_{base}"
        );
    }
}

#[test]
fn every_emitted_histogram_is_registered_as_a_histogram() {
    let registered = registered("");

    for base in EMITTED_HISTOGRAMS {
        assert_eq!(
            registered.get(&name("mini_chat", base)),
            Some(&Kind::Histogram),
            "mini_chat_{base}"
        );
    }
}

#[test]
fn the_declared_but_not_recorded_instruments_are_registered() {
    let registered = registered("");

    for base in DECLARED_NOT_RECORDED {
        assert!(
            registered.contains_key(&name("mini_chat", base)),
            "mini_chat_{base} is declared in DESIGN and must be registered"
        );
    }
}

#[test]
fn exactly_the_design_instruments_are_registered() {
    let registered = registered("");

    let mut expected: Vec<String> = EMITTED_COUNTERS
        .iter()
        .chain(EMITTED_UP_DOWN_COUNTERS)
        .chain(EMITTED_HISTOGRAMS)
        .chain(DECLARED_NOT_RECORDED)
        .map(|base| name("mini_chat", base))
        .collect();
    expected.sort();

    let actual: Vec<String> = registered.keys().cloned().collect();
    assert_eq!(actual, expected);
}

#[test]
fn no_mcp_instrument_and_no_undefined_series_is_registered() {
    let registered = registered("");

    assert!(registered.contains_key("mini_chat_audit_emit"));
    assert!(
        registered.keys().all(|name| !name.contains("mcp")),
        "{:?}",
        registered
            .keys()
            .filter(|n| n.contains("mcp"))
            .collect::<Vec<_>>()
    );
    for base in NOT_DEFINED {
        assert!(
            !registered.contains_key(&name("mini_chat", base)),
            "mini_chat_{base} has no instrument"
        );
    }
}

#[test]
fn a_custom_prefix_replaces_mini_chat_on_every_instrument() {
    let default = registered("");
    let custom = registered("acme");

    assert_eq!(custom.len(), default.len());
    assert!(
        custom.keys().all(|name| name.starts_with("acme_")),
        "{:?}",
        custom
            .keys()
            .filter(|n| !n.starts_with("acme_"))
            .collect::<Vec<_>>()
    );
    for (name, kind) in &default {
        let renamed = name.replacen("mini_chat_", "acme_", 1);
        assert_eq!(custom.get(&renamed), Some(kind), "{renamed}");
    }
}

#[test]
fn the_audit_counter_is_named_after_the_prefix() {
    assert_eq!(
        registered("").get("mini_chat_audit_emit"),
        Some(&Kind::Counter)
    );
    assert_eq!(
        registered("acme").get("acme_audit_emit"),
        Some(&Kind::Counter)
    );
}

// ---- mini_chat_audit_emit{result} ---------------------------------------------------------------

#[test]
fn audit_result_labels_are_the_documented_values() {
    assert_eq!(AuditEmitResult::Ok.as_str(), "ok");
    assert_eq!(AuditEmitResult::Retry.as_str(), "retry");
    assert_eq!(AuditEmitResult::Reject.as_str(), "reject");
    assert_eq!(AuditEmitResult::Dropped.as_str(), "dropped");
}

/// `(result label, count)` of the audit counter exported by `provider`.
fn audit_counts(
    provider: &SdkMeterProvider,
    exporter: &InMemoryMetricExporter,
    metric_name: &str,
) -> Vec<(String, u64, usize)> {
    provider.force_flush().expect("flush");
    let mut out = Vec::new();
    for rm in exporter.get_finished_metrics().unwrap() {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                if metric.name() != metric_name {
                    continue;
                }
                let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data() else {
                    panic!("`{metric_name}` is not a u64 sum");
                };
                for point in sum.data_points() {
                    let attributes: Vec<_> = point.attributes().collect();
                    let result = attributes
                        .iter()
                        .find(|kv| kv.key.as_str() == "result")
                        .map(|kv| kv.value.as_str().into_owned())
                        .unwrap_or_default();
                    out.push((result, point.value(), attributes.len()));
                }
            }
        }
    }
    out.sort();
    out
}

#[test]
fn record_audit_emit_counts_each_result_under_the_result_label_only() {
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(exporter.clone()).build())
        .build();
    let metrics = MiniChatMetrics::new(&provider.meter("mini_chat"), "");

    metrics.record_audit_emit(AuditEmitResult::Ok);
    metrics.record_audit_emit(AuditEmitResult::Ok);
    metrics.record_audit_emit(AuditEmitResult::Retry);
    metrics.record_audit_emit(AuditEmitResult::Reject);
    metrics.record_audit_emit(AuditEmitResult::Dropped);

    assert_eq!(
        audit_counts(&provider, &exporter, "mini_chat_audit_emit"),
        vec![
            ("dropped".to_owned(), 1, 1),
            ("ok".to_owned(), 2, 1),
            ("reject".to_owned(), 1, 1),
            ("retry".to_owned(), 1, 1),
        ]
    );
}

#[test]
fn the_audit_counter_uses_the_configured_prefix() {
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(exporter.clone()).build())
        .build();
    let metrics = MiniChatMetrics::new(&provider.meter("mini_chat"), "acme");

    metrics.record_audit_emit(AuditEmitResult::Ok);

    assert_eq!(
        audit_counts(&provider, &exporter, "acme_audit_emit"),
        vec![("ok".to_owned(), 1, 1)]
    );
    assert!(audit_counts(&provider, &exporter, "mini_chat_audit_emit").is_empty());
}
