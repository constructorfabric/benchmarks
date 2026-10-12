//! OpenTelemetry implementation of the domain metrics sink (DESIGN section 3.4 "Metrics (OTLP)").
//!
//! Instrument names are `{prefix}_{name}` (`mini_chat` by default, `metrics.prefix` overrides it);
//! counters carry no `_total` suffix, the exporter conversion decides.

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram, Meter, UpDownCounter};

use crate::domain::metrics::Metrics;

/// Default prefix of the instrument names.
const DEFAULT_PREFIX: &str = "mini_chat";

/// Histogram boundaries of the latency instruments, in milliseconds.
const LATENCY_BUCKETS_MS: [f64; 12] = [
    5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0, 10_000.0, 30_000.0,
];

/// Histogram boundaries of the scan duration, in seconds.
const SCAN_BUCKETS_SECONDS: [f64; 10] = [0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 5.0, 30.0];

/// Histogram boundaries of the token instruments.
const TOKEN_BUCKETS: [f64; 10] = [
    100.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16_000.0, 32_000.0, 64_000.0, 128_000.0,
];

/// The instruments the send pipeline touches.
pub struct OtelMetrics {
    stream_started: Counter<u64>,
    stream_completed: Counter<u64>,
    stream_failed: Counter<u64>,
    stream_incomplete: Counter<u64>,
    stream_disconnected: Counter<u64>,
    active_streams: UpDownCounter<i64>,
    cancel_requested: Counter<u64>,
    cancel_effective: Counter<u64>,
    time_to_abort_ms: Histogram<f64>,
    streams_aborted: Counter<u64>,
    quota_preflight: Counter<u64>,
    quota_reserve: Counter<u64>,
    quota_commit: Counter<u64>,
    quota_overshoot: Counter<u64>,
    quota_estimated_tokens: Histogram<f64>,
    quota_actual_tokens: Histogram<f64>,
    code_interpreter_calls: Counter<u64>,
    finalization_latency_ms: Histogram<f64>,
    ttft_provider_ms: Histogram<f64>,
    stream_total_latency_ms: Histogram<f64>,
    audit_emit: Counter<u64>,
    orphan_detected: Counter<u64>,
    orphan_finalized: Counter<u64>,
    orphan_scan_duration_seconds: Histogram<f64>,
    thread_summary_trigger: Counter<u64>,
    thread_summary_execution: Counter<u64>,
    thread_summary_cas_conflicts: Counter<u64>,
    summary_fallback: Counter<u64>,
    turn_mutation: Counter<u64>,
    turn_mutation_latency_ms: Histogram<f64>,
    image_inputs_per_turn: Histogram<f64>,
    files: FileMetrics,
}

/// Histogram boundaries of the image count per turn.
const IMAGE_COUNT_BUCKETS: [f64; 6] = [1.0, 2.0, 3.0, 4.0, 6.0, 8.0];

/// The instruments of the attachment upload, the upload reaper and the provider cleanup.
struct FileMetrics {
    attachment_upload: Counter<u64>,
    attachment_upload_bytes: Histogram<f64>,
    attachments_pending: UpDownCounter<i64>,
    attachment_background_indexing: Counter<u64>,
    attachment_upload_abandoned: Counter<u64>,
    upload_reaper_scan_duration_seconds: Histogram<f64>,
    cleanup_completed: Counter<u64>,
    cleanup_failed: Counter<u64>,
    cleanup_retry: Counter<u64>,
    cleanup_vector_store_with_failed_attachments: Counter<u64>,
    secondary_cleanup_skipped: Counter<u64>,
}

impl FileMetrics {
    fn new(meter: &Meter, prefix: &str) -> Self {
        Self {
            attachment_upload: counter(
                meter,
                prefix,
                "attachment_upload",
                "Attachment upload requests by kind and result",
            ),
            attachment_upload_bytes: histogram(
                meter,
                prefix,
                "attachment_upload_bytes",
                "Size of uploaded attachments",
                &UPLOAD_BYTES_BUCKETS,
            ),
            attachments_pending: meter
                .i64_up_down_counter(format!("{prefix}_attachments_pending"))
                .with_description("Attachments between the insert and the provider upload")
                .build(),
            attachment_background_indexing: counter(
                meter,
                prefix,
                "attachment_background_indexing",
                "Outcomes of the background indexing task",
            ),
            attachment_upload_abandoned: counter(
                meter,
                prefix,
                "attachment_upload_abandoned",
                "Uploads failed by the upload reaper",
            ),
            upload_reaper_scan_duration_seconds: histogram(
                meter,
                prefix,
                "upload_reaper_scan_duration_seconds",
                "Duration of one upload reaper scan",
                &SCAN_BUCKETS_SECONDS,
            ),
            cleanup_completed: counter(
                meter,
                prefix,
                "cleanup_completed",
                "Successful provider cleanup operations",
            ),
            cleanup_failed: counter(
                meter,
                prefix,
                "cleanup_failed",
                "Terminal provider cleanup failures",
            ),
            cleanup_retry: counter(
                meter,
                prefix,
                "cleanup_retry",
                "Provider cleanup attempts delegated to the outbox retry",
            ),
            cleanup_vector_store_with_failed_attachments: counter(
                meter,
                prefix,
                "cleanup_vector_store_with_failed_attachments",
                "Vector store deletions that left failed attachment cleanups behind",
            ),
            secondary_cleanup_skipped: counter(
                meter,
                prefix,
                "secondary_cleanup_skipped",
                "Secondary file deletions that were skipped",
            ),
        }
    }
}

/// Histogram boundaries of the upload size, in bytes.
const UPLOAD_BYTES_BUCKETS: [f64; 8] = [
    10_240.0,
    102_400.0,
    1_048_576.0,
    5_242_880.0,
    10_485_760.0,
    26_214_400.0,
    52_428_800.0,
    104_857_600.0,
];

fn counter(meter: &Meter, prefix: &str, name: &str, description: &'static str) -> Counter<u64> {
    meter
        .u64_counter(format!("{prefix}_{name}"))
        .with_description(description)
        .build()
}

fn histogram(
    meter: &Meter,
    prefix: &str,
    name: &str,
    description: &'static str,
    boundaries: &[f64],
) -> Histogram<f64> {
    meter
        .f64_histogram(format!("{prefix}_{name}"))
        .with_description(description)
        .with_boundaries(boundaries.to_vec())
        .build()
}

impl OtelMetrics {
    /// Creates every instrument; an empty `prefix` means `mini_chat`.
    #[must_use]
    pub fn new(meter: &Meter, prefix: &str) -> Self {
        let prefix = if prefix.trim().is_empty() {
            DEFAULT_PREFIX
        } else {
            prefix.trim()
        };
        Self {
            stream_started: counter(meter, prefix, "stream_started", "Streams started"),
            stream_completed: counter(meter, prefix, "stream_completed", "Streams completed"),
            stream_failed: counter(meter, prefix, "stream_failed", "Streams failed"),
            stream_incomplete: counter(
                meter,
                prefix,
                "stream_incomplete",
                "Incomplete provider responses",
            ),
            stream_disconnected: counter(
                meter,
                prefix,
                "stream_disconnected",
                "Client disconnects",
            ),
            active_streams: meter
                .i64_up_down_counter(format!("{prefix}_active_streams"))
                .with_description("Streams in flight")
                .build(),
            cancel_requested: counter(meter, prefix, "cancel_requested", "Cancellations requested"),
            cancel_effective: counter(meter, prefix, "cancel_effective", "Cancellations finalized"),
            time_to_abort_ms: histogram(
                meter,
                prefix,
                "time_to_abort_ms",
                "Time from the observed disconnect to the committed cancel",
                &LATENCY_BUCKETS_MS,
            ),
            streams_aborted: counter(meter, prefix, "streams_aborted", "Turns settled as aborted"),
            quota_preflight: counter(
                meter,
                prefix,
                "quota_preflight",
                "Quota preflight decisions",
            ),
            quota_reserve: counter(meter, prefix, "quota_reserve", "Quota reserves written"),
            quota_commit: counter(
                meter,
                prefix,
                "quota_commit",
                "Actual settlements committed",
            ),
            quota_overshoot: counter(
                meter,
                prefix,
                "quota_overshoot",
                "Actual settlements above the reserve",
            ),
            quota_estimated_tokens: histogram(
                meter,
                prefix,
                "quota_estimated_tokens",
                "Reserve tokens of a turn",
                &TOKEN_BUCKETS,
            ),
            quota_actual_tokens: histogram(
                meter,
                prefix,
                "quota_actual_tokens",
                "Provider-reported tokens of an actual settlement",
                &TOKEN_BUCKETS,
            ),
            code_interpreter_calls: counter(
                meter,
                prefix,
                "code_interpreter_calls",
                "Code interpreter calls",
            ),
            finalization_latency_ms: histogram(
                meter,
                prefix,
                "finalization_latency_ms",
                "Finalization transaction latency",
                &LATENCY_BUCKETS_MS,
            ),
            ttft_provider_ms: histogram(
                meter,
                prefix,
                "ttft_provider_ms",
                "Time to the first provider token",
                &LATENCY_BUCKETS_MS,
            ),
            stream_total_latency_ms: histogram(
                meter,
                prefix,
                "stream_total_latency_ms",
                "Total stream latency",
                &LATENCY_BUCKETS_MS,
            ),
            audit_emit: counter(meter, prefix, "audit_emit", "Audit delivery outcomes"),
            orphan_detected: counter(
                meter,
                prefix,
                "orphan_detected",
                "Stale running turns found by the orphan watchdog",
            ),
            orphan_finalized: counter(
                meter,
                prefix,
                "orphan_finalized",
                "Orphan turns finalized by the watchdog",
            ),
            orphan_scan_duration_seconds: histogram(
                meter,
                prefix,
                "orphan_scan_duration_seconds",
                "Duration of one orphan watchdog scan",
                &SCAN_BUCKETS_SECONDS,
            ),
            thread_summary_trigger: counter(
                meter,
                prefix,
                "thread_summary_trigger",
                "Thread summary trigger evaluations",
            ),
            thread_summary_execution: counter(
                meter,
                prefix,
                "thread_summary_execution",
                "Thread summary handler outcomes",
            ),
            thread_summary_cas_conflicts: counter(
                meter,
                prefix,
                "thread_summary_cas_conflicts",
                "Thread summary commits that lost the frontier compare-and-set",
            ),
            summary_fallback: counter(
                meter,
                prefix,
                "summary_fallback",
                "Summary calls that failed and kept the previous summary",
            ),
            turn_mutation: counter(
                meter,
                prefix,
                "turn_mutation",
                "Turn mutations (retry, edit, delete)",
            ),
            turn_mutation_latency_ms: histogram(
                meter,
                prefix,
                "turn_mutation_latency_ms",
                "Turn mutation latency up to the start of the stream",
                &LATENCY_BUCKETS_MS,
            ),
            image_inputs_per_turn: histogram(
                meter,
                prefix,
                "image_inputs_per_turn",
                "Images attached to a turn",
                &IMAGE_COUNT_BUCKETS,
            ),
            files: FileMetrics::new(meter, prefix),
        }
    }
}

fn kv(key: &'static str, value: &str) -> KeyValue {
    KeyValue::new(key, value.to_owned())
}

impl Metrics for OtelMetrics {
    fn stream_started(&self, provider: &str, model: &str) {
        self.stream_started
            .add(1, &[kv("provider", provider), kv("model", model)]);
    }

    fn stream_completed(&self, provider: &str, model: &str) {
        self.stream_completed
            .add(1, &[kv("provider", provider), kv("model", model)]);
    }

    fn stream_failed(&self, provider: &str, model: &str, error_code: &str) {
        self.stream_failed.add(
            1,
            &[
                kv("provider", provider),
                kv("model", model),
                kv("error_code", error_code),
            ],
        );
    }

    fn stream_incomplete(&self, provider: &str, model: &str, reason: &str) {
        self.stream_incomplete.add(
            1,
            &[
                kv("provider", provider),
                kv("model", model),
                kv("reason", reason),
            ],
        );
    }

    fn stream_disconnected(&self, stage: &str) {
        self.stream_disconnected.add(1, &[kv("stage", stage)]);
    }

    fn active_streams(&self, delta: i64) {
        self.active_streams.add(delta, &[]);
    }

    fn cancel_requested(&self, trigger: &str) {
        self.cancel_requested.add(1, &[kv("trigger", trigger)]);
    }

    fn cancel_effective(&self, trigger: &str) {
        self.cancel_effective.add(1, &[kv("trigger", trigger)]);
    }

    fn time_to_abort_ms(&self, trigger: &str, millis: f64) {
        self.time_to_abort_ms
            .record(millis, &[kv("trigger", trigger)]);
    }

    fn streams_aborted(&self, trigger: &str) {
        self.streams_aborted.add(1, &[kv("trigger", trigger)]);
    }

    fn quota_preflight(&self, decision: &str, model: &str, tier: &str) {
        self.quota_preflight.add(
            1,
            &[
                kv("decision", decision),
                kv("model", model),
                kv("tier", tier),
            ],
        );
    }

    fn quota_reserve(&self, period: &str) {
        self.quota_reserve.add(1, &[kv("period", period)]);
    }

    fn quota_commit(&self, period: &str) {
        self.quota_commit.add(1, &[kv("period", period)]);
    }

    fn quota_overshoot(&self, period: &str) {
        self.quota_overshoot.add(1, &[kv("period", period)]);
    }

    fn quota_estimated_tokens(&self, tokens: f64) {
        self.quota_estimated_tokens.record(tokens, &[]);
    }

    fn quota_actual_tokens(&self, tokens: f64) {
        self.quota_actual_tokens.record(tokens, &[]);
    }

    fn code_interpreter_calls(&self, model: &str, calls: u64) {
        self.code_interpreter_calls
            .add(calls, &[kv("model", model)]);
    }

    fn finalization_latency_ms(&self, millis: f64) {
        self.finalization_latency_ms.record(millis, &[]);
    }

    fn ttft_provider_ms(&self, provider: &str, model: &str, millis: f64) {
        self.ttft_provider_ms
            .record(millis, &[kv("provider", provider), kv("model", model)]);
    }

    fn stream_total_latency_ms(&self, provider: &str, model: &str, millis: f64) {
        self.stream_total_latency_ms
            .record(millis, &[kv("provider", provider), kv("model", model)]);
    }

    fn orphan_detected(&self, reason: &str) {
        self.orphan_detected.add(1, &[kv("reason", reason)]);
    }

    fn orphan_finalized(&self, reason: &str) {
        self.orphan_finalized.add(1, &[kv("reason", reason)]);
    }

    fn orphan_scan_duration_seconds(&self, seconds: f64) {
        self.orphan_scan_duration_seconds.record(seconds, &[]);
    }

    fn audit_emit(&self, result: &str) {
        self.audit_emit.add(1, &[kv("result", result)]);
    }

    fn thread_summary_trigger(&self, result: &str) {
        self.thread_summary_trigger.add(1, &[kv("result", result)]);
    }

    fn thread_summary_execution(&self, result: &str) {
        self.thread_summary_execution
            .add(1, &[kv("result", result)]);
    }

    fn thread_summary_cas_conflict(&self) {
        self.thread_summary_cas_conflicts.add(1, &[]);
    }

    fn summary_fallback(&self) {
        self.summary_fallback.add(1, &[]);
    }

    fn turn_mutation(&self, op: &str, result: &str) {
        self.turn_mutation
            .add(1, &[kv("op", op), kv("result", result)]);
    }

    fn turn_mutation_latency_ms(&self, op: &str, millis: f64) {
        self.turn_mutation_latency_ms
            .record(millis, &[kv("op", op)]);
    }

    fn image_inputs_per_turn(&self, count: f64) {
        self.image_inputs_per_turn.record(count, &[]);
    }

    fn attachment_upload(&self, kind: &str, result: &str) {
        self.files
            .attachment_upload
            .add(1, &[kv("kind", kind), kv("result", result)]);
    }

    fn attachment_upload_bytes(&self, kind: &str, bytes: f64) {
        self.files
            .attachment_upload_bytes
            .record(bytes, &[kv("kind", kind)]);
    }

    fn attachments_pending(&self, delta: i64) {
        self.files.attachments_pending.add(delta, &[]);
    }

    fn attachment_background_indexing(&self, result: &str) {
        self.files
            .attachment_background_indexing
            .add(1, &[kv("result", result)]);
    }

    fn attachment_upload_abandoned(&self, from_status: &str) {
        self.files
            .attachment_upload_abandoned
            .add(1, &[kv("from_status", from_status)]);
    }

    fn upload_reaper_scan_duration_seconds(&self, seconds: f64) {
        self.files
            .upload_reaper_scan_duration_seconds
            .record(seconds, &[]);
    }

    fn cleanup_completed(&self, resource_type: &str) {
        self.files
            .cleanup_completed
            .add(1, &[kv("resource_type", resource_type)]);
    }

    fn cleanup_failed(&self, resource_type: &str) {
        self.files
            .cleanup_failed
            .add(1, &[kv("resource_type", resource_type)]);
    }

    fn cleanup_retry(&self, resource_type: &str, reason: &str) {
        self.files.cleanup_retry.add(
            1,
            &[kv("resource_type", resource_type), kv("reason", reason)],
        );
    }

    fn cleanup_vector_store_with_failed_attachments(&self) {
        self.files
            .cleanup_vector_store_with_failed_attachments
            .add(1, &[]);
    }

    fn secondary_cleanup_skipped(&self, provider_kind: &str) {
        self.files
            .secondary_cleanup_skipped
            .add(1, &[kv("provider_kind", provider_kind)]);
    }
}
