//! Outbox payloads of the five mini-chat queues.
//!
//! Usage (`UsageEvent`) and audit (`TurnAuditEvent` / `TurnMutationAuditEvent`)
//! payloads are SDK types; the cleanup and thread-summary payloads are
//! gear-internal.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// Payload type of usage events.
pub const PAYLOAD_USAGE: &str = "mini_chat.usage_event.v1";
/// Payload type of turn audit events.
pub const PAYLOAD_TURN_AUDIT: &str = "mini_chat.turn_audit.v1";
/// Payload type of turn mutation audit events.
pub const PAYLOAD_MUTATION_AUDIT: &str = "mini_chat.turn_mutation_audit.v1";
/// Payload type of attachment cleanup events.
pub const PAYLOAD_ATTACHMENT_CLEANUP: &str = "mini_chat.attachment_cleanup.v1";
/// Payload type of chat cleanup events.
pub const PAYLOAD_CHAT_CLEANUP: &str = "mini_chat.chat_cleanup.v1";
/// Payload type of thread summary tasks.
pub const PAYLOAD_THREAD_SUMMARY: &str = "mini_chat.thread_summary.v1";

/// System task type of the thread summary.
pub const THREAD_SUMMARY_TASK: &str = "thread_summary_update";

/// Secondary (Anthropic) copy of an attachment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecondaryRef {
    pub file_id: String,
    pub provider_kind: String,
    pub upstream_alias: String,
}

/// `mini-chat.attachment_cleanup` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentCleanupEvent {
    /// `attachment_deleted`, `attachment_upload_abandoned` or
    /// `attachment_indexing_failed`.
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

/// `mini-chat.chat_cleanup` payload.
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

/// `mini-chat.thread_summary` payload (frozen summary range).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadSummaryTask {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub system_request_id: Uuid,
    pub system_task_type: String,
    #[serde(with = "time::serde::rfc3339::option")]
    pub base_frontier_created_at: Option<OffsetDateTime>,
    pub base_frontier_message_id: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    pub frozen_target_created_at: OffsetDateTime,
    pub frozen_target_message_id: Uuid,
}
