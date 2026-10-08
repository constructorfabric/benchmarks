//! Payload types carried by the outbox queues (JSON, `serde`). Usage and audit payloads are the
//! SDK's `UsageEvent` / `AuditEvent`.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// Payload type of the usage queue (`UsageEvent`).
pub const USAGE_PAYLOAD_TYPE: &str = "mini-chat.usage.v1";
/// Payload type of the audit queue (`AuditEvent`).
pub const AUDIT_PAYLOAD_TYPE: &str = "mini-chat.audit.v1";
/// Payload type of the attachment cleanup queue ([`AttachmentCleanupEvent`]).
pub const ATTACHMENT_CLEANUP_PAYLOAD_TYPE: &str = "mini-chat.attachment_cleanup.v1";
/// Payload type of the chat cleanup queue ([`ChatCleanupEvent`]).
pub const CHAT_CLEANUP_PAYLOAD_TYPE: &str = "mini-chat.chat_cleanup.v1";
/// Payload type of the thread summary queue ([`ThreadSummaryTask`]).
pub const THREAD_SUMMARY_PAYLOAD_TYPE: &str = "mini-chat.thread_summary.v1";

/// Secondary copy of an attachment (Anthropic images) to delete with the primary file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecondaryRef {
    pub file_id: String,
    pub provider_kind: String,
    pub upstream_alias: String,
}

/// Provider file cleanup of one attachment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentCleanupEvent {
    /// `attachment_deleted`, `attachment_upload_abandoned` or `attachment_indexing_failed`.
    pub event_type: String,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub attachment_id: Uuid,
    /// `None` when the upload never reached the provider.
    pub provider_file_id: Option<String>,
    /// Always `None` (the handler does not read it).
    pub vector_store_id: Option<String>,
    /// Upstream used to delete the primary file.
    pub storage_backend: String,
    /// `document` or `image`.
    pub attachment_kind: String,
    #[serde(with = "time::serde::rfc3339")]
    pub deleted_at: OffsetDateTime,
    pub secondary_ref: Option<SecondaryRef>,
}

/// Provider-side cleanup of a soft-deleted chat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatCleanupEvent {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    /// Stable across retries of the same enqueue.
    pub system_request_id: Uuid,
    /// `chat_soft_delete`.
    pub reason: String,
    #[serde(with = "time::serde::rfc3339")]
    pub chat_deleted_at: OffsetDateTime,
}

/// Thread summary update of one chat, frozen at enqueue time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadSummaryTask {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    /// Stable across retries of the same enqueue.
    pub system_request_id: Uuid,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub base_frontier_created_at: Option<OffsetDateTime>,
    pub base_frontier_message_id: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    pub frozen_target_created_at: OffsetDateTime,
    pub frozen_target_message_id: Uuid,
    /// `thread_summary_update`.
    pub system_task_type: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payloads_round_trip_with_rfc3339_timestamps() {
        let ev = ThreadSummaryTask {
            tenant_id: Uuid::from_u128(1),
            chat_id: Uuid::from_u128(2),
            system_request_id: Uuid::from_u128(3),
            base_frontier_created_at: None,
            base_frontier_message_id: None,
            frozen_target_created_at: OffsetDateTime::UNIX_EPOCH,
            frozen_target_message_id: Uuid::from_u128(4),
            system_task_type: "thread_summary_update".to_owned(),
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["frozen_target_created_at"], "1970-01-01T00:00:00Z");
        assert!(v["base_frontier_created_at"].is_null());
        assert_eq!(serde_json::from_value::<ThreadSummaryTask>(v).unwrap(), ev);
    }
}
