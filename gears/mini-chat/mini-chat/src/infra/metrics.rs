//! OpenTelemetry instruments of the gear (D "Metrics (OTLP)", "Emitted"
//! series; S§13).
//!
//! Instrument names are `{prefix}_{series}` with the prefix from
//! `metrics.prefix` (default `mini_chat`); counters carry no `_total`
//! suffix (the OTLP → Prometheus conversion adds it). Labels are
//! low-cardinality only: never tenant, user, chat, request or provider
//! response ids.
//!
//! [`MiniChatMetrics::from_global`] uses the process-global meter provider
//! (a no-op unless the toolkit installed an OTLP exporter);
//! [`MiniChatMetrics::noop`] builds the instruments on a no-op provider.

use std::sync::Arc;
use std::time::Duration;

use opentelemetry::metrics::{Counter, Histogram, Meter, MeterProvider, UpDownCounter};
use opentelemetry::{InstrumentationScope, KeyValue};

/// Instrumentation scope of the gear's meter.
pub const SCOPE: &str = "cf-gears-mini-chat";

/// Every recorded instrument of the gear.
#[derive(Debug)]
pub struct MiniChatMetrics {
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
    attachment_upload_abandoned: Counter<u64>,
    upload_reaper_scan_duration_seconds: Histogram<f64>,
    attachment_background_indexing: Counter<u64>,
    quota_preflight: Counter<u64>,
    quota_reserve: Counter<u64>,
    quota_commit: Counter<u64>,
    quota_overshoot: Counter<u64>,
    quota_estimated_tokens: Histogram<f64>,
    quota_actual_tokens: Histogram<f64>,
    code_interpreter_calls: Counter<u64>,
    thread_summary_trigger: Counter<u64>,
    thread_summary_execution: Counter<u64>,
    thread_summary_cas_conflicts: Counter<u64>,
    summary_fallback: Counter<u64>,
    turn_mutation: Counter<u64>,
    turn_mutation_latency_ms: Histogram<f64>,
    attachment_upload: Counter<u64>,
    attachment_upload_bytes: Histogram<f64>,
    attachments_pending: UpDownCounter<i64>,
    image_inputs_per_turn: Histogram<f64>,
    cleanup_completed: Counter<u64>,
    cleanup_failed: Counter<u64>,
    cleanup_retry: Counter<u64>,
    cleanup_vector_store_with_failed_attachments: Counter<u64>,
    secondary_cleanup_skipped: Counter<u64>,
    audit_emit: Counter<u64>,
    finalization_latency_ms: Histogram<f64>,
    knowledge_search: Counter<u64>,
    knowledge_search_latency_ms: Histogram<f64>,
    knowledge_search_chunks: Histogram<f64>,
}

fn kv(key: &'static str, value: &'static str) -> KeyValue {
    KeyValue::new(key, value)
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Lossy `u64` → `f64` for histogram values (exact below 2^53).
#[allow(clippy::cast_precision_loss)]
fn as_f64(v: u64) -> f64 {
    v as f64
}

impl MiniChatMetrics {
    /// Declare every instrument on `meter` with `prefix`.
    #[must_use]
    pub fn new(meter: &Meter, prefix: &str) -> Self {
        let counter = |name: &str, desc: &'static str| {
            meter
                .u64_counter(format!("{prefix}_{name}"))
                .with_description(desc)
                .build()
        };
        let histogram = |name: &str, desc: &'static str| {
            meter
                .f64_histogram(format!("{prefix}_{name}"))
                .with_description(desc)
                .build()
        };
        Self {
            stream_started: counter("stream_started", "Provider streams started"),
            stream_completed: counter(
                "stream_completed",
                "Streams finalized completed (incomplete included)",
            ),
            stream_failed: counter("stream_failed", "Streams finalized failed, by error code"),
            stream_incomplete: counter(
                "stream_incomplete",
                "Streams that ended with response.incomplete, by reason",
            ),
            stream_disconnected: counter(
                "stream_disconnected",
                "Client disconnects during a stream, by stage",
            ),
            active_streams: meter
                .i64_up_down_counter(format!("{prefix}_active_streams"))
                .with_description("Provider streams in progress")
                .build(),
            ttft_provider_ms: histogram(
                "ttft_provider_ms",
                "From the provider call to the first text delta (ms)",
            ),
            ttft_overhead_ms: histogram(
                "ttft_overhead_ms",
                "From the first provider event to its first send on the SSE channel (ms)",
            ),
            stream_total_latency_ms: histogram(
                "stream_total_latency_ms",
                "From the provider call to the terminal outcome (ms)",
            ),
            cancel_requested: counter(
                "cancel_requested",
                "Disconnects observed through the cancellation token",
            ),
            cancel_effective: counter(
                "cancel_effective",
                "Cancelled turns whose provider stream was cancelled and finalized",
            ),
            time_to_abort_ms: histogram(
                "time_to_abort_ms",
                "From the observed disconnect until the provider stream is cancelled (ms)",
            ),
            streams_aborted: counter("streams_aborted", "Streams aborted, by trigger"),
            orphan_detected: counter(
                "orphan_detected",
                "Orphan turn candidates found by the watchdog",
            ),
            orphan_finalized: counter(
                "orphan_finalized",
                "Orphan turns finalized by the watchdog (CAS won)",
            ),
            orphan_scan_duration_seconds: histogram(
                "orphan_scan_duration_seconds",
                "Orphan watchdog scan duration (s)",
            ),
            attachment_upload_abandoned: counter(
                "attachment_upload_abandoned",
                "Uploads marked upload_abandoned by the reaper, by previous status",
            ),
            upload_reaper_scan_duration_seconds: histogram(
                "upload_reaper_scan_duration_seconds",
                "Upload reaper scan duration (s)",
            ),
            attachment_background_indexing: counter(
                "attachment_background_indexing",
                "Outcomes of background document indexing",
            ),
            quota_preflight: counter("quota_preflight", "Quota preflight decisions"),
            quota_reserve: counter("quota_reserve", "Quota reserves taken, per period"),
            quota_commit: counter("quota_commit", "Actual quota settlements, per period"),
            quota_overshoot: counter(
                "quota_overshoot",
                "Actual settlements above the reserve, per period",
            ),
            quota_estimated_tokens: histogram(
                "quota_estimated_tokens",
                "Reserve tokens of a turn after preflight",
            ),
            quota_actual_tokens: histogram(
                "quota_actual_tokens",
                "Provider-reported tokens of actual settlements",
            ),
            code_interpreter_calls: counter(
                "code_interpreter_calls",
                "Code interpreter calls of actual settlements",
            ),
            thread_summary_trigger: counter(
                "thread_summary_trigger",
                "Thread summary trigger evaluations, by result",
            ),
            thread_summary_execution: counter(
                "thread_summary_execution",
                "Thread summary task executions, by result",
            ),
            thread_summary_cas_conflicts: counter(
                "thread_summary_cas_conflicts",
                "Thread summary commits lost to a concurrent frontier change",
            ),
            summary_fallback: counter(
                "summary_fallback",
                "Thread summary provider failures (previous summary kept)",
            ),
            turn_mutation: counter("turn_mutation", "Turn mutations, by op and result"),
            turn_mutation_latency_ms: histogram(
                "turn_mutation_latency_ms",
                "Turn mutation latency (ms), by op",
            ),
            attachment_upload: counter("attachment_upload", "Uploads, by kind and result"),
            attachment_upload_bytes: histogram(
                "attachment_upload_bytes",
                "Size of accepted uploads (bytes), by kind",
            ),
            attachments_pending: meter
                .i64_up_down_counter(format!("{prefix}_attachments_pending"))
                .with_description(
                    "Uploads this process is processing (pending / uploaded: request or \
                     background indexing)",
                )
                .build(),
            image_inputs_per_turn: histogram(
                "image_inputs_per_turn",
                "Images sent to the provider per turn",
            ),
            cleanup_completed: counter(
                "cleanup_completed",
                "Provider resources deleted, by resource type",
            ),
            cleanup_failed: counter(
                "cleanup_failed",
                "Provider cleanups that became terminal failures, by resource type",
            ),
            cleanup_retry: counter(
                "cleanup_retry",
                "Provider cleanups to be retried, by resource type and reason",
            ),
            cleanup_vector_store_with_failed_attachments: counter(
                "cleanup_vector_store_with_failed_attachments",
                "Vector stores deleted while some attachment cleanups failed",
            ),
            secondary_cleanup_skipped: counter(
                "secondary_cleanup_skipped",
                "Secondary provider file deletes skipped, by provider kind",
            ),
            audit_emit: counter("audit_emit", "Audit delivery outcomes, by result"),
            finalization_latency_ms: histogram(
                "finalization_latency_ms",
                "Duration of a stream finalization (ms)",
            ),
            knowledge_search: counter("knowledge_search", "Knowledge searches, by result"),
            knowledge_search_latency_ms: histogram(
                "knowledge_search_latency_ms",
                "Knowledge search latency (ms)",
            ),
            knowledge_search_chunks: histogram(
                "knowledge_search_chunks",
                "Chunks returned by a successful knowledge search",
            ),
        }
    }

    /// Instruments on the process-global meter provider.
    #[must_use]
    pub fn from_global(prefix: &str) -> Self {
        let scope = InstrumentationScope::builder(SCOPE).build();
        Self::new(&opentelemetry::global::meter_with_scope(scope), prefix)
    }

    /// Instruments on a no-op provider (tests, defaults).
    #[must_use]
    pub fn noop() -> Self {
        let provider = opentelemetry::metrics::NoopMeterProvider::new();
        Self::new(&provider.meter(SCOPE), "mini_chat")
    }

    // -- Streaming ----------------------------------------------------------

    /// A provider stream started (`stream_started`, `active_streams` +1).
    pub fn stream_started(&self, provider: &str, model: &str) {
        let labels = [
            KeyValue::new("provider", provider.to_owned()),
            KeyValue::new("model", model.to_owned()),
        ];
        self.stream_started.add(1, &labels);
        self.active_streams.add(1, &[]);
    }

    /// The provider stream of a turn ended (`active_streams` -1, total latency,
    /// TTFT when a text delta arrived).
    pub fn stream_ended(&self, provider: &str, model: &str, total: Duration, ttft_ms: Option<u64>) {
        let labels = [
            KeyValue::new("provider", provider.to_owned()),
            KeyValue::new("model", model.to_owned()),
        ];
        self.active_streams.add(-1, &[]);
        self.stream_total_latency_ms.record(ms(total), &labels);
        if let Some(t) = ttft_ms {
            self.ttft_provider_ms.record(as_f64(t), &labels);
        }
    }

    /// A turn was finalized `completed`; `incomplete` carries the reason of a
    /// `response.incomplete`.
    pub fn stream_completed(&self, provider: &str, model: &str, incomplete: Option<&str>) {
        let labels = [
            KeyValue::new("provider", provider.to_owned()),
            KeyValue::new("model", model.to_owned()),
        ];
        self.stream_completed.add(1, &labels);
        if let Some(reason) = incomplete {
            let mut with_reason = labels.to_vec();
            with_reason.push(KeyValue::new("reason", reason.to_owned()));
            self.stream_incomplete.add(1, &with_reason);
        }
    }

    /// A turn was finalized `failed` with a streaming error code.
    pub fn stream_failed(&self, provider: &str, model: &str, error_code: &str) {
        self.stream_failed.add(
            1,
            &[
                KeyValue::new("provider", provider.to_owned()),
                KeyValue::new("model", model.to_owned()),
                KeyValue::new("error_code", error_code.to_owned()),
            ],
        );
    }

    /// Gear overhead of the first content event: from the first provider
    /// event to its first send on the internal SSE channel.
    pub fn ttft_overhead(&self, provider: &str, model: &str, d: Duration) {
        self.ttft_overhead_ms.record(
            ms(d),
            &[
                KeyValue::new("provider", provider.to_owned()),
                KeyValue::new("model", model.to_owned()),
            ],
        );
    }

    /// The client disconnected; `before_first_token` selects the stage.
    pub fn stream_disconnected(&self, before_first_token: bool) {
        let stage = if before_first_token {
            "before_first_token"
        } else {
            "mid_stream"
        };
        self.stream_disconnected.add(1, &[kv("stage", stage)]);
    }

    /// A disconnect seen through the cancellation token.
    pub fn cancel_requested(&self) {
        self.cancel_requested
            .add(1, &[kv("trigger", "client_disconnect")]);
    }

    /// A cancelled turn was finalized; `abort` = disconnect → read loop exit.
    pub fn cancel_effective(&self, abort: Duration) {
        let labels = [kv("trigger", "client_disconnect")];
        self.cancel_effective.add(1, &labels);
        self.time_to_abort_ms.record(ms(abort), &labels);
    }

    /// A stream was aborted (`client_disconnect` | `orphan_timeout`).
    pub fn stream_aborted(&self, trigger: &'static str) {
        self.streams_aborted.add(1, &[kv("trigger", trigger)]);
    }

    /// Duration of one stream finalization.
    pub fn finalization_latency(&self, d: Duration) {
        self.finalization_latency_ms.record(ms(d), &[]);
    }

    // -- Orphan watchdog / upload reaper -------------------------------------

    /// An orphan candidate (stale progress) was found.
    pub fn orphan_detected(&self) {
        self.orphan_detected
            .add(1, &[kv("reason", "stale_progress")]);
    }

    /// An orphan turn was finalized (after the orphan CAS committed).
    pub fn orphan_finalized(&self) {
        self.orphan_finalized
            .add(1, &[kv("reason", "stale_progress")]);
        self.stream_aborted("orphan_timeout");
    }

    /// Duration of one watchdog scan.
    pub fn orphan_scan_duration(&self, d: Duration) {
        self.orphan_scan_duration_seconds
            .record(d.as_secs_f64(), &[]);
    }

    /// An upload was marked `upload_abandoned` (`pending` | `uploaded`).
    pub fn upload_abandoned(&self, from_status: &'static str) {
        self.attachment_upload_abandoned
            .add(1, &[kv("from_status", from_status)]);
    }

    /// Duration of one reaper scan.
    pub fn upload_reaper_scan_duration(&self, d: Duration) {
        self.upload_reaper_scan_duration_seconds
            .record(d.as_secs_f64(), &[]);
    }

    // -- Attachments ----------------------------------------------------------

    /// Background indexing outcome (`ready` | `failed` | `timeout` |
    /// `set_ready_failed`).
    pub fn background_indexing(&self, result: &'static str) {
        self.attachment_background_indexing
            .add(1, &[kv("result", result)]);
    }

    /// An upload finished with `result`; `bytes` of an accepted upload.
    pub fn attachment_upload(&self, kind: &'static str, result: &'static str, bytes: Option<u64>) {
        self.attachment_upload
            .add(1, &[kv("kind", kind), kv("result", result)]);
        if let Some(b) = bytes {
            self.attachment_upload_bytes
                .record(as_f64(b), &[kv("kind", kind)]);
        }
    }

    /// An upload entered processing (`attachments_pending` +1); dropping the
    /// guard ends it (-1), so every path out (ready, failed, abandoned
    /// task, panic) is balanced.
    #[must_use = "the attachment stops counting as pending when the guard is dropped"]
    pub fn attachment_pending(self: &Arc<Self>) -> PendingAttachment {
        self.attachments_pending.add(1, &[]);
        PendingAttachment {
            metrics: Arc::clone(self),
        }
    }

    /// Images sent with one turn.
    pub fn image_inputs(&self, count: u32) {
        self.image_inputs_per_turn.record(f64::from(count), &[]);
    }

    // -- Quota ------------------------------------------------------------------

    /// Preflight decision (`allow` | `downgrade`) with the effective model.
    pub fn quota_preflight(&self, decision: &'static str, model: &str, tier: &'static str) {
        self.quota_preflight.add(
            1,
            &[
                kv("decision", decision),
                KeyValue::new("model", model.to_owned()),
                kv("tier", tier),
            ],
        );
    }

    /// Reserve tokens of an allowed / downgraded preflight.
    pub fn quota_estimated_tokens(&self, reserve_tokens: i64) {
        #[allow(clippy::cast_precision_loss)]
        self.quota_estimated_tokens
            .record(reserve_tokens.max(0) as f64, &[]);
    }

    /// A reserve was written (one per period).
    pub fn quota_reserved(&self) {
        for period in ["daily", "monthly"] {
            self.quota_reserve.add(1, &[kv("period", period)]);
        }
    }

    /// An actual settlement (per period), its tokens and code interpreter calls.
    pub fn quota_actual_settlement(
        &self,
        tokens: Option<i64>,
        overshoot: bool,
        model: &str,
        code_interpreter_calls: u32,
    ) {
        for period in ["daily", "monthly"] {
            self.quota_commit.add(1, &[kv("period", period)]);
            if overshoot {
                self.quota_overshoot.add(1, &[kv("period", period)]);
            }
        }
        if let Some(t) = tokens {
            #[allow(clippy::cast_precision_loss)]
            self.quota_actual_tokens.record(t.max(0) as f64, &[]);
        }
        if code_interpreter_calls > 0 {
            self.code_interpreter_calls.add(
                u64::from(code_interpreter_calls),
                &[KeyValue::new("model", model.to_owned())],
            );
        }
    }

    // -- Thread summary ----------------------------------------------------------

    /// Trigger evaluated after a finalization commit (`scheduled` | `not_needed`).
    pub fn summary_trigger(&self, result: &'static str) {
        self.thread_summary_trigger.add(1, &[kv("result", result)]);
    }

    /// Summary task execution result.
    pub fn summary_execution(&self, result: &'static str) {
        self.thread_summary_execution
            .add(1, &[kv("result", result)]);
    }

    /// Summary commit lost the frontier CAS.
    pub fn summary_cas_conflict(&self) {
        self.thread_summary_cas_conflicts.add(1, &[]);
    }

    /// Summary provider failure (previous summary kept).
    pub fn summary_fallback(&self) {
        self.summary_fallback.add(1, &[]);
    }

    // -- Turn mutations ------------------------------------------------------------

    /// A turn mutation (`retry` | `edit` | `delete`) finished with `result`.
    pub fn turn_mutation(&self, op: &'static str, result: &'static str, latency: Duration) {
        self.turn_mutation
            .add(1, &[kv("op", op), kv("result", result)]);
        self.turn_mutation_latency_ms
            .record(ms(latency), &[kv("op", op)]);
    }

    // -- Cleanup / audit -------------------------------------------------------------

    /// A provider resource (`file` | `vector_store`) was deleted.
    pub fn cleanup_completed(&self, resource_type: &'static str) {
        self.cleanup_completed
            .add(1, &[kv("resource_type", resource_type)]);
    }

    /// A provider cleanup became a terminal failure.
    pub fn cleanup_failed(&self, resource_type: &'static str) {
        self.cleanup_failed
            .add(1, &[kv("resource_type", resource_type)]);
    }

    /// A provider cleanup will be retried (`provider_error` |
    /// `vector_store_delete_failed`).
    pub fn cleanup_retry(&self, resource_type: &'static str, reason: &'static str) {
        self.cleanup_retry.add(
            1,
            &[kv("resource_type", resource_type), kv("reason", reason)],
        );
    }

    /// A vector store was deleted while some attachment cleanups failed.
    pub fn cleanup_vector_store_with_failed_attachments(&self) {
        self.cleanup_vector_store_with_failed_attachments
            .add(1, &[]);
    }

    /// A secondary provider file delete was skipped.
    pub fn secondary_cleanup_skipped(&self, provider_kind: &str) {
        self.secondary_cleanup_skipped.add(
            1,
            &[KeyValue::new("provider_kind", provider_kind.to_owned())],
        );
    }

    /// One knowledge search (`ok` | `error`); `chunks` of a successful one.
    pub fn knowledge_search(&self, result: &'static str, latency: Duration, chunks: Option<usize>) {
        self.knowledge_search.add(1, &[kv("result", result)]);
        self.knowledge_search_latency_ms.record(ms(latency), &[]);
        if let Some(n) = chunks {
            self.knowledge_search_chunks
                .record(as_f64(u64::try_from(n).unwrap_or(u64::MAX)), &[]);
        }
    }

    /// Audit delivery outcome (`ok` | `retry` | `reject` | `dropped`).
    pub fn audit_emit(&self, result: &'static str) {
        self.audit_emit.add(1, &[kv("result", result)]);
    }
}

/// One upload counted in `attachments_pending` while alive.
#[derive(Debug)]
pub struct PendingAttachment {
    metrics: Arc<MiniChatMetrics>,
}

impl Drop for PendingAttachment {
    fn drop(&mut self) {
        self.metrics.attachments_pending.add(-1, &[]);
    }
}

impl Default for MiniChatMetrics {
    fn default() -> Self {
        Self::noop()
    }
}

#[cfg(test)]
#[path = "metrics_tests.rs"]
mod tests;
