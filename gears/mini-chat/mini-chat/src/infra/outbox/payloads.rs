//! Outbox payloads of the attachment-cleanup, chat-cleanup and thread-summary queues.
//! Usage payloads are `mini_chat_sdk::UsageEvent`; audit payloads are `MiniChatAuditEvent`.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// Secondary (Anthropic) file reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecondaryRef {
    pub file_id: String,
    pub provider_kind: String,
    pub upstream_alias: String,
}

/// `mini-chat.attachment_cleanup` message (DESIGN §4 "Attachment Deletion").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentCleanupPayload {
    /// `attachment_deleted` | `attachment_upload_abandoned` | `attachment_indexing_failed`
    pub event_type: String,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub attachment_id: Uuid,
    pub provider_file_id: Option<String>,
    /// Always `null` in P1.
    pub vector_store_id: Option<String>,
    pub storage_backend: String,
    pub attachment_kind: String,
    #[serde(with = "time::serde::rfc3339")]
    pub deleted_at: OffsetDateTime,
    pub secondary_ref: Option<SecondaryRef>,
}

/// `mini-chat.chat_cleanup` message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatCleanupPayload {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub system_request_id: Uuid,
    /// Always `chat_soft_delete`.
    pub reason: String,
    #[serde(with = "time::serde::rfc3339")]
    pub chat_deleted_at: OffsetDateTime,
}

/// `mini-chat.thread_summary` message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadSummaryPayload {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub system_request_id: Uuid,
    /// Always `thread_summary_update`.
    pub system_task_type: String,
    #[serde(with = "time::serde::rfc3339::option")]
    pub base_frontier_created_at: Option<OffsetDateTime>,
    pub base_frontier_message_id: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    pub frozen_target_created_at: OffsetDateTime,
    pub frozen_target_message_id: Uuid,
}
