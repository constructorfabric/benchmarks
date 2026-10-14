//! The metric ports of the two handlers
//! (`cpt-cf-mini-chat-algo-background-maintenance-metrics`): low-cardinality labels only, never a
//! tenant, user, chat, message, attachment, request, file or vector store identifier and never
//! free-form error text. `infra::metrics::maintenance` implements them for the OTLP instruments.

// @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-metrics:p1:inst-mt-labels
// only low-cardinality label values are defined here: a `result`, a `resource_type`, a `reason` and
// a `provider_kind`; no tenant, user, chat, message, attachment, request, file or vector store
// identifier and no free-form error text ever becomes a label
// @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-metrics:p1:inst-mt-labels
// @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-metrics:p1:inst-mt-execution
/// `result` values of `mini_chat_thread_summary_execution`.
pub const RESULT_SUCCESS: &str = "success";
pub const RESULT_PROVIDER_ERROR: &str = "provider_error";
pub const RESULT_EMPTY_SUMMARY: &str = "empty_summary";
pub const RESULT_RETRY: &str = "retry";
pub const RESULT_MODEL_UNAVAILABLE: &str = "model_unavailable";
pub const RESULT_FRONTIER_DELETED: &str = "frontier_deleted";
pub const RESULT_BASE_MISSING: &str = "base_missing";
// @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-metrics:p1:inst-mt-execution

/// `resource_type` values of the cleanup series.
pub const RESOURCE_FILE: &str = "file";
pub const RESOURCE_VECTOR_STORE: &str = "vector_store";

/// `reason` of `mini_chat_cleanup_retry` for a failed provider file delete.
pub const REASON_PROVIDER_ERROR: &str = "provider_error";
/// `reason` of `mini_chat_cleanup_retry` for a failed vector store delete.
pub const REASON_VECTOR_STORE_DELETE_FAILED: &str = "vector_store_delete_failed";

/// The thread-summary series.
pub trait SummaryMetrics: Send + Sync {
    /// `mini_chat_thread_summary_execution{result}`: exactly one per decided delivery; a delivery
    /// that ends in a CAS conflict, a deleted chat, an empty range or a malformed payload records
    /// no result.
    fn execution(&self, result: &str);

    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-metrics:p1:inst-mt-cas
    /// `mini_chat_thread_summary_cas_conflicts`: the delivery lost the CAS precondition.
    fn cas_conflict(&self);
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-metrics:p1:inst-mt-cas

    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-metrics:p1:inst-mt-fallback
    /// `mini_chat_summary_fallback`: the summary provider call failed (never for an empty
    /// summary, a missing model, another resolution failure or a failed commit).
    fn fallback(&self);
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-metrics:p1:inst-mt-fallback
}

// @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-metrics:p1:inst-mt-deferred
// @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-metrics:p1:inst-mt-trigger-elsewhere
// `cleanup_backlog`, `summary_regen` and the `outbox_*` series are declared and not recorded: queue
// lag, retry backlog, lease churn and dead-letter depth are read from the shared outbox surface;
// `mini_chat_thread_summary_trigger` belongs to the trigger of the message streaming feature
// @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-metrics:p1:inst-mt-trigger-elsewhere
// @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-metrics:p1:inst-mt-deferred
// @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-metrics:p1:inst-mt-cleanup
/// The chat-cleanup series.
pub trait CleanupMetrics: Send + Sync {
    /// `mini_chat_cleanup_completed{resource_type}`.
    fn completed(&self, resource_type: &str);

    /// `mini_chat_cleanup_retry{resource_type, reason}`.
    fn retry(&self, resource_type: &str, reason: &str);

    /// `mini_chat_cleanup_failed{resource_type}`.
    fn failed(&self, resource_type: &str);

    /// `mini_chat_cleanup_vector_store_with_failed_attachments`.
    fn vector_store_with_failed_attachments(&self);

    /// `mini_chat_secondary_cleanup_skipped{provider_kind}`.
    fn secondary_skipped(&self, provider_kind: &str);
}
// @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-metrics:p1:inst-mt-cleanup
