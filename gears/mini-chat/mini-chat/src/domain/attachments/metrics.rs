//! The attachment metric port of the upload, reaper, cleanup and knowledge-search sides
//! (`cpt-cf-mini-chat-algo-attachments-and-file-search-metrics`): low-cardinality labels only,
//! never a tenant, user, chat, attachment, file or vector store identifier.
//!
//! `infra::metrics::attachments` implements it for the OTLP instruments.

use std::time::Duration;

/// The upload-side series of the attachment metrics algorithm.
pub trait AttachmentMetrics: Send + Sync {
    /// `mini_chat_attachment_upload{kind, result}`: once per request that took a permit; `kind`
    /// is `document` or `image`, `result` is `ok` (every 201) or `error` (every Problem).
    fn upload(&self, kind: &str, result: &str);

    /// `mini_chat_attachment_upload_bytes{kind}`: the byte count of a 201 upload.
    fn upload_bytes(&self, kind: &str, bytes: u64);

    /// `mini_chat_attachments_pending`: `+1` at the insert, `-1` where the row leaves `pending`.
    fn pending_add(&self, delta: i64);

    /// `mini_chat_attachment_background_indexing{result}`: `ready`, `failed`, `timeout` or
    /// `set_ready_failed`.
    fn background_indexing(&self, result: &str);

    /// `mini_chat_attachment_upload_abandoned{from_status}`: after each reaper commit; `from_status`
    /// is the status the row had (`pending` or `uploaded`).
    fn upload_abandoned(&self, from_status: &str);

    /// `mini_chat_upload_reaper_scan_duration_seconds`: once per reaper scan.
    fn reaper_scan_duration(&self, elapsed: Duration);

    /// `mini_chat_cleanup_completed{resource_type="file"}`.
    fn cleanup_completed(&self);

    /// `mini_chat_cleanup_retry{resource_type="file", reason}`; `reason` is `provider_error`.
    fn cleanup_retry(&self, reason: &str);

    /// `mini_chat_cleanup_failed{resource_type="file"}`.
    fn cleanup_failed(&self);

    /// `mini_chat_secondary_cleanup_skipped{provider_kind}`: no Anthropic Files client exists.
    fn secondary_cleanup_skipped(&self, provider_kind: &str);

    /// `mini_chat_knowledge_search{result}`: `ok` or `error`.
    fn knowledge_search(&self, result: &str);

    /// `mini_chat_knowledge_search_latency_ms`: one retrieval.
    fn knowledge_search_latency(&self, elapsed: Duration);

    /// `mini_chat_knowledge_search_chunks`: the chunks one successful retrieval returned.
    fn knowledge_search_chunks(&self, chunks: usize);
}
