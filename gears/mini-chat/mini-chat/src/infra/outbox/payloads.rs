//! JSON payloads of the mini-chat outbox queues that are not SDK types.
//!
//! - attachment cleanup: D "Attachment Deletion" Phase 1 table;
//! - chat cleanup: D "Outbox payload and execution semantics";
//! - thread summary: D "Execution stages and invariants" (2. Durable scheduling).
//!
//! Usage and audit queues carry `mini_chat_sdk::UsageEvent` /
//! `mini_chat_sdk::MiniChatAuditEvent` as is. Timestamps are RFC 3339.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// `reason` of a chat cleanup message.
pub const CHAT_CLEANUP_REASON: &str = "chat_soft_delete";

/// `system_task_type` of a thread summary message.
pub const THREAD_SUMMARY_TASK_TYPE: &str = "thread_summary_update";

/// Which path enqueued an attachment cleanup message (wire names are fixed).
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentCleanupEventType {
    /// `DELETE /v1/chats/{id}/attachments/{attachment_id}`.
    AttachmentDeleted,
    /// Upload reaper failed an abandoned upload that has a provider file.
    AttachmentUploadAbandoned,
    /// Background indexing failed or timed out.
    AttachmentIndexingFailed,
}

/// Secondary (Anthropic) copy of an attachment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecondaryRef {
    pub file_id: String,
    pub provider_kind: String,
    pub upstream_alias: String,
}

/// Attachment cleanup message (queue `outbox.cleanup_queue_name`, partition by tenant).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentCleanupPayload {
    pub event_type: AttachmentCleanupEventType,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub attachment_id: Uuid,
    /// Primary provider file id; `null` if the upload never reached the provider.
    pub provider_file_id: Option<String>,
    /// Always `null` in P1; the handler does not read it.
    pub vector_store_id: Option<String>,
    /// Upstream used to delete the primary file.
    pub storage_backend: String,
    /// `document` | `image`.
    pub attachment_kind: String,
    /// Enqueue time.
    #[serde(with = "time::serde::rfc3339")]
    pub deleted_at: OffsetDateTime,
    /// Set when a secondary upload succeeded (never for the reaper).
    pub secondary_ref: Option<SecondaryRef>,
}

/// Chat cleanup message (queue `outbox.chat_cleanup_queue_name`, partition by chat).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatCleanupPayload {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    /// Server-generated v4, persisted at enqueue time, stable across retries.
    pub system_request_id: Uuid,
    /// Always [`CHAT_CLEANUP_REASON`].
    pub reason: String,
    #[serde(with = "time::serde::rfc3339")]
    pub chat_deleted_at: OffsetDateTime,
}

impl ChatCleanupPayload {
    /// Payload for a chat soft-deleted at `chat_deleted_at` (fresh v4
    /// `system_request_id`).
    #[must_use]
    pub fn new(tenant_id: Uuid, chat_id: Uuid, chat_deleted_at: OffsetDateTime) -> Self {
        Self {
            tenant_id,
            chat_id,
            system_request_id: Uuid::new_v4(),
            reason: CHAT_CLEANUP_REASON.to_owned(),
            chat_deleted_at,
        }
    }
}

/// Thread summary message (queue `outbox.thread_summary_queue_name`, partition by chat).
///
/// Frontiers are `(created_at, message_id)` pairs in the per-chat order
/// `(created_at ASC, id ASC)`; the base frontier is `null` when the chat has
/// no summary yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadSummaryPayload {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    /// Server-generated v4, persisted at enqueue time, stable across retries.
    pub system_request_id: Uuid,
    #[serde(with = "time::serde::rfc3339::option")]
    pub base_frontier_created_at: Option<OffsetDateTime>,
    pub base_frontier_message_id: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    pub frozen_target_created_at: OffsetDateTime,
    pub frozen_target_message_id: Uuid,
    /// Always [`THREAD_SUMMARY_TASK_TYPE`].
    pub system_task_type: String,
}

impl ThreadSummaryPayload {
    /// Payload for summarizing `(base, target]` (fresh v4 `system_request_id`).
    #[must_use]
    pub fn new(
        tenant_id: Uuid,
        chat_id: Uuid,
        base: Option<(OffsetDateTime, Uuid)>,
        target: (OffsetDateTime, Uuid),
    ) -> Self {
        Self {
            tenant_id,
            chat_id,
            system_request_id: Uuid::new_v4(),
            base_frontier_created_at: base.map(|b| b.0),
            base_frontier_message_id: base.map(|b| b.1),
            frozen_target_created_at: target.0,
            frozen_target_message_id: target.1,
            system_task_type: THREAD_SUMMARY_TASK_TYPE.to_owned(),
        }
    }

    /// Base frontier; `None` unless both components are present.
    #[must_use]
    pub fn base_frontier(&self) -> Option<(OffsetDateTime, Uuid)> {
        Some((
            self.base_frontier_created_at?,
            self.base_frontier_message_id?,
        ))
    }

    /// Frozen target frontier.
    #[must_use]
    pub fn frozen_target(&self) -> (OffsetDateTime, Uuid) {
        (self.frozen_target_created_at, self.frozen_target_message_id)
    }
}

#[cfg(test)]
#[path = "payloads_tests.rs"]
mod payloads_tests;
