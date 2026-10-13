//! The worker metrics port (`cpt-cf-mini-chat-algo-background-workers-record-metrics`):
//! the orphan, cleanup and thread-summary groups of DESIGN "Metric series".
//! Only the labels `reason`, `resource_type`, `trigger`, `provider_kind` and
//! `result` with the listed values ever exist.

use toolkit_macros::domain_model;

/// `resource_type` label of the cleanup instruments.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupResource {
    /// A provider file.
    File,
    /// The chat vector store.
    VectorStore,
}

impl CleanupResource {
    /// Label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::VectorStore => "vector_store",
        }
    }

    /// `reason` label of `mini_chat_cleanup_retry`.
    #[must_use]
    pub const fn retry_reason(self) -> &'static str {
        match self {
            Self::File => "provider_error",
            Self::VectorStore => "vector_store_delete_failed",
        }
    }
}

/// `result` label of `mini_chat_thread_summary_execution`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryResult {
    /// The summary was committed.
    Success,
    /// The provider call failed.
    ProviderError,
    /// The answer had no usable summary.
    EmptySummary,
    /// A transient failure; the message is retried.
    Retry,
    /// The summary model is missing or disabled.
    ModelUnavailable,
    /// The frozen target was deleted.
    FrontierDeleted,
    /// The summary the task was based on is gone.
    BaseMissing,
}

impl SummaryResult {
    /// Label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::ProviderError => "provider_error",
            Self::EmptySummary => "empty_summary",
            Self::Retry => "retry",
            Self::ModelUnavailable => "model_unavailable",
            Self::FrontierDeleted => "frontier_deleted",
            Self::BaseMissing => "base_missing",
        }
    }
}

/// The instruments of the background workers.
pub trait WorkerMetrics: Send + Sync {
    /// `mini_chat_orphan_detected{reason="stale_progress"}`.
    fn orphan_detected(&self);
    /// `mini_chat_orphan_finalized{reason="stale_progress"}`.
    fn orphan_finalized(&self);
    /// `mini_chat_streams_aborted{trigger="orphan_timeout"}`.
    fn orphan_stream_aborted(&self);
    /// `mini_chat_orphan_scan_duration_seconds`.
    fn orphan_scan_seconds(&self, seconds: f64);
    /// `mini_chat_cleanup_completed{resource_type}`.
    fn chat_cleanup_completed(&self, resource: CleanupResource);
    /// `mini_chat_cleanup_failed{resource_type}`.
    fn chat_cleanup_failed(&self, resource: CleanupResource);
    /// `mini_chat_cleanup_retry{resource_type,reason}`.
    fn chat_cleanup_retry(&self, resource: CleanupResource);
    /// `mini_chat_cleanup_vector_store_with_failed_attachments`.
    fn chat_cleanup_store_with_failed_attachments(&self);
    /// `mini_chat_secondary_cleanup_skipped{provider_kind="anthropic"}`.
    fn chat_secondary_cleanup_skipped(&self);
    /// `mini_chat_thread_summary_execution{result}`.
    fn summary_execution(&self, result: SummaryResult);
    /// `mini_chat_thread_summary_cas_conflicts`.
    fn summary_cas_conflict(&self);
    /// `mini_chat_summary_fallback` (provider failures only).
    fn summary_fallback(&self);
}
