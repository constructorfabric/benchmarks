//! The metrics sink of the domain (DESIGN section 3.4 "Metrics (OTLP)"). The infrastructure layer
//! implements it with OpenTelemetry instruments; label values are low-cardinality by construction.

/// Records the `mini_chat_*` instruments the send pipeline touches.
pub trait Metrics: Send + Sync {
    /// `stream_started{provider, model}`.
    fn stream_started(&self, provider: &str, model: &str);
    /// `stream_completed{provider, model}`.
    fn stream_completed(&self, provider: &str, model: &str);
    /// `stream_failed{provider, model, error_code}`.
    fn stream_failed(&self, provider: &str, model: &str, error_code: &str);
    /// `stream_incomplete{provider, model, reason}`.
    fn stream_incomplete(&self, provider: &str, model: &str, reason: &str);
    /// `stream_disconnected{stage}`: `before_first_token` or `mid_stream`.
    fn stream_disconnected(&self, stage: &str);
    /// `active_streams` up-down counter.
    fn active_streams(&self, delta: i64);
    /// `cancel_requested{trigger}`.
    fn cancel_requested(&self, trigger: &str);
    /// `cancel_effective{trigger}`.
    fn cancel_effective(&self, trigger: &str);
    /// `time_to_abort_ms{trigger}`.
    fn time_to_abort_ms(&self, trigger: &str, millis: f64);
    /// `streams_aborted{trigger}`.
    fn streams_aborted(&self, trigger: &str);
    /// `quota_preflight{decision, model, tier}`.
    fn quota_preflight(&self, decision: &str, model: &str, tier: &str);
    /// `quota_reserve{period}`.
    fn quota_reserve(&self, period: &str);
    /// `quota_commit{period}`: actual settlements only.
    fn quota_commit(&self, period: &str);
    /// `quota_overshoot{period}`.
    fn quota_overshoot(&self, period: &str);
    /// `quota_estimated_tokens`: the turn's `reserve_tokens`.
    fn quota_estimated_tokens(&self, tokens: f64);
    /// `quota_actual_tokens`: actual settlements only.
    fn quota_actual_tokens(&self, tokens: f64);
    /// `code_interpreter_calls{model}`: actual settlements only.
    fn code_interpreter_calls(&self, model: &str, calls: u64);
    /// `finalization_latency_ms`.
    fn finalization_latency_ms(&self, millis: f64);
    /// `ttft_provider_ms{provider, model}`.
    fn ttft_provider_ms(&self, provider: &str, model: &str, millis: f64);
    /// `stream_total_latency_ms{provider, model}`.
    fn stream_total_latency_ms(&self, provider: &str, model: &str, millis: f64);
    /// `orphan_detected{reason}`.
    fn orphan_detected(&self, reason: &str);
    /// `orphan_finalized{reason}`: after the conditional update succeeded.
    fn orphan_finalized(&self, reason: &str);
    /// `orphan_scan_duration_seconds`.
    fn orphan_scan_duration_seconds(&self, seconds: f64);
    /// `audit_emit{result}`: `ok`, `retry`, `reject` or `dropped`.
    fn audit_emit(&self, result: &str);
    /// `thread_summary_trigger{result}`: `scheduled` or `not_needed`, after the finalization
    /// commit of a turn whose trigger was evaluated.
    fn thread_summary_trigger(&self, _result: &str) {}
    /// `thread_summary_execution{result}`: `success`, `provider_error`, `empty_summary`, `retry`,
    /// `model_unavailable`, `frontier_deleted` or `base_missing`.
    fn thread_summary_execution(&self, _result: &str) {}
    /// `thread_summary_cas_conflicts`: the frontier already moved (pre-check or commit).
    fn thread_summary_cas_conflict(&self) {}
    /// `summary_fallback`: the summary call failed and the previous summary is kept.
    fn summary_fallback(&self) {}
    /// `turn_mutation{op, result}`: `retry`, `edit` or `delete` with `ok`, `rejected`,
    /// `conflict` or `setup_failed`.
    fn turn_mutation(&self, _op: &str, _result: &str) {}
    /// `turn_mutation_latency_ms{op}`.
    fn turn_mutation_latency_ms(&self, _op: &str, _millis: f64) {}
    /// `image_inputs_per_turn`: images of a turn that passed the image guards (turns without
    /// images record nothing).
    fn image_inputs_per_turn(&self, _count: f64) {}
    /// `attachment_upload{kind, result}`: `ok` or the failure class of an upload request.
    fn attachment_upload(&self, _kind: &str, _result: &str) {}
    /// `attachment_upload_bytes{kind}`.
    fn attachment_upload_bytes(&self, _kind: &str, _bytes: f64) {}
    /// `attachments_pending` up-down counter: rows between the insert and the provider upload.
    fn attachments_pending(&self, _delta: i64) {}
    /// `attachment_background_indexing{result}`: `ready`, `failed`, `timeout` or
    /// `set_ready_failed`.
    fn attachment_background_indexing(&self, _result: &str) {}
    /// `attachment_upload_abandoned{from_status}`: after the reaper's transaction committed.
    fn attachment_upload_abandoned(&self, _from_status: &str) {}
    /// `upload_reaper_scan_duration_seconds`.
    fn upload_reaper_scan_duration_seconds(&self, _seconds: f64) {}
    /// `cleanup_completed{resource_type}`: `file` or `vector_store`.
    fn cleanup_completed(&self, _resource_type: &str) {}
    /// `cleanup_failed{resource_type}`: a terminal cleanup failure.
    fn cleanup_failed(&self, _resource_type: &str) {}
    /// `cleanup_retry{resource_type, reason}`: `provider_error` or `vector_store_delete_failed`.
    fn cleanup_retry(&self, _resource_type: &str, _reason: &str) {}
    /// `cleanup_vector_store_with_failed_attachments`.
    fn cleanup_vector_store_with_failed_attachments(&self) {}
    /// `secondary_cleanup_skipped{provider_kind}`.
    fn secondary_cleanup_skipped(&self, _provider_kind: &str) {}
}

/// A sink that records nothing, for unit tests of code that only needs a `Metrics` value.
#[cfg(test)]
pub(crate) struct NullMetrics;

#[cfg(test)]
impl Metrics for NullMetrics {
    fn stream_started(&self, _: &str, _: &str) {}
    fn stream_completed(&self, _: &str, _: &str) {}
    fn stream_failed(&self, _: &str, _: &str, _: &str) {}
    fn stream_incomplete(&self, _: &str, _: &str, _: &str) {}
    fn stream_disconnected(&self, _: &str) {}
    fn active_streams(&self, _: i64) {}
    fn cancel_requested(&self, _: &str) {}
    fn cancel_effective(&self, _: &str) {}
    fn time_to_abort_ms(&self, _: &str, _: f64) {}
    fn streams_aborted(&self, _: &str) {}
    fn quota_preflight(&self, _: &str, _: &str, _: &str) {}
    fn quota_reserve(&self, _: &str) {}
    fn quota_commit(&self, _: &str) {}
    fn quota_overshoot(&self, _: &str) {}
    fn quota_estimated_tokens(&self, _: f64) {}
    fn quota_actual_tokens(&self, _: f64) {}
    fn code_interpreter_calls(&self, _: &str, _: u64) {}
    fn finalization_latency_ms(&self, _: f64) {}
    fn ttft_provider_ms(&self, _: &str, _: &str, _: f64) {}
    fn stream_total_latency_ms(&self, _: &str, _: &str, _: f64) {}
    fn orphan_detected(&self, _: &str) {}
    fn orphan_finalized(&self, _: &str) {}
    fn orphan_scan_duration_seconds(&self, _: f64) {}
    fn audit_emit(&self, _: &str) {}
}
