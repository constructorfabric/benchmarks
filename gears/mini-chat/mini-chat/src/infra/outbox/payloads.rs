//! Serialized outbox payloads of the cleanup and thread-summary queues.
//!
//! Usage and audit payloads are the SDK's `UsageEvent` / `MiniChatAuditEvent`.
//! Timestamps serialize as RFC 3339 UTC (`...Z`).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Payload type of usage messages (`UsageEvent`).
pub const USAGE_PAYLOAD_TYPE: &str = "mini_chat.usage.v1";
/// Payload type of audit messages (`MiniChatAuditEvent`).
pub const AUDIT_PAYLOAD_TYPE: &str = "mini_chat.audit.v1";
/// Payload type of chat-cleanup messages.
pub const CHAT_CLEANUP_PAYLOAD_TYPE: &str = "mini_chat.chat_cleanup.v1";
/// Payload type of attachment-cleanup messages.
pub const ATTACHMENT_CLEANUP_PAYLOAD_TYPE: &str = "mini_chat.attachment_cleanup.v1";
/// Payload type of thread-summary messages.
pub const THREAD_SUMMARY_PAYLOAD_TYPE: &str = "mini_chat.thread_summary.v1";

/// `reason` of the chat-cleanup payload.
pub const CHAT_SOFT_DELETE_REASON: &str = "chat_soft_delete";
/// `system_task_type` of the thread-summary payload.
pub const THREAD_SUMMARY_TASK_TYPE: &str = "thread_summary_update";

/// Chat-cleanup message written by the chat soft-delete transaction
/// (DESIGN §3.6 "Cleanup on Chat Deletion").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatCleanupPayload {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    /// Stable identity of this system task, generated once at enqueue time.
    pub system_request_id: Uuid,
    /// Always [`CHAT_SOFT_DELETE_REASON`].
    pub reason: String,
    pub chat_deleted_at: DateTime<Utc>,
}

impl ChatCleanupPayload {
    /// Payload for a chat soft-deleted at `chat_deleted_at`, with a fresh `system_request_id`.
    #[must_use]
    pub fn soft_delete(tenant_id: Uuid, chat_id: Uuid, chat_deleted_at: DateTime<Utc>) -> Self {
        Self {
            tenant_id,
            chat_id,
            system_request_id: Uuid::new_v4(),
            reason: CHAT_SOFT_DELETE_REASON.to_owned(),
            chat_deleted_at,
        }
    }
}

/// Why an attachment-cleanup message was enqueued.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(clippy::enum_variant_names)] // wire names are `attachment_*`
pub enum AttachmentCleanupEvent {
    /// User deletion (`DELETE .../attachments/{id}`).
    AttachmentDeleted,
    /// Upload reaper failed an abandoned upload that has a provider file.
    AttachmentUploadAbandoned,
    /// Background indexing failed or timed out.
    AttachmentIndexingFailed,
}

/// Secondary provider copy of an attachment (Anthropic).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecondaryRef {
    pub file_id: String,
    pub provider_kind: String,
    pub upstream_alias: String,
}

/// Attachment-cleanup message (DESIGN §4 "Attachment Deletion", Phase 1 table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentCleanupPayload {
    pub event_type: AttachmentCleanupEvent,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub attachment_id: Uuid,
    /// Primary provider file id; `None` if the upload never reached the provider.
    pub provider_file_id: Option<String>,
    /// Always `None` in P1; the handler does not read it.
    pub vector_store_id: Option<String>,
    /// Upstream used to delete the primary file.
    pub storage_backend: String,
    /// `document` or `image`.
    pub attachment_kind: String,
    /// Enqueue time.
    pub deleted_at: DateTime<Utc>,
    pub secondary_ref: Option<SecondaryRef>,
}

/// Thread-summary message written by the finalization transaction (spec §14).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadSummaryPayload {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    /// Stable identity of this system task, generated once at enqueue time.
    pub system_request_id: Uuid,
    /// Current summary frontier; `None` when the chat has no summary yet.
    pub base_frontier_created_at: Option<DateTime<Utc>>,
    pub base_frontier_message_id: Option<Uuid>,
    pub frozen_target_created_at: DateTime<Utc>,
    pub frozen_target_message_id: Uuid,
    /// Always [`THREAD_SUMMARY_TASK_TYPE`].
    pub system_task_type: String,
}
