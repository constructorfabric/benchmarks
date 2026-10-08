//! OpenTelemetry instruments (`mini_chat_*`, DESIGN "Metric series").

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram, UpDownCounter};

/// All recorded instruments.
pub struct Metrics {
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
    orphan_scan_duration: Histogram<f64>,
    upload_abandoned: Counter<u64>,
    upload_reaper_scan_duration: Histogram<f64>,
    background_indexing: Counter<u64>,
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
    cleanup_vs_with_failed: Counter<u64>,
    secondary_cleanup_skipped: Counter<u64>,
    audit_emit: Counter<u64>,
    finalization_latency_ms: Histogram<f64>,
}

impl Metrics {
    /// Build instruments with the given name prefix (default `mini_chat`).
    #[must_use]
    pub fn new(prefix: &str) -> Self {
        let prefix = if prefix.trim().is_empty() { "mini_chat".to_owned() } else { prefix.trim().to_owned() };
        let scope = opentelemetry::InstrumentationScope::builder("mini-chat").build();
        let meter = opentelemetry::global::meter_with_scope(scope);
        let c = |n: &str| meter.u64_counter(format!("{prefix}_{n}")).build();
        let h = |n: &str| meter.f64_histogram(format!("{prefix}_{n}")).build();
        let u = |n: &str| meter.i64_up_down_counter(format!("{prefix}_{n}")).build();
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
            orphan_scan_duration: h("orphan_scan_duration_seconds"),
            upload_abandoned: c("attachment_upload_abandoned"),
            upload_reaper_scan_duration: h("upload_reaper_scan_duration_seconds"),
            background_indexing: c("attachment_background_indexing"),
            quota_preflight: c("quota_preflight"),
            quota_reserve: c("quota_reserve"),
            quota_commit: c("quota_commit"),
            quota_overshoot: c("quota_overshoot"),
            quota_estimated_tokens: h("quota_estimated_tokens"),
            quota_actual_tokens: h("quota_actual_tokens"),
            code_interpreter_calls: c("code_interpreter_calls"),
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
            cleanup_vs_with_failed: c("cleanup_vector_store_with_failed_attachments"),
            secondary_cleanup_skipped: c("secondary_cleanup_skipped"),
            audit_emit: c("audit_emit"),
            finalization_latency_ms: h("finalization_latency_ms"),
        }
    }

    fn pm(provider: &str, model: &str) -> [KeyValue; 2] {
        [KeyValue::new("provider", provider.to_owned()), KeyValue::new("model", model.to_owned())]
    }

    pub fn stream_started(&self, provider: &str, model: &str) {
        self.stream_started.add(1, &Self::pm(provider, model));
        self.active_streams.add(1, &[]);
    }
    pub fn stream_ended(&self) {
        self.active_streams.add(-1, &[]);
    }
    pub fn stream_completed(&self, provider: &str, model: &str) {
        self.stream_completed.add(1, &Self::pm(provider, model));
    }
    pub fn stream_failed(&self, provider: &str, model: &str, code: &str) {
        let [a, b] = Self::pm(provider, model);
        self.stream_failed.add(1, &[a, b, KeyValue::new("error_code", code.to_owned())]);
    }
    pub fn stream_incomplete(&self, provider: &str, model: &str, reason: &str) {
        let [a, b] = Self::pm(provider, model);
        self.stream_incomplete.add(1, &[a, b, KeyValue::new("reason", reason.to_owned())]);
    }
    pub fn stream_disconnected(&self, stage: &'static str) {
        self.stream_disconnected.add(1, &[KeyValue::new("stage", stage)]);
    }
    pub fn ttft(&self, provider: &str, model: &str, provider_ms: f64, overhead_ms: f64) {
        self.ttft_provider_ms.record(provider_ms, &Self::pm(provider, model));
        self.ttft_overhead_ms.record(overhead_ms, &Self::pm(provider, model));
    }
    pub fn total_latency(&self, provider: &str, model: &str, ms: f64) {
        self.stream_total_latency_ms.record(ms, &Self::pm(provider, model));
    }
    pub fn cancel_requested(&self) {
        self.cancel_requested.add(1, &[KeyValue::new("trigger", "disconnect")]);
    }
    pub fn cancel_effective(&self, abort_ms: f64) {
        self.cancel_effective.add(1, &[KeyValue::new("trigger", "disconnect")]);
        self.time_to_abort_ms.record(abort_ms, &[KeyValue::new("trigger", "disconnect")]);
    }
    pub fn stream_aborted(&self, trigger: &'static str) {
        self.streams_aborted.add(1, &[KeyValue::new("trigger", trigger)]);
    }
    pub fn orphan_detected(&self) {
        self.orphan_detected.add(1, &[KeyValue::new("reason", "stale_progress")]);
    }
    pub fn orphan_finalized(&self) {
        self.orphan_finalized.add(1, &[KeyValue::new("reason", "stale_progress")]);
    }
    pub fn orphan_scan(&self, secs: f64) {
        self.orphan_scan_duration.record(secs, &[]);
    }
    pub fn upload_abandoned(&self, from_status: &str) {
        self.upload_abandoned.add(1, &[KeyValue::new("from_status", from_status.to_owned())]);
    }
    pub fn upload_reaper_scan(&self, secs: f64) {
        self.upload_reaper_scan_duration.record(secs, &[]);
    }
    pub fn background_indexing(&self, result: &'static str) {
        self.background_indexing.add(1, &[KeyValue::new("result", result)]);
    }
    pub fn quota_preflight(&self, decision: &str, model: &str, tier: &str) {
        self.quota_preflight.add(
            1,
            &[
                KeyValue::new("decision", decision.to_owned()),
                KeyValue::new("model", model.to_owned()),
                KeyValue::new("tier", tier.to_owned()),
            ],
        );
    }
    pub fn quota_reserve(&self) {
        for p in ["daily", "monthly"] {
            self.quota_reserve.add(1, &[KeyValue::new("period", p)]);
        }
    }
    pub fn quota_commit(&self, overshoot: bool) {
        for p in ["daily", "monthly"] {
            self.quota_commit.add(1, &[KeyValue::new("period", p)]);
            if overshoot {
                self.quota_overshoot.add(1, &[KeyValue::new("period", p)]);
            }
        }
    }
    #[allow(clippy::cast_precision_loss, reason = "metric value")]
    pub fn quota_estimated_tokens(&self, tokens: i64) {
        self.quota_estimated_tokens.record(tokens as f64, &[]);
    }
    #[allow(clippy::cast_precision_loss, reason = "metric value")]
    pub fn quota_actual_tokens(&self, tokens: i64) {
        self.quota_actual_tokens.record(tokens as f64, &[]);
    }
    pub fn code_interpreter_calls(&self, model: &str, n: u64) {
        if n > 0 {
            self.code_interpreter_calls.add(n, &[KeyValue::new("model", model.to_owned())]);
        }
    }
    pub fn thread_summary_trigger(&self, result: &'static str) {
        self.thread_summary_trigger.add(1, &[KeyValue::new("result", result)]);
    }
    pub fn thread_summary_execution(&self, result: &'static str) {
        self.thread_summary_execution.add(1, &[KeyValue::new("result", result)]);
    }
    pub fn thread_summary_cas_conflict(&self) {
        self.thread_summary_cas_conflicts.add(1, &[]);
    }
    pub fn summary_fallback(&self) {
        self.summary_fallback.add(1, &[]);
    }
    pub fn turn_mutation(&self, op: &'static str, result: &'static str, ms: f64) {
        self.turn_mutation.add(1, &[KeyValue::new("op", op), KeyValue::new("result", result)]);
        self.turn_mutation_latency_ms.record(ms, &[KeyValue::new("op", op)]);
    }
    #[allow(clippy::cast_precision_loss, reason = "metric value")]
    pub fn attachment_upload(&self, kind: &str, result: &'static str, bytes: u64) {
        self.attachment_upload.add(1, &[KeyValue::new("kind", kind.to_owned()), KeyValue::new("result", result)]);
        if result == "ok" {
            self.attachment_upload_bytes.record(bytes as f64, &[KeyValue::new("kind", kind.to_owned())]);
        }
    }
    pub fn attachments_pending(&self, delta: i64) {
        self.attachments_pending.add(delta, &[]);
    }
    pub fn image_inputs(&self, n: usize) {
        self.image_inputs_per_turn.record(f64::from(u32::try_from(n).unwrap_or(u32::MAX)), &[]);
    }
    pub fn cleanup_completed(&self, resource: &'static str) {
        self.cleanup_completed.add(1, &[KeyValue::new("resource_type", resource)]);
    }
    pub fn cleanup_failed(&self, resource: &'static str) {
        self.cleanup_failed.add(1, &[KeyValue::new("resource_type", resource)]);
    }
    pub fn cleanup_retry(&self, resource: &'static str, reason: &'static str) {
        self.cleanup_retry.add(1, &[KeyValue::new("resource_type", resource), KeyValue::new("reason", reason)]);
    }
    pub fn cleanup_vs_with_failed(&self) {
        self.cleanup_vs_with_failed.add(1, &[]);
    }
    pub fn secondary_cleanup_skipped(&self, kind: &str) {
        self.secondary_cleanup_skipped.add(1, &[KeyValue::new("provider_kind", kind.to_owned())]);
    }
    pub fn audit_emit(&self, result: &'static str) {
        self.audit_emit.add(1, &[KeyValue::new("result", result)]);
    }
    pub fn finalization_latency(&self, ms: f64) {
        self.finalization_latency_ms.record(ms, &[]);
    }
}
