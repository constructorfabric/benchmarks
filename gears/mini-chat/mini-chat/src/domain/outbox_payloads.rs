//! Serialized payloads of the mini-chat outbox queues (shared by producers and handlers).
//! Usage and audit payloads are the SDK `UsageEvent` / `AuditEvent`.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// `event_type` values of the attachment cleanup queue.
pub mod attachment_event_types {
    pub const DELETED: &str = "attachment_deleted";
    pub const UPLOAD_ABANDONED: &str = "attachment_upload_abandoned";
    pub const INDEXING_FAILED: &str = "attachment_indexing_failed";
}

/// Reference to a secondary (Anthropic) copy of an image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecondaryRef {
    pub file_id: String,
    pub provider_kind: String,
    pub upstream_alias: String,
}

/// Attachment cleanup message (`outbox.cleanup_queue_name`, partitioned by tenant).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentCleanupEvent {
    pub event_type: String,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub attachment_id: Uuid,
    pub provider_file_id: Option<String>,
    /// Always `None` in P1.
    pub vector_store_id: Option<String>,
    pub storage_backend: String,
    pub attachment_kind: String,
    #[serde(with = "time::serde::rfc3339")]
    pub deleted_at: OffsetDateTime,
    pub secondary_ref: Option<SecondaryRef>,
}

/// Chat cleanup message (`outbox.chat_cleanup_queue_name`, partitioned by chat).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatCleanupEvent {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub system_request_id: Uuid,
    /// Always `chat_soft_delete`.
    pub reason: String,
    #[serde(with = "time::serde::rfc3339")]
    pub chat_deleted_at: OffsetDateTime,
}

/// Thread summary work item (`outbox.thread_summary_queue_name`, partitioned by chat).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadSummaryTask {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub system_request_id: Uuid,
    #[serde(with = "time::serde::rfc3339::option")]
    pub base_frontier_created_at: Option<OffsetDateTime>,
    pub base_frontier_message_id: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    pub frozen_target_created_at: OffsetDateTime,
    pub frozen_target_message_id: Uuid,
    /// Always `thread_summary_update`.
    pub system_task_type: String,
}

/// Simple-form (32 hex chars) UUID used in dedupe keys.
#[must_use]
pub fn simple_uuid(u: Uuid) -> String {
    u.as_simple().to_string()
}

/// `{tenant_hex}/{turn_hex}/{request_hex}` dedupe key of a turn usage event.
#[must_use]
pub fn turn_dedupe_key(tenant_id: Uuid, turn_id: Uuid, request_id: Uuid) -> String {
    format!(
        "{}/{}/{}",
        simple_uuid(tenant_id),
        simple_uuid(turn_id),
        simple_uuid(request_id)
    )
}
