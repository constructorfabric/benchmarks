//! Serialized payloads of the cleanup and thread-summary outbox queues.
//! (Usage and audit payloads are the SDK `UsageEvent` / `AuditEvent`.)

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Secondary (Anthropic) file reference of an attachment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecondaryRef {
    pub file_id: String,
    pub provider_kind: String,
    pub upstream_alias: String,
}

/// `mini-chat.attachment_cleanup` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentCleanupPayload {
    /// `attachment_deleted` | `attachment_upload_abandoned` | `attachment_indexing_failed`.
    pub event_type: String,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub attachment_id: Uuid,
    pub provider_file_id: Option<String>,
    pub vector_store_id: Option<String>,
    pub storage_backend: String,
    pub attachment_kind: String,
    pub deleted_at: DateTime<Utc>,
    pub secondary_ref: Option<SecondaryRef>,
}

/// `mini-chat.chat_cleanup` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatCleanupPayload {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub system_request_id: Uuid,
    pub reason: String,
    pub chat_deleted_at: DateTime<Utc>,
}

/// `mini-chat.thread_summary` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadSummaryPayload {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub system_request_id: Uuid,
    pub base_frontier_created_at: Option<DateTime<Utc>>,
    pub base_frontier_message_id: Option<Uuid>,
    pub frozen_target_created_at: DateTime<Utc>,
    pub frozen_target_message_id: Uuid,
    pub system_task_type: String,
}
