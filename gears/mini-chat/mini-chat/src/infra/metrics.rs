//! OpenTelemetry instruments of the gear (observability algorithm).
//!
//! One meter; every instrument is named `{prefix}_{name}` where the prefix is
//! `metrics.prefix` (empty means `mini_chat`). Metrics are exported over OTLP by the
//! `ToolKit`; the process has no Prometheus exporter. Labels are restricted to the low
//! cardinality keys of DESIGN 4 (`provider`, `model`, `provider_kind`, `resource_type`,
//! `kind`, `tier`, `state`, `stage`, `op`, `reason`, `error_code`, `decision`, `period`,
//! `trigger`, `result`, `from_status`). Counter names carry no `_total` suffix: the OTLP to
//! Prometheus conversion downstream decides that.

use std::collections::BTreeMap;
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Gauge, Histogram, Meter, UpDownCounter};
use opentelemetry::trace::TraceContextExt;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::domain::quota::{PeriodType, QuotaMetrics};

pub mod alerts;
pub mod attachments;
pub mod maintenance;

/// The `result` label of `mini_chat_audit_emit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditEmitResult {
    Ok,
    Retry,
    Reject,
    Dropped,
}

impl AuditEmitResult {
    /// The label value.
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

/// The kind of an instrument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Counter,
    UpDownCounter,
    Histogram,
    Gauge,
}

/// The `Emitted` table of DESIGN 4 (names without the prefix): recorded by the owning
/// features.
const EMITTED: [(&str, Kind); 46] = [
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
    (AUDIT_EMIT, Kind::Counter),
    ("finalization_latency_ms", Kind::Histogram),
];

// @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-metrics:p1:inst-met-deferred
// the attachment series of this list (`file_search_latency_ms`, `retrieval_*`, `upload_rejected`,
// ...) are registered and left unrecorded
/// `Declared but not recorded (deferred)`: registered, never recorded.
const DECLARED_NOT_RECORDED: [(&str, Kind); 48] = [
    ("stream_replay", Kind::Counter),
    ("quota_preflight_v2", Kind::Counter),
    ("quota_tier_downgrade", Kind::Counter),
    ("quota_negative", Kind::Counter),
    ("quota_overshoot_tokens", Kind::Histogram),
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
// @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-metrics:p1:inst-met-deferred

/// Name (without the prefix) of the audit delivery counter.
const AUDIT_EMIT: &str = "audit_emit";

/// A registered instrument, handed to the feature that records it.
#[derive(Debug, Clone)]
pub enum Instrument {
    Counter(Counter<u64>),
    UpDownCounter(UpDownCounter<i64>),
    Histogram(Histogram<f64>),
    Gauge(Gauge<u64>),
}

impl Instrument {
    fn register(meter: &Meter, name: String, kind: Kind) -> Self {
        match kind {
            Kind::Counter => Self::Counter(meter.u64_counter(name).build()),
            Kind::UpDownCounter => Self::UpDownCounter(meter.i64_up_down_counter(name).build()),
            Kind::Histogram => Self::Histogram(meter.f64_histogram(name).build()),
            Kind::Gauge => Self::Gauge(meter.u64_gauge(name).build()),
        }
    }
}

/// The recorded cost series of the quota rules.
#[derive(Debug)]
struct CostInstruments {
    preflight: Counter<u64>,
    reserve: Counter<u64>,
    commit: Counter<u64>,
    overshoot: Counter<u64>,
    estimated_tokens: Histogram<f64>,
    actual_tokens: Histogram<f64>,
    code_interpreter_calls: Counter<u64>,
}

/// The recorded series of the message streaming feature (stream, cancellation, finalization and
/// orphan watchdog series).
#[derive(Debug)]
struct StreamInstruments {
    stream_started: Counter<u64>,
    stream_completed: Counter<u64>,
    stream_failed: Counter<u64>,
    stream_incomplete: Counter<u64>,
    stream_disconnected: Counter<u64>,
    active_streams: UpDownCounter<i64>,
    ttft_provider_ms: Histogram<f64>,
    ttft_overhead_ms: Histogram<f64>,
    stream_total_latency_ms: Histogram<f64>,
    cancel_requested: Counter<u64>,
    cancel_effective: Counter<u64>,
    time_to_abort_ms: Histogram<f64>,
    streams_aborted: Counter<u64>,
    orphan_detected: Counter<u64>,
    orphan_finalized: Counter<u64>,
    orphan_scan_duration_seconds: Histogram<f64>,
    image_inputs_per_turn: Histogram<f64>,
    thread_summary_trigger: Counter<u64>,
    finalization_latency_ms: Histogram<f64>,
}

/// The attachment series of the upload side, recorded through `domain::attachments::AttachmentMetrics`
/// (`metrics/attachments.rs`).
#[derive(Debug)]
pub(crate) struct AttachmentInstruments {
    pub(crate) upload: Counter<u64>,
    pub(crate) upload_bytes: Histogram<f64>,
    pub(crate) pending: UpDownCounter<i64>,
    pub(crate) background: Counter<u64>,
    pub(crate) abandoned: Counter<u64>,
    pub(crate) reaper_scan: Histogram<f64>,
    pub(crate) cleanup_completed: Counter<u64>,
    pub(crate) cleanup_retry: Counter<u64>,
    pub(crate) cleanup_failed: Counter<u64>,
    pub(crate) secondary_skipped: Counter<u64>,
    pub(crate) knowledge: Counter<u64>,
    pub(crate) knowledge_latency: Histogram<f64>,
    pub(crate) knowledge_chunks: Histogram<f64>,
}

/// The background-maintenance series (`metrics/maintenance.rs`).
#[derive(Debug)]
pub(crate) struct MaintenanceInstruments {
    pub(crate) execution: Counter<u64>,
    pub(crate) cas_conflicts: Counter<u64>,
    pub(crate) fallback: Counter<u64>,
    pub(crate) debt: Counter<u64>,
}

/// The registered instruments.
#[derive(Debug)]
pub struct MiniChatMetrics {
    instruments: BTreeMap<String, Instrument>,
    audit_emit: Counter<u64>,
    cost: CostInstruments,
    stream: StreamInstruments,
    pub(crate) attachment: AttachmentInstruments,
    pub(crate) maintenance: MaintenanceInstruments,
    turn_mutation: Counter<u64>,
    turn_mutation_latency_ms: Histogram<f64>,
}

/// The registered counter `name` (a full name), or a new one on `meter` when it is missing.
fn registered_counter(
    instruments: &BTreeMap<String, Instrument>,
    meter: &Meter,
    name: String,
) -> Counter<u64> {
    match instruments.get(&name) {
        Some(Instrument::Counter(counter)) => counter.clone(),
        _ => meter.u64_counter(name).build(),
    }
}

/// The registered histogram `name` (a full name), or a new one on `meter` when it is missing.
fn registered_histogram(
    instruments: &BTreeMap<String, Instrument>,
    meter: &Meter,
    name: String,
) -> Histogram<f64> {
    match instruments.get(&name) {
        Some(Instrument::Histogram(histogram)) => histogram.clone(),
        _ => meter.f64_histogram(name).build(),
    }
}

/// The registered up-down counter `name` (a full name), or a new one on `meter` when it is
/// missing.
fn registered_up_down(
    instruments: &BTreeMap<String, Instrument>,
    meter: &Meter,
    name: String,
) -> UpDownCounter<i64> {
    match instruments.get(&name) {
        Some(Instrument::UpDownCounter(counter)) => counter.clone(),
        _ => meter.i64_up_down_counter(name).build(),
    }
}

/// A token count as a histogram value (exact below 2^32, saturating above).
fn histogram_value(tokens: i64) -> f64 {
    u32::try_from(tokens.max(0)).map_or_else(|_| f64::from(u32::MAX), f64::from)
}

impl MiniChatMetrics {
    /// Register every instrument of the DESIGN `Emitted` table and of the `Declared but not
    /// recorded (deferred)` list on `meter`, named `{prefix}_{name}`. Nothing of the `Not
    /// defined` list is registered.
    #[must_use]
    pub fn new(meter: &Meter, prefix: &str) -> Self {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-observability:p1:inst-obs-meter
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-operator-observability:p1:inst-observe-collect-metrics
        // one meter and one prefix for every instrument; the export is OTLP through the
        // `ToolKit`, no Prometheus exporter is created here
        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-admin-observability:p1:inst-obs-open
        // the series are exported over OTLP by the `ToolKit` under `{prefix}_{name}`
        let full = |name: &str| format!("{prefix}_{name}");
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-admin-observability:p1:inst-obs-open
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-operator-observability:p1:inst-observe-collect-metrics
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-observability:p1:inst-obs-meter
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-observability:p1:inst-obs-instruments
        // the declared series are registered here and never recorded; the `Not defined`
        // series have no entry in either table
        let instruments: BTreeMap<String, Instrument> = EMITTED
            .iter()
            .chain(DECLARED_NOT_RECORDED.iter())
            .map(|(name, kind)| {
                let name = full(name);
                (name.clone(), Instrument::register(meter, name, *kind))
            })
            .collect();
        let audit_emit = match instruments.get(&full(AUDIT_EMIT)) {
            Some(Instrument::Counter(counter)) => counter.clone(),
            _ => meter.u64_counter(full(AUDIT_EMIT)).build(),
        };
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-observability:p1:inst-obs-instruments
        // the cost series of the quota rules are recorded on the instruments registered above
        let cost = CostInstruments {
            preflight: registered_counter(&instruments, meter, full("quota_preflight")),
            reserve: registered_counter(&instruments, meter, full("quota_reserve")),
            commit: registered_counter(&instruments, meter, full("quota_commit")),
            overshoot: registered_counter(&instruments, meter, full("quota_overshoot")),
            estimated_tokens: registered_histogram(
                &instruments,
                meter,
                full("quota_estimated_tokens"),
            ),
            actual_tokens: registered_histogram(&instruments, meter, full("quota_actual_tokens")),
            code_interpreter_calls: registered_counter(
                &instruments,
                meter,
                full("code_interpreter_calls"),
            ),
        };
        // the series of the message streaming feature, on the instruments registered above
        let counter = |name: &str| registered_counter(&instruments, meter, full(name));
        let histogram = |name: &str| registered_histogram(&instruments, meter, full(name));
        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-admin-observability:p1:inst-obs-read
        let stream = StreamInstruments {
            stream_started: counter("stream_started"),
            stream_completed: counter("stream_completed"),
            stream_failed: counter("stream_failed"),
            stream_incomplete: counter("stream_incomplete"),
            stream_disconnected: counter("stream_disconnected"),
            active_streams: registered_up_down(&instruments, meter, full("active_streams")),
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
            image_inputs_per_turn: histogram("image_inputs_per_turn"),
            thread_summary_trigger: counter("thread_summary_trigger"),
            finalization_latency_ms: histogram("finalization_latency_ms"),
        };
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-admin-observability:p1:inst-obs-read
        // A counter series is exported only after its first add. The orphan watchdog alert reads
        // the finalized series, which stays unrecorded exactly when the watchdog stalls, so the
        // series is initialised at zero (defence in depth next to the absence-tolerant alert).
        stream
            .orphan_finalized
            .add(0, &[KeyValue::new("reason", "stale_progress")]);
        // the upload-side series of the attachments feature, on the instruments registered above
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-admin-observability:p1:inst-adm-read-metrics
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-metrics:p1:inst-met-labels
        // low-cardinality labels only: `kind`, `result`; never an identifier
        let attachment = AttachmentInstruments {
            upload: counter("attachment_upload"),
            upload_bytes: histogram("attachment_upload_bytes"),
            pending: registered_up_down(&instruments, meter, full("attachments_pending")),
            background: counter("attachment_background_indexing"),
            abandoned: counter("attachment_upload_abandoned"),
            reaper_scan: histogram("upload_reaper_scan_duration_seconds"),
            cleanup_completed: counter("cleanup_completed"),
            cleanup_retry: counter("cleanup_retry"),
            cleanup_failed: counter("cleanup_failed"),
            secondary_skipped: counter("secondary_cleanup_skipped"),
            knowledge: counter("knowledge_search"),
            knowledge_latency: histogram("knowledge_search_latency_ms"),
            knowledge_chunks: histogram("knowledge_search_chunks"),
        };
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-metrics:p1:inst-met-labels
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-admin-observability:p1:inst-adm-read-metrics
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-admin-observability:p1:inst-obs-mutation-counter
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-admin-observability:p1:inst-obs-mutation-latency
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-admin-observability:p1:inst-obs-return
        // `mini_chat_turn_mutation{op, result}` and `mini_chat_turn_mutation_latency_ms{op}`: the
        // labels are low-cardinality and carry no tenant, user, chat or request identifier
        let turn_mutation = counter("turn_mutation");
        let turn_mutation_latency_ms = histogram("turn_mutation_latency_ms");
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-admin-observability:p1:inst-obs-return
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-admin-observability:p1:inst-obs-mutation-latency
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-admin-observability:p1:inst-obs-mutation-counter
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-observability:p1:inst-obs-return
        let maintenance = MaintenanceInstruments {
            execution: counter("thread_summary_execution"),
            cas_conflicts: counter("thread_summary_cas_conflicts"),
            fallback: counter("summary_fallback"),
            debt: counter("cleanup_vector_store_with_failed_attachments"),
        };
        Self {
            instruments,
            audit_emit,
            cost,
            stream,
            attachment,
            maintenance,
            turn_mutation,
            turn_mutation_latency_ms,
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-observability:p1:inst-obs-return
    }

    /// The full instrument names registered by [`new`](Self::new).
    #[must_use]
    pub fn instrument_names(&self) -> Vec<String> {
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-streaming-metrics:p1:inst-met-return
        self.instruments.keys().cloned().collect()
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-streaming-metrics:p1:inst-met-return
    }

    /// The instrument registered under the full name `{prefix}_{name}`, for the feature that
    /// owns it. The caller attaches only the low-cardinality labels of DESIGN 4.
    #[must_use]
    pub fn instrument(&self, full_name: &str) -> Option<&Instrument> {
        self.instruments.get(full_name)
    }

    /// Count one audit delivery outcome in `{prefix}_audit_emit` with the label
    /// `result` (and no other label). Called only by the audit delivery handler.
    pub fn record_audit_emit(&self, result: AuditEmitResult) {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-observability:p1:inst-obs-audit-series
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-operator-observability:p1:inst-observe-audit-health
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-admin-observability:p1:inst-obs-audit-health
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-observability:p1:inst-obs-labels
        // the `result` label is the only label of this series
        self.audit_emit
            .add(1, &[KeyValue::new("result", result.as_str())]);
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-observability:p1:inst-obs-labels
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-admin-observability:p1:inst-obs-audit-health
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-operator-observability:p1:inst-observe-audit-health
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-observability:p1:inst-obs-audit-series
    }
}

/// A duration as milliseconds for a histogram.
fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// The `provider` and `model` labels of the stream series.
fn provider_model(provider: &str, model: &str) -> [KeyValue; 2] {
    [
        KeyValue::new("provider", provider.to_owned()),
        KeyValue::new("model", model.to_owned()),
    ]
}

/// The recording helpers of the message streaming series
/// (`cpt-cf-mini-chat-algo-message-streaming-streaming-metrics`): the low-cardinality labels
/// `provider`, `model`, `reason`, `error_code`, `stage`, `trigger` and `result` only; never a
/// tenant, user, chat or request identifier, a provider response id or free-form error text. The
/// declared-but-unrecorded series stay unrecorded.
impl MiniChatMetrics {
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-streaming-metrics:p1:inst-met-not-recorded
    // `stream_replay` stays declared and unrecorded, and `tool_calls`, `tool_call_limited`,
    // `web_search_latency_ms`, `citations_count`, `citations_by_source`, `context_truncation`,
    // `provider_requests`, `provider_errors`, `provider_latency_ms`, `tokens_after_cancel`,
    // `time_from_ui_disconnect_to_cancel_ms` and `cancel_orphan` have no helper here; the
    // `trigger` values `user_stop`, `timeout` and `internal_abort` are never emitted
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-streaming-metrics:p1:inst-met-not-recorded
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-streaming-metrics:p1:inst-met-cardinality
    // the helpers take no tenant, user, chat or request id, provider response id, file name or
    // free-form error text: only the low-cardinality values of their parameters become labels
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-streaming-metrics:p1:inst-met-cardinality

    /// `mini_chat_stream_started`: a new generation opened its stream.
    pub fn stream_started(&self, provider: &str, model: &str) {
        self.stream
            .stream_started
            .add(1, &provider_model(provider, model));
    }

    /// `mini_chat_stream_completed`: a turn committed as `completed`.
    pub fn stream_completed(&self, provider: &str, model: &str) {
        self.stream
            .stream_completed
            .add(1, &provider_model(provider, model));
    }

    /// `mini_chat_stream_incomplete`: a provider `incomplete` (which also records
    /// `stream_completed`), with the adapter's incomplete reason.
    pub fn stream_incomplete(&self, provider: &str, model: &str, reason: &str) {
        let [provider, model] = provider_model(provider, model);
        self.stream.stream_incomplete.add(
            1,
            &[provider, model, KeyValue::new("reason", reason.to_owned())],
        );
    }

    /// `mini_chat_stream_failed`: a terminal `error` sent by a new generation, with its
    /// streaming error code.
    pub fn stream_failed(&self, provider: &str, model: &str, error_code: &str) {
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-streaming-metrics:p1:inst-met-failed
        let [provider, model] = provider_model(provider, model);
        self.stream.stream_failed.add(
            1,
            &[
                provider,
                model,
                KeyValue::new("error_code", error_code.to_owned()),
            ],
        );
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-streaming-metrics:p1:inst-met-failed
    }

    /// `mini_chat_stream_disconnected` with `stage` `before_first_token` or `mid_stream`.
    pub fn stream_disconnected(&self, stage: &str) {
        self.stream
            .stream_disconnected
            .add(1, &[KeyValue::new("stage", stage.to_owned())]);
    }

    /// `mini_chat_active_streams`: `+1` when a stream opens, `-1` when it closes.
    pub fn active_streams(&self, delta: i64) {
        self.stream.active_streams.add(delta, &[]);
    }

    /// `mini_chat_ttft_provider_ms`: from sending the provider request to its first response
    /// byte.
    pub fn ttft_provider(&self, provider: &str, model: &str, elapsed: Duration) {
        self.stream
            .ttft_provider_ms
            .record(millis(elapsed), &provider_model(provider, model));
    }

    /// `mini_chat_ttft_overhead_ms`: from the provider's first streamed token to its send on the
    /// internal channel.
    pub fn ttft_overhead(&self, provider: &str, model: &str, elapsed: Duration) {
        self.stream
            .ttft_overhead_ms
            .record(millis(elapsed), &provider_model(provider, model));
    }

    /// `mini_chat_stream_total_latency_ms`: from the request receipt to the terminal event.
    pub fn stream_total_latency(&self, provider: &str, model: &str, elapsed: Duration) {
        self.stream
            .stream_total_latency_ms
            .record(millis(elapsed), &provider_model(provider, model));
    }

    /// `mini_chat_cancel_requested` with `trigger` `disconnect`.
    pub fn cancel_requested(&self) {
        self.stream
            .cancel_requested
            .add(1, &[KeyValue::new("trigger", "disconnect")]);
    }

    /// `mini_chat_cancel_effective` with `trigger` `disconnect`.
    pub fn cancel_effective(&self) {
        self.stream
            .cancel_effective
            .add(1, &[KeyValue::new("trigger", "disconnect")]);
    }

    /// `mini_chat_time_to_abort_ms`: from the observed disconnect until the provider stream is
    /// cancelled and the read loop exits (finalization excluded).
    pub fn time_to_abort(&self, elapsed: Duration) {
        self.stream
            .time_to_abort_ms
            .record(millis(elapsed), &[KeyValue::new("trigger", "disconnect")]);
    }

    /// `mini_chat_streams_aborted` with `trigger` `client_disconnect` or `orphan_timeout`.
    pub fn streams_aborted(&self, trigger: &str) {
        self.stream
            .streams_aborted
            .add(1, &[KeyValue::new("trigger", trigger.to_owned())]);
    }

    /// `mini_chat_image_inputs_per_turn`: once per provider request that carries images.
    pub fn image_inputs(&self, count: usize) {
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-streaming-metrics:p1:inst-met-images
        self.stream
            .image_inputs_per_turn
            .record(f64::from(u32::try_from(count).unwrap_or(u32::MAX)), &[]);
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-streaming-metrics:p1:inst-met-images
    }

    /// `mini_chat_finalization_latency_ms`: the duration of a committed finalization.
    pub fn finalization_latency(&self, elapsed: Duration) {
        self.stream
            .finalization_latency_ms
            .record(millis(elapsed), &[]);
    }

    /// `mini_chat_thread_summary_trigger` with `result` `scheduled` or `not_needed`.
    pub fn thread_summary_trigger(&self, result: &str) {
        self.stream
            .thread_summary_trigger
            .add(1, &[KeyValue::new("result", result.to_owned())]);
    }

    /// `mini_chat_orphan_detected` with `reason` `stale_progress`.
    pub fn orphan_detected(&self) {
        self.stream
            .orphan_detected
            .add(1, &[KeyValue::new("reason", "stale_progress")]);
    }

    /// `mini_chat_orphan_finalized` with `reason` `stale_progress`.
    pub fn orphan_finalized(&self) {
        self.stream
            .orphan_finalized
            .add(1, &[KeyValue::new("reason", "stale_progress")]);
    }

    /// `mini_chat_orphan_scan_duration_seconds`: the duration of one scan.
    pub fn orphan_scan_duration(&self, elapsed: Duration) {
        self.stream
            .orphan_scan_duration_seconds
            .record(elapsed.as_secs_f64(), &[]);
    }
}

/// The cost series of the quota rules (`cpt-cf-mini-chat-algo-quota-and-billing-cost-metrics`):
/// `{prefix}_quota_preflight`, `_quota_reserve`, `_quota_commit`, `_quota_overshoot`,
/// `_quota_estimated_tokens`, `_quota_actual_tokens` and `_code_interpreter_calls`, with the
/// labels `decision`, `model`, `tier` and `period` only. The declared-but-unrecorded series stay
/// unrecorded.
impl QuotaMetrics for MiniChatMetrics {
    fn preflight(&self, decision: &str, model: &str, tier: &str) {
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-cost-metrics:p1:inst-cm-naming
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-cost-metrics:p1:inst-cm-preflight
        // low-cardinality labels only: never a tenant, user, chat or request identifier
        self.cost.preflight.add(
            1,
            &[
                KeyValue::new("decision", decision.to_owned()),
                KeyValue::new("model", model.to_owned()),
                KeyValue::new("tier", tier.to_owned()),
            ],
        );
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-cost-metrics:p1:inst-cm-preflight
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-cost-metrics:p1:inst-cm-naming
    }

    fn estimated_tokens(&self, reserve_tokens: i64) {
        self.cost
            .estimated_tokens
            .record(histogram_value(reserve_tokens), &[]);
    }

    fn reserve(&self, period: PeriodType) {
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-cost-metrics:p1:inst-cm-reserve
        self.cost
            .reserve
            .add(1, &[KeyValue::new("period", period.as_str())]);
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-cost-metrics:p1:inst-cm-reserve
    }

    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-cost-metrics:p1:inst-cm-settlement
    fn commit(&self, period: PeriodType) {
        self.cost
            .commit
            .add(1, &[KeyValue::new("period", period.as_str())]);
    }

    fn overshoot(&self, period: PeriodType) {
        self.cost
            .overshoot
            .add(1, &[KeyValue::new("period", period.as_str())]);
    }

    fn actual_tokens(&self, tokens: i64) {
        self.cost.actual_tokens.record(histogram_value(tokens), &[]);
    }

    fn code_interpreter_calls(&self, model: &str, calls: u64) {
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-cost-metrics:p1:inst-cm-not-recorded
        // `quota_preflight_v2`, `quota_tier_downgrade`, `quota_negative`, `quota_overshoot_tokens`,
        // `quota_image_commit`, `credits_overflow` and `unknown_error_code` stay registered and
        // are never recorded here
        self.cost
            .code_interpreter_calls
            .add(calls, &[KeyValue::new("model", model.to_owned())]);
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-cost-metrics:p1:inst-cm-not-recorded
    }
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-cost-metrics:p1:inst-cm-settlement
}

/// The turn mutation series (`cpt-cf-mini-chat-algo-turn-mutations-and-reactions-metrics`).
impl crate::domain::mutations::MutationMetrics for MiniChatMetrics {
    fn mutation(
        &self,
        op: crate::domain::mutations::MutationOp,
        result: crate::domain::mutations::MutationResult,
    ) {
        self.turn_mutation.add(
            1,
            &[
                KeyValue::new("op", op.as_str()),
                KeyValue::new("result", result.as_str()),
            ],
        );
    }

    fn mutation_latency(&self, op: crate::domain::mutations::MutationOp, elapsed: Duration) {
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-metrics:p1:inst-met-return
        self.turn_mutation_latency_ms.record(
            elapsed.as_secs_f64() * 1000.0,
            &[KeyValue::new("op", op.as_str())],
        );
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-metrics:p1:inst-met-return
    }
}

/// The OpenTelemetry trace id (32 lowercase hex characters) of the current span, or `None`
/// when no span is active.
#[must_use]
pub fn current_trace_id() -> Option<String> {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-observability:p1:inst-obs-tracing
    // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-operator-observability:p1:inst-observe-correlate
    let context = tracing::Span::current().context();
    let span = context.span();
    let span_context = span.span_context();
    span_context
        .is_valid()
        .then(|| span_context.trace_id().to_string())
    // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-operator-observability:p1:inst-observe-correlate
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-observability:p1:inst-obs-tracing
}

#[cfg(test)]
#[path = "metrics_tests.rs"]
mod metrics_tests;

#[cfg(test)]
#[path = "quota_metrics_tests.rs"]
mod quota_metrics_tests;

#[cfg(test)]
#[path = "stream_label_tests.rs"]
mod stream_label_tests;
