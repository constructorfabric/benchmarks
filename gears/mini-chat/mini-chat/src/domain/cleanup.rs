//! Chat-deletion cleanup message (DESIGN section 3.6 "Cleanup on Chat Deletion").

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use toolkit_macros::domain_model;
use uuid::Uuid;

/// `reason` of a chat-cleanup message produced by a chat soft delete.
pub const CHAT_SOFT_DELETE_REASON: &str = "chat_soft_delete";

/// Payload of the chat-cleanup outbox message, serialized in the soft-delete transaction.
///
/// `system_request_id` is a server-generated UUID v4 persisted at enqueue time and reused
/// unchanged across every retry of the message.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatCleanupPayload {
    /// The tenant owning the chat.
    pub tenant_id: Uuid,
    /// The soft-deleted chat.
    pub chat_id: Uuid,
    /// Stable identity of this cleanup task.
    pub system_request_id: Uuid,
    /// Why the cleanup was scheduled.
    pub reason: String,
    /// When the chat was soft-deleted.
    #[serde(with = "mini_chat_sdk::time_serde")]
    pub chat_deleted_at: OffsetDateTime,
}

impl ChatCleanupPayload {
    /// The payload of a chat soft delete.
    #[must_use]
    pub fn for_soft_delete(
        tenant_id: Uuid,
        chat_id: Uuid,
        system_request_id: Uuid,
        chat_deleted_at: OffsetDateTime,
    ) -> Self {
        Self {
            tenant_id,
            chat_id,
            system_request_id,
            reason: CHAT_SOFT_DELETE_REASON.to_owned(),
            chat_deleted_at,
        }
    }
}

/// Why an attachment cleanup message was produced (`event_type` of the payload).
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttachmentCleanupEvent {
    /// The user deleted the attachment.
    #[serde(rename = "attachment_deleted")]
    Deleted,
    /// The upload reaper failed an abandoned upload.
    #[serde(rename = "attachment_upload_abandoned")]
    UploadAbandoned,
    /// The background indexing task failed or timed out.
    #[serde(rename = "attachment_indexing_failed")]
    IndexingFailed,
}

/// The secondary (Anthropic) copy of an image, deleted best effort after the primary file.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecondaryRef {
    /// The file id in the secondary provider's Files API.
    pub file_id: String,
    /// The secondary provider kind (`anthropic`).
    pub provider_kind: String,
    /// The OAGW upstream alias the secondary file is deleted through.
    pub upstream_alias: String,
}

/// Payload of the attachment-cleanup outbox message (DESIGN "Attachment Deletion", phase 1).
///
/// `vector_store_id` is always `None` in P1: the handler makes no Vector Stores API call.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentCleanupPayload {
    /// Why the message was produced.
    pub event_type: AttachmentCleanupEvent,
    /// The tenant owning the attachment.
    pub tenant_id: Uuid,
    /// The parent chat.
    pub chat_id: Uuid,
    /// The attachment.
    pub attachment_id: Uuid,
    /// The primary provider file; `None` when the upload never reached the provider.
    pub provider_file_id: Option<String>,
    /// Always `None` in P1.
    #[serde(default)]
    pub vector_store_id: Option<String>,
    /// The storage backend label the primary file was uploaded through.
    pub storage_backend: String,
    /// `document` or `image`.
    pub attachment_kind: String,
    /// When the message was enqueued.
    #[serde(with = "mini_chat_sdk::time_serde")]
    pub deleted_at: OffsetDateTime,
    /// The secondary copy, when one was uploaded.
    #[serde(default)]
    pub secondary_ref: Option<SecondaryRef>,
}

#[cfg(test)]
#[path = "cleanup_tests.rs"]
mod tests;
