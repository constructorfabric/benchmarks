//! Outbox integration: payloads, enqueuer and handlers.

pub mod handlers;

use std::sync::{Arc, OnceLock};

use mini_chat_sdk::{MiniChatAuditEvent, UsageEvent};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use toolkit_db::DbTx;
use toolkit_db::outbox::{Outbox, Record, Wake};
use uuid::Uuid;

use crate::config::OutboxConfig;
use crate::domain::error::DomainError;

pub const PAYLOAD_USAGE: &str = "mini-chat.usage.v1";
pub const PAYLOAD_AUDIT: &str = "mini-chat.audit.v1";
pub const PAYLOAD_ATTACHMENT_CLEANUP: &str = "mini-chat.attachment_cleanup.v1";
pub const PAYLOAD_CHAT_CLEANUP: &str = "mini-chat.chat_cleanup.v1";
pub const PAYLOAD_THREAD_SUMMARY: &str = "mini-chat.thread_summary.v1";

/// Secondary (Anthropic) copy reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecondaryRef {
    pub file_id: String,
    pub provider_kind: String,
    pub upstream_alias: String,
}

/// Attachment cleanup message (`attachment_deleted`, `attachment_upload_abandoned`,
/// `attachment_indexing_failed`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentCleanupPayload {
    pub event_type: String,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub attachment_id: Uuid,
    pub provider_file_id: Option<String>,
    pub vector_store_id: Option<String>,
    pub storage_backend: String,
    pub attachment_kind: String,
    #[serde(with = "time::serde::rfc3339")]
    pub deleted_at: OffsetDateTime,
    pub secondary_ref: Option<SecondaryRef>,
}

/// Chat cleanup message written by chat soft-delete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatCleanupPayload {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub system_request_id: Uuid,
    pub reason: String,
    #[serde(with = "time::serde::rfc3339")]
    pub chat_deleted_at: OffsetDateTime,
}

/// Thread summary work item with a frozen target frontier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadSummaryPayload {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub system_request_id: Uuid,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub base_frontier_created_at: Option<OffsetDateTime>,
    pub base_frontier_message_id: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    pub frozen_target_created_at: OffsetDateTime,
    pub frozen_target_message_id: Uuid,
    pub system_task_type: String,
}

/// Stable partition of a key in `0..n`.
#[must_use]
pub fn partition(key: Uuid, partitions: u32) -> u32 {
    let b = key.as_bytes();
    u32::from(u16::from_be_bytes([b[14], b[15]])) % partitions.max(1)
}

/// Enqueues mini-chat messages inside the caller's transaction. The pipeline
/// is bound at gear start.
pub struct OutboxEnqueuer {
    outbox: OnceLock<Arc<Outbox>>,
    cfg: OutboxConfig,
}

impl OutboxEnqueuer {
    #[must_use]
    pub fn new(cfg: OutboxConfig) -> Self {
        Self {
            outbox: OnceLock::new(),
            cfg,
        }
    }

    pub fn bind(&self, outbox: Arc<Outbox>) {
        let _ = self.outbox.set(outbox);
    }

    #[must_use]
    pub fn config(&self) -> &OutboxConfig {
        &self.cfg
    }

    async fn enqueue<T: Serialize + Sync>(
        &self,
        tx: &DbTx<'_>,
        queue: &str,
        key: Uuid,
        payload_type: &str,
        value: &T,
    ) -> Result<Wake, DomainError> {
        let outbox = self
            .outbox
            .get()
            .ok_or_else(|| DomainError::internal("outbox pipeline is not running"))?;
        let bytes = serde_json::to_vec(value).map_err(DomainError::internal)?;
        let record = Record::to(queue, partition(key, self.cfg.num_partitions))
            .payload(bytes, payload_type)
            .build()?;
        Ok(outbox.enqueue(tx, record).await?)
    }

    /// # Errors
    /// Outbox or serialization errors.
    pub async fn usage(&self, tx: &DbTx<'_>, ev: &UsageEvent) -> Result<Wake, DomainError> {
        self.enqueue(tx, &self.cfg.queue_name, ev.tenant_id, PAYLOAD_USAGE, ev).await
    }

    /// # Errors
    /// Outbox or serialization errors.
    pub async fn audit(&self, tx: &DbTx<'_>, ev: &MiniChatAuditEvent) -> Result<Wake, DomainError> {
        self.enqueue(tx, &self.cfg.audit_queue_name, ev.tenant_id(), PAYLOAD_AUDIT, ev).await
    }

    /// # Errors
    /// Outbox or serialization errors.
    pub async fn attachment_cleanup(&self, tx: &DbTx<'_>, p: &AttachmentCleanupPayload) -> Result<Wake, DomainError> {
        self.enqueue(tx, &self.cfg.cleanup_queue_name, p.tenant_id, PAYLOAD_ATTACHMENT_CLEANUP, p).await
    }

    /// # Errors
    /// Outbox or serialization errors.
    pub async fn chat_cleanup(&self, tx: &DbTx<'_>, p: &ChatCleanupPayload) -> Result<Wake, DomainError> {
        self.enqueue(tx, &self.cfg.chat_cleanup_queue_name, p.chat_id, PAYLOAD_CHAT_CLEANUP, p).await
    }

    /// # Errors
    /// Outbox or serialization errors.
    pub async fn thread_summary(&self, tx: &DbTx<'_>, p: &ThreadSummaryPayload) -> Result<Wake, DomainError> {
        self.enqueue(tx, &self.cfg.thread_summary_queue_name, p.chat_id, PAYLOAD_THREAD_SUMMARY, p).await
    }
}
