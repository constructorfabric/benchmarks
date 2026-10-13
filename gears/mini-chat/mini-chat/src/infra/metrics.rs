//! OpenTelemetry instruments of the gear
//! (`cpt-cf-mini-chat-algo-gear-foundation-register-metrics`).
// @cpt-dod:cpt-cf-mini-chat-dod-gear-foundation-metrics:p1

use mini_chat_sdk::ModelTier;
use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram, Meter};

use crate::domain::service::attachments::model::{AttachmentKind, AttachmentStatus};
use crate::domain::service::attachments::ports::{
    AttachmentMetrics, BackgroundResult, UploadResult,
};
use crate::domain::service::mutations::ports::MutationMetrics;
use crate::domain::service::mutations::types::{MutationOp, MutationResult};
use crate::domain::service::quota::ports::{PreflightLabel, QuotaMetrics};
use crate::domain::service::quota::types::QuotaPeriod;
use crate::domain::service::stream::ports::{DisconnectStage, StreamMetrics};
use crate::domain::service::workers::metrics::{CleanupResource, SummaryResult, WorkerMetrics};

/// Metric prefix used when `metrics.prefix` is empty.
pub const DEFAULT_METRICS_PREFIX: &str = "mini_chat";
/// Instrumentation scope of the gear meter.
pub const METER_NAME: &str = "mini_chat";

/// Outcome label of `mini_chat_audit_emit{result}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditEmitResult {
    /// The plugin accepted the event.
    Ok,
    /// Transient failure, timeout or resolution failure.
    Retry,
    /// Corrupt payload, permanent error or last attempt.
    Reject,
    /// No plugin registered.
    Dropped,
}

impl AuditEmitResult {
    /// Label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Retry => "retry",
            Self::Reject => "reject",
            Self::Dropped => "dropped",
        }
    }
}

/// Records the audit delivery outcome (small port so handlers are testable).
pub trait AuditEmitMetrics: Send + Sync {
    /// Counts one delivery with `result`.
    fn record_audit_emit(&self, result: AuditEmitResult);
}

#[derive(Clone, Copy)]
enum Kind {
    Counter,
    UpDownCounter,
    Histogram,
    Gauge,
}

/// Every instrument of DESIGN "Metric series" (emitted and declared).
const INSTRUMENTS: &[(&str, Kind)] = &[
    ("stream_started", Kind::Counter),
    ("stream_completed", Kind::Counter),
    ("stream_failed", Kind::Counter),
    ("stream_incomplete", Kind::Counter),
    ("stream_disconnected", Kind::Counter),
    ("active_streams", Kind::UpDownCounter),
    ("ttft_provider_ms", Kind::Histogram),
    ("ttft_overhead_ms", Kind::Histogram),
    ("stream_total_latency_ms", Kind::Histogram),
    ("cancel_requested", Kind::Counter),
    ("cancel_effective", Kind::Counter),
    ("time_to_abort_ms", Kind::Histogram),
    ("streams_aborted", Kind::Counter),
    ("orphan_detected", Kind::Counter),
    ("orphan_finalized", Kind::Counter),
    ("orphan_scan_duration_seconds", Kind::Histogram),
    ("attachment_upload_abandoned", Kind::Counter),
    ("upload_reaper_scan_duration_seconds", Kind::Histogram),
    ("attachment_background_indexing", Kind::Counter),
    ("quota_preflight", Kind::Counter),
    ("quota_reserve", Kind::Counter),
    ("quota_commit", Kind::Counter),
    ("quota_overshoot", Kind::Counter),
    ("quota_estimated_tokens", Kind::Histogram),
    ("quota_actual_tokens", Kind::Histogram),
    ("code_interpreter_calls", Kind::Counter),
    ("knowledge_search", Kind::Counter),
    ("knowledge_search_latency_ms", Kind::Histogram),
    ("knowledge_search_chunks", Kind::Histogram),
    ("thread_summary_trigger", Kind::Counter),
    ("thread_summary_execution", Kind::Counter),
    ("thread_summary_cas_conflicts", Kind::Counter),
    ("summary_fallback", Kind::Counter),
    ("turn_mutation", Kind::Counter),
    ("turn_mutation_latency_ms", Kind::Histogram),
    ("attachment_upload", Kind::Counter),
    ("attachment_upload_bytes", Kind::Histogram),
    ("attachments_pending", Kind::UpDownCounter),
    ("image_inputs_per_turn", Kind::Histogram),
    ("cleanup_completed", Kind::Counter),
    ("cleanup_failed", Kind::Counter),
    ("cleanup_retry", Kind::Counter),
    (
        "cleanup_vector_store_with_failed_attachments",
        Kind::Counter,
    ),
    ("secondary_cleanup_skipped", Kind::Counter),
    ("audit_emit", Kind::Counter),
    ("finalization_latency_ms", Kind::Histogram),
    // declared, never recorded
    ("stream_replay", Kind::Counter),
    ("quota_preflight_v2", Kind::Counter),
    ("quota_tier_downgrade", Kind::Counter),
    ("quota_negative", Kind::Counter),
    ("quota_overshoot_tokens", Kind::Counter),
    ("quota_image_commit", Kind::Counter),
    ("tokens_after_cancel", Kind::Histogram),
    ("time_from_ui_disconnect_to_cancel_ms", Kind::Histogram),
    ("cancel_orphan", Kind::Counter),
    ("tool_calls", Kind::Counter),
    ("tool_call_limited", Kind::Counter),
    ("file_search_latency_ms", Kind::Histogram),
    ("web_search_latency_ms", Kind::Histogram),
    ("web_search_disabled", Kind::Counter),
    ("citations_count", Kind::Histogram),
    ("citations_by_source", Kind::Counter),
    ("retrieval_latency_ms", Kind::Histogram),
    ("retrieval_chunks_returned", Kind::Histogram),
    ("retrieval_zero_hit", Kind::Counter),
    ("indexed_chunks_per_chat", Kind::Gauge),
    ("upload_rejected", Kind::Counter),
    ("vector_stores_per_user", Kind::Gauge),
    ("context_truncation", Kind::Counter),
    ("provider_requests", Kind::Counter),
    ("provider_errors", Kind::Counter),
    ("provider_latency_ms", Kind::Histogram),
    ("oagw_retries", Kind::Counter),
    ("oagw_upstream_latency_ms", Kind::Histogram),
    ("oagw_circuit_open", Kind::Counter),
    ("attachment_index", Kind::Counter),
    ("attachment_summary", Kind::Counter),
    ("attachments_failed", Kind::Counter),
    ("attachment_index_latency_ms", Kind::Histogram),
    ("image_turns", Kind::Counter),
    ("media_rejected", Kind::Counter),
    ("cleanup_backlog", Kind::Gauge),
    ("audit_redaction_hits", Kind::Counter),
    ("summary_regen", Kind::Counter),
    ("outbox_enqueue", Kind::Counter),
    ("outbox_dispatch", Kind::Counter),
    ("outbox_dead", Kind::Counter),
    ("outbox_dead_rows", Kind::Gauge),
    ("outbox_pending_age_seconds", Kind::Gauge),
    ("outbox_oldest_pending_age_seconds", Kind::Gauge),
    ("db_query_latency_ms", Kind::Histogram),
    ("db_errors", Kind::Counter),
    ("unknown_error_code", Kind::Counter),
    ("credits_overflow", Kind::Counter),
];

/// Counters of the turn-mutation group.
const MUTATION_COUNTERS: [&str; 1] = ["turn_mutation"];

/// Histograms of the turn-mutation group.
const MUTATION_HISTOGRAMS: [&str; 1] = ["turn_mutation_latency_ms"];

/// Counters of the billing group.
// @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-record-metrics:p1:inst-label-rules
const QUOTA_COUNTERS: [&str; 5] = [
    "quota_preflight",
    "quota_reserve",
    "quota_commit",
    "quota_overshoot",
    "code_interpreter_calls",
];

/// Histograms of the billing group.
const QUOTA_HISTOGRAMS: [&str; 2] = ["quota_estimated_tokens", "quota_actual_tokens"];
// @cpt-end:cpt-cf-mini-chat-algo-quota-billing-record-metrics:p1:inst-label-rules

/// Counters of the background workers group
/// (`cpt-cf-mini-chat-algo-background-workers-record-metrics`).
const WORKER_COUNTERS: [&str; 7] = [
    "orphan_detected",
    "orphan_finalized",
    "cleanup_vector_store_with_failed_attachments",
    "thread_summary_trigger",
    "thread_summary_execution",
    "thread_summary_cas_conflicts",
    "summary_fallback",
];

/// Histograms of the background workers group.
const WORKER_HISTOGRAMS: [&str; 1] = ["orphan_scan_duration_seconds"];

/// Counters of the request group.
const STREAM_COUNTERS: [&str; 9] = [
    "stream_started",
    "stream_completed",
    "stream_failed",
    "stream_incomplete",
    "stream_disconnected",
    "cancel_requested",
    "cancel_effective",
    "streams_aborted",
    "knowledge_search",
];

/// Histograms of the request group.
const STREAM_HISTOGRAMS: [&str; 8] = [
    "ttft_provider_ms",
    "ttft_overhead_ms",
    "stream_total_latency_ms",
    "time_to_abort_ms",
    "image_inputs_per_turn",
    "finalization_latency_ms",
    "knowledge_search_latency_ms",
    "knowledge_search_chunks",
];

/// Counters of the attachments group.
const ATTACHMENT_COUNTERS: [&str; 7] = [
    "attachment_upload",
    "attachment_background_indexing",
    "attachment_upload_abandoned",
    "cleanup_completed",
    "cleanup_failed",
    "cleanup_retry",
    "secondary_cleanup_skipped",
];

/// Histograms of the attachments group.
const ATTACHMENT_HISTOGRAMS: [&str; 2] = [
    "attachment_upload_bytes",
    "upload_reaper_scan_duration_seconds",
];

/// The attachment instruments (`cpt-cf-mini-chat-algo-attachments-record-metrics`).
struct AttachmentInstruments {
    upload: Counter<u64>,
    upload_bytes: Histogram<f64>,
    pending: opentelemetry::metrics::UpDownCounter<i64>,
    background: Counter<u64>,
    abandoned: Counter<u64>,
    reaper_scan: Histogram<f64>,
    cleanup_completed: Counter<u64>,
    cleanup_failed: Counter<u64>,
    cleanup_retry: Counter<u64>,
    secondary_skipped: Counter<u64>,
}

/// A registered instrument, kept alive for the lifetime of the gear.
type Instrument = Box<dyn std::any::Any + Send + Sync>;

/// The billing instruments recorded by the quota service.
struct QuotaInstruments {
    preflight: Counter<u64>,
    reserve: Counter<u64>,
    commit: Counter<u64>,
    overshoot: Counter<u64>,
    code_interpreter_calls: Counter<u64>,
    estimated_tokens: Histogram<f64>,
    actual_tokens: Histogram<f64>,
}

/// The request group of the cost metrics recorded by the stream pipeline.
struct StreamInstruments {
    started: Counter<u64>,
    active: opentelemetry::metrics::UpDownCounter<i64>,
    completed: Counter<u64>,
    failed: Counter<u64>,
    incomplete: Counter<u64>,
    disconnected: Counter<u64>,
    cancel_requested: Counter<u64>,
    cancel_effective: Counter<u64>,
    aborted: Counter<u64>,
    knowledge_search: Counter<u64>,
    ttft_provider: Histogram<f64>,
    ttft_overhead: Histogram<f64>,
    total_latency: Histogram<f64>,
    time_to_abort: Histogram<f64>,
    image_inputs: Histogram<f64>,
    finalization_latency: Histogram<f64>,
    knowledge_latency: Histogram<f64>,
    knowledge_chunks: Histogram<f64>,
}

/// The background worker instruments.
struct WorkerInstruments {
    orphan_detected: Counter<u64>,
    orphan_finalized: Counter<u64>,
    orphan_scan: Histogram<f64>,
    store_with_failed_attachments: Counter<u64>,
    summary_trigger: Counter<u64>,
    summary_execution: Counter<u64>,
    summary_cas_conflicts: Counter<u64>,
    summary_fallback: Counter<u64>,
}

/// The turn-mutation instruments.
struct MutationInstruments {
    count: Counter<u64>,
    latency: Histogram<f64>,
}

/// The metrics handle shared by all services and handlers.
pub struct MiniChatMetrics {
    audit_emit: Counter<u64>,
    mutation: MutationInstruments,
    quota: QuotaInstruments,
    stream: StreamInstruments,
    attachments: AttachmentInstruments,
    workers: WorkerInstruments,
    _registered: Vec<Instrument>,
}

/// The effective prefix: `prefix`, or `mini_chat` when empty.
#[must_use]
pub fn effective_prefix(prefix: &str) -> &str {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-register-metrics:p1:inst-create-meter
    if prefix.is_empty() {
        DEFAULT_METRICS_PREFIX
    } else {
        prefix
    }
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-register-metrics:p1:inst-create-meter
}

impl MiniChatMetrics {
    /// Registers every instrument on `meter` with the effective `prefix`.
    #[must_use]
    pub fn new(meter: &Meter, prefix: &str) -> Self {
        let prefix = effective_prefix(prefix);
        let mut registered: Vec<Instrument> = Vec::with_capacity(INSTRUMENTS.len());
        let mut audit_emit = None;
        let mut counters: std::collections::HashMap<&str, Counter<u64>> =
            std::collections::HashMap::new();
        let mut histograms: std::collections::HashMap<&str, Histogram<f64>> =
            std::collections::HashMap::new();
        let mut active_streams = None;
        let mut attachments_pending = None;
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-register-metrics:p1:inst-register-instruments
        for (base, kind) in INSTRUMENTS {
            let name = format!("{prefix}_{base}");
            match kind {
                Kind::Counter => {
                    let counter = meter.u64_counter(name).build();
                    if *base == "audit_emit" {
                        audit_emit = Some(counter.clone());
                    }
                    if QUOTA_COUNTERS.contains(base)
                        || MUTATION_COUNTERS.contains(base)
                        || STREAM_COUNTERS.contains(base)
                        || ATTACHMENT_COUNTERS.contains(base)
                        || WORKER_COUNTERS.contains(base)
                    {
                        counters.insert(base, counter.clone());
                    }
                    registered.push(Box::new(counter));
                }
                Kind::UpDownCounter => {
                    let counter = meter.i64_up_down_counter(name).build();
                    if *base == "active_streams" {
                        active_streams = Some(counter.clone());
                    }
                    if *base == "attachments_pending" {
                        attachments_pending = Some(counter.clone());
                    }
                    registered.push(Box::new(counter));
                }
                Kind::Histogram => {
                    let histogram = meter.f64_histogram(name).build();
                    if QUOTA_HISTOGRAMS.contains(base)
                        || MUTATION_HISTOGRAMS.contains(base)
                        || STREAM_HISTOGRAMS.contains(base)
                        || ATTACHMENT_HISTOGRAMS.contains(base)
                        || WORKER_HISTOGRAMS.contains(base)
                    {
                        histograms.insert(base, histogram.clone());
                    }
                    registered.push(Box::new(histogram));
                }
                Kind::Gauge => registered.push(Box::new(meter.u64_gauge(name).build())),
            }
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-register-metrics:p1:inst-register-instruments
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-register-metrics:p1:inst-return-metrics
        let counter = |base: &str| {
            counters
                .get(base)
                .cloned()
                .unwrap_or_else(|| meter.u64_counter(format!("{prefix}_{base}")).build())
        };
        let histogram = |base: &str| {
            histograms
                .get(base)
                .cloned()
                .unwrap_or_else(|| meter.f64_histogram(format!("{prefix}_{base}")).build())
        };
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-record-metrics:p1:inst-return-metrics
        let quota = QuotaInstruments {
            preflight: counter("quota_preflight"),
            reserve: counter("quota_reserve"),
            commit: counter("quota_commit"),
            overshoot: counter("quota_overshoot"),
            code_interpreter_calls: counter("code_interpreter_calls"),
            estimated_tokens: histogram("quota_estimated_tokens"),
            actual_tokens: histogram("quota_actual_tokens"),
        };
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-record-metrics:p1:inst-return-metrics
        let stream = StreamInstruments {
            started: counter("stream_started"),
            active: active_streams.unwrap_or_else(|| {
                meter
                    .i64_up_down_counter(format!("{prefix}_active_streams"))
                    .build()
            }),
            completed: counter("stream_completed"),
            failed: counter("stream_failed"),
            incomplete: counter("stream_incomplete"),
            disconnected: counter("stream_disconnected"),
            cancel_requested: counter("cancel_requested"),
            cancel_effective: counter("cancel_effective"),
            aborted: counter("streams_aborted"),
            knowledge_search: counter("knowledge_search"),
            ttft_provider: histogram("ttft_provider_ms"),
            ttft_overhead: histogram("ttft_overhead_ms"),
            total_latency: histogram("stream_total_latency_ms"),
            time_to_abort: histogram("time_to_abort_ms"),
            image_inputs: histogram("image_inputs_per_turn"),
            finalization_latency: histogram("finalization_latency_ms"),
            knowledge_latency: histogram("knowledge_search_latency_ms"),
            knowledge_chunks: histogram("knowledge_search_chunks"),
        };
        let attachments = AttachmentInstruments {
            upload: counter("attachment_upload"),
            upload_bytes: histogram("attachment_upload_bytes"),
            pending: attachments_pending.unwrap_or_else(|| {
                meter
                    .i64_up_down_counter(format!("{prefix}_attachments_pending"))
                    .build()
            }),
            background: counter("attachment_background_indexing"),
            abandoned: counter("attachment_upload_abandoned"),
            reaper_scan: histogram("upload_reaper_scan_duration_seconds"),
            cleanup_completed: counter("cleanup_completed"),
            cleanup_failed: counter("cleanup_failed"),
            cleanup_retry: counter("cleanup_retry"),
            secondary_skipped: counter("secondary_cleanup_skipped"),
        };
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-record-metrics:p1:inst-return-worker-metrics
        let workers = WorkerInstruments {
            orphan_detected: counter("orphan_detected"),
            orphan_finalized: counter("orphan_finalized"),
            orphan_scan: histogram("orphan_scan_duration_seconds"),
            store_with_failed_attachments: counter("cleanup_vector_store_with_failed_attachments"),
            summary_trigger: counter("thread_summary_trigger"),
            summary_execution: counter("thread_summary_execution"),
            summary_cas_conflicts: counter("thread_summary_cas_conflicts"),
            summary_fallback: counter("summary_fallback"),
        };
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-record-metrics:p1:inst-return-worker-metrics
        Self {
            audit_emit: audit_emit
                .unwrap_or_else(|| meter.u64_counter(format!("{prefix}_audit_emit")).build()),
            mutation: MutationInstruments {
                count: counter("turn_mutation"),
                latency: histogram("turn_mutation_latency_ms"),
            },
            quota,
            stream,
            attachments,
            workers,
            _registered: registered,
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-register-metrics:p1:inst-return-metrics
    }

    /// Registers the instruments on the process-global meter.
    #[must_use]
    pub fn from_global(prefix: &str) -> Self {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-register-metrics:p1:inst-create-meter
        Self::new(&opentelemetry::global::meter(METER_NAME), prefix)
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-register-metrics:p1:inst-create-meter
    }
}

impl AuditEmitMetrics for MiniChatMetrics {
    fn record_audit_emit(&self, result: AuditEmitResult) {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-register-metrics:p1:inst-label-rules
        // Only the low-cardinality `result` label is ever attached.
        self.audit_emit
            .add(1, &[KeyValue::new("result", result.as_str())]);
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-register-metrics:p1:inst-label-rules
    }
}

/// The streaming request instruments of the streaming-turns feature; only the
/// low-cardinality labels `provider`, `model`, `error_code`, `reason`, `stage`,
/// `trigger` and `result` are ever attached.
// @cpt-dod:cpt-cf-mini-chat-dod-streaming-turns-audit-metrics:p1
// @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-record-metrics:p1:inst-return-metrics
impl StreamMetrics for MiniChatMetrics {
    fn stream_started(&self, provider: &str, model: &str) {
        self.stream.started.add(1, &provider_model(provider, model));
    }

    fn active_streams_add(&self, delta: i64) {
        self.stream.active.add(delta, &[]);
    }

    fn ttft_provider_ms(&self, provider: &str, model: &str, millis: f64) {
        self.stream
            .ttft_provider
            .record(millis, &provider_model(provider, model));
    }

    fn ttft_overhead_ms(&self, provider: &str, model: &str, millis: f64) {
        self.stream
            .ttft_overhead
            .record(millis, &provider_model(provider, model));
    }

    fn stream_completed(&self, provider: &str, model: &str) {
        self.stream
            .completed
            .add(1, &provider_model(provider, model));
    }

    fn stream_incomplete(&self, provider: &str, model: &str, reason: &str) {
        let mut labels = provider_model(provider, model).to_vec();
        labels.push(KeyValue::new("reason", reason.to_owned()));
        self.stream.incomplete.add(1, &labels);
    }

    fn stream_failed(&self, provider: &str, model: &str, error_code: &str) {
        let mut labels = provider_model(provider, model).to_vec();
        labels.push(KeyValue::new("error_code", error_code.to_owned()));
        self.stream.failed.add(1, &labels);
    }

    fn stream_total_latency_ms(&self, provider: &str, model: &str, millis: f64) {
        self.stream
            .total_latency
            .record(millis, &provider_model(provider, model));
    }

    fn cancel_requested(&self) {
        self.stream
            .cancel_requested
            .add(1, &[KeyValue::new("trigger", "disconnect")]);
    }

    fn stream_disconnected(&self, stage: DisconnectStage) {
        self.stream
            .disconnected
            .add(1, &[KeyValue::new("stage", stage.as_str())]);
    }

    fn time_to_abort_ms(&self, millis: f64) {
        self.stream
            .time_to_abort
            .record(millis, &[KeyValue::new("trigger", "disconnect")]);
    }

    fn cancel_effective(&self) {
        self.stream
            .cancel_effective
            .add(1, &[KeyValue::new("trigger", "disconnect")]);
    }

    fn streams_aborted(&self) {
        self.stream
            .aborted
            .add(1, &[KeyValue::new("trigger", "client_disconnect")]);
    }

    fn image_inputs_per_turn(&self, count: u32) {
        self.stream.image_inputs.record(f64::from(count), &[]);
    }

    fn finalization_latency_ms(&self, millis: f64) {
        self.stream.finalization_latency.record(millis, &[]);
    }

    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-record-metrics:p1:inst-metric-knowledge
    fn knowledge_search(&self, result: &str) {
        self.stream
            .knowledge_search
            .add(1, &[KeyValue::new("result", result.to_owned())]);
    }

    fn knowledge_search_latency_ms(&self, millis: f64) {
        self.stream.knowledge_latency.record(millis, &[]);
    }

    fn knowledge_search_chunks(&self, chunks: u64) {
        let chunks = u32::try_from(chunks).unwrap_or(u32::MAX);
        self.stream.knowledge_chunks.record(f64::from(chunks), &[]);
    }

    // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-record-metrics:p1:inst-summary-metrics
    fn thread_summary_trigger(&self, result: &str) {
        self.workers
            .summary_trigger
            .add(1, &[KeyValue::new("result", result.to_owned())]);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-background-workers-record-metrics:p1:inst-summary-metrics
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-record-metrics:p1:inst-metric-knowledge
}
// @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-record-metrics:p1:inst-return-metrics

// @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-record-metrics:p1:inst-metric-labels
fn provider_model(provider: &str, model: &str) -> [KeyValue; 2] {
    [
        KeyValue::new("provider", provider.to_owned()),
        KeyValue::new("model", model.to_owned()),
    ]
}
// @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-record-metrics:p1:inst-metric-labels

#[allow(clippy::cast_precision_loss)]
fn tokens_value(tokens: i64) -> f64 {
    tokens.max(0) as f64
}

// @cpt-dod:cpt-cf-mini-chat-dod-quota-billing-cost-metrics:p1
impl QuotaMetrics for MiniChatMetrics {
    fn record_preflight(&self, decision: PreflightLabel, model: &str, tier: ModelTier) {
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-record-metrics:p1:inst-count-preflight
        self.quota.preflight.add(
            1,
            &[
                KeyValue::new("decision", decision.as_str()),
                KeyValue::new("model", model.to_owned()),
                KeyValue::new(
                    "tier",
                    match tier {
                        ModelTier::Premium => "premium",
                        ModelTier::Standard => "standard",
                    },
                ),
            ],
        );
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-record-metrics:p1:inst-count-preflight
    }

    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-record-metrics:p1:inst-record-estimated
    fn record_estimated_tokens(&self, tokens: i64) {
        self.quota
            .estimated_tokens
            .record(tokens_value(tokens), &[]);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-record-metrics:p1:inst-record-estimated

    fn record_reserve(&self, period: QuotaPeriod) {
        self.quota
            .reserve
            .add(1, &[KeyValue::new("period", period.as_str())]);
    }

    fn record_commit(&self, period: QuotaPeriod) {
        self.quota
            .commit
            .add(1, &[KeyValue::new("period", period.as_str())]);
    }

    fn record_actual_tokens(&self, tokens: i64) {
        self.quota.actual_tokens.record(tokens_value(tokens), &[]);
    }

    fn record_overshoot(&self, period: QuotaPeriod) {
        self.quota
            .overshoot
            .add(1, &[KeyValue::new("period", period.as_str())]);
    }

    fn record_code_interpreter_calls(&self, model: &str, calls: u64) {
        self.quota
            .code_interpreter_calls
            .add(calls, &[KeyValue::new("model", model.to_owned())]);
    }
}

// @cpt-dod:cpt-cf-mini-chat-dod-attachments-metrics:p1
/// The attachment instruments of `cpt-cf-mini-chat-algo-attachments-record-metrics`;
/// only bounded labels are ever attached.
// @cpt-begin:cpt-cf-mini-chat-algo-attachments-record-metrics:p1:inst-no-high-cardinality
impl AttachmentMetrics for MiniChatMetrics {
    fn upload(&self, kind: AttachmentKind, result: UploadResult) {
        self.attachments.upload.add(
            1,
            &[
                KeyValue::new("kind", kind.as_str()),
                KeyValue::new("result", result.as_str()),
            ],
        );
    }

    #[allow(clippy::cast_precision_loss)]
    fn upload_bytes(&self, kind: AttachmentKind, bytes: u64) {
        self.attachments
            .upload_bytes
            .record(bytes as f64, &[KeyValue::new("kind", kind.as_str())]);
    }

    fn pending_add(&self, delta: i64) {
        self.attachments.pending.add(delta, &[]);
    }

    fn background_indexing(&self, result: BackgroundResult) {
        self.attachments
            .background
            .add(1, &[KeyValue::new("result", result.as_str())]);
    }

    fn upload_abandoned(&self, from_status: AttachmentStatus) {
        self.attachments
            .abandoned
            .add(1, &[KeyValue::new("from_status", from_status.as_str())]);
    }

    fn reaper_scan_seconds(&self, seconds: f64) {
        self.attachments.reaper_scan.record(seconds, &[]);
    }

    fn cleanup_completed(&self) {
        self.attachments
            .cleanup_completed
            .add(1, &[KeyValue::new("resource_type", "file")]);
    }

    fn cleanup_failed(&self) {
        self.attachments
            .cleanup_failed
            .add(1, &[KeyValue::new("resource_type", "file")]);
    }

    fn cleanup_retry(&self) {
        self.attachments.cleanup_retry.add(
            1,
            &[
                KeyValue::new("resource_type", "file"),
                KeyValue::new("reason", "provider_error"),
            ],
        );
    }

    fn secondary_cleanup_skipped(&self) {
        self.attachments
            .secondary_skipped
            .add(1, &[KeyValue::new("provider_kind", "anthropic")]);
    }
}
// @cpt-end:cpt-cf-mini-chat-algo-attachments-record-metrics:p1:inst-no-high-cardinality

// @cpt-dod:cpt-cf-mini-chat-dod-background-workers-metrics:p1
/// The worker instruments of the background-workers feature; only the labels
/// `reason`, `resource_type`, `trigger`, `provider_kind` and `result` are ever
/// attached, with the values of `cpt-cf-mini-chat-algo-background-workers-record-metrics`.
// @cpt-begin:cpt-cf-mini-chat-algo-background-workers-record-metrics:p1:inst-worker-label-rules
impl WorkerMetrics for MiniChatMetrics {
    // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-record-metrics:p1:inst-watchdog-metrics
    fn orphan_detected(&self) {
        self.workers
            .orphan_detected
            .add(1, &[KeyValue::new("reason", "stale_progress")]);
    }

    fn orphan_finalized(&self) {
        self.workers
            .orphan_finalized
            .add(1, &[KeyValue::new("reason", "stale_progress")]);
    }

    fn orphan_stream_aborted(&self) {
        self.stream
            .aborted
            .add(1, &[KeyValue::new("trigger", "orphan_timeout")]);
    }

    fn orphan_scan_seconds(&self, seconds: f64) {
        self.workers.orphan_scan.record(seconds, &[]);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-background-workers-record-metrics:p1:inst-watchdog-metrics

    // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-record-metrics:p1:inst-cleanup-metrics
    fn chat_cleanup_completed(&self, resource: CleanupResource) {
        self.attachments
            .cleanup_completed
            .add(1, &[KeyValue::new("resource_type", resource.as_str())]);
    }

    fn chat_cleanup_failed(&self, resource: CleanupResource) {
        self.attachments
            .cleanup_failed
            .add(1, &[KeyValue::new("resource_type", resource.as_str())]);
    }

    fn chat_cleanup_retry(&self, resource: CleanupResource) {
        self.attachments.cleanup_retry.add(
            1,
            &[
                KeyValue::new("resource_type", resource.as_str()),
                KeyValue::new("reason", resource.retry_reason()),
            ],
        );
    }

    fn chat_cleanup_store_with_failed_attachments(&self) {
        self.workers.store_with_failed_attachments.add(1, &[]);
    }

    fn chat_secondary_cleanup_skipped(&self) {
        self.attachments
            .secondary_skipped
            .add(1, &[KeyValue::new("provider_kind", "anthropic")]);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-background-workers-record-metrics:p1:inst-cleanup-metrics

    // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-record-metrics:p1:inst-summary-metrics
    fn summary_execution(&self, result: SummaryResult) {
        self.workers
            .summary_execution
            .add(1, &[KeyValue::new("result", result.as_str())]);
    }

    fn summary_cas_conflict(&self) {
        self.workers.summary_cas_conflicts.add(1, &[]);
    }

    fn summary_fallback(&self) {
        self.workers.summary_fallback.add(1, &[]);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-background-workers-record-metrics:p1:inst-summary-metrics
}
// @cpt-end:cpt-cf-mini-chat-algo-background-workers-record-metrics:p1:inst-worker-label-rules

/// The mutation instruments of the turn-mutations feature; only the labels
/// `op` and `result` are ever attached.
impl MutationMetrics for MiniChatMetrics {
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-record-metrics:p1:inst-metric-mutation-labels
    fn turn_mutation(&self, op: MutationOp, result: MutationResult) {
        self.mutation.count.add(
            1,
            &[
                KeyValue::new("op", op.as_str()),
                KeyValue::new("result", result.as_str()),
            ],
        );
    }

    fn turn_mutation_latency_ms(&self, op: MutationOp, millis: f64) {
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-record-metrics:p1:inst-return-mutation-metrics
        self.mutation
            .latency
            .record(millis, &[KeyValue::new("op", op.as_str())]);
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-record-metrics:p1:inst-return-mutation-metrics
    }
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-record-metrics:p1:inst-metric-mutation-labels
}

#[cfg(test)]
#[path = "metrics_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "metrics_attachments_tests.rs"]
mod attachments_tests;

#[cfg(test)]
#[path = "metrics_quota_tests.rs"]
mod quota_tests;

#[cfg(test)]
#[path = "metrics_stream_tests.rs"]
mod stream_tests;

#[cfg(test)]
#[path = "metrics_mutations_tests.rs"]
mod mutations_tests;

#[cfg(test)]
#[path = "metrics_workers_tests.rs"]
mod workers_tests;
