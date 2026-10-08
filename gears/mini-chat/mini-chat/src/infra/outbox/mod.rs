//! Outbox enqueuer, payloads and handlers (DESIGN §5.6).

pub mod attachment_cleanup;
pub mod audit;
pub mod chat_cleanup;
pub mod thread_summary;
pub mod usage;

use std::sync::{Arc, OnceLock, Weak};

use mini_chat_sdk::{MiniChatAuditEvent, UsageEvent};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use toolkit_db::outbox::{Outbox, Record, Wake};
use toolkit_db::secure::DbTx;
use uuid::Uuid;

use crate::config::OutboxConfig;
use crate::domain::error::DomainError;

/// Payload type of usage events.
pub const USAGE_PAYLOAD_TYPE: &str = "mini_chat.usage_event.v1";
/// Payload type of audit events.
pub const AUDIT_PAYLOAD_TYPE: &str = "mini_chat.audit_event.v1";
/// Payload type of attachment cleanup events.
pub const ATTACHMENT_CLEANUP_PAYLOAD_TYPE: &str = "mini_chat.attachment_cleanup.v1";
/// Payload type of chat cleanup events.
pub const CHAT_CLEANUP_PAYLOAD_TYPE: &str = "mini_chat.chat_cleanup.v1";
/// Payload type of thread summary tasks.
pub const THREAD_SUMMARY_PAYLOAD_TYPE: &str = "mini_chat.thread_summary.v1";

/// Secondary (Anthropic) file reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecondaryRef {
    /// File id.
    pub file_id: String,
    /// `anthropic`.
    pub provider_kind: String,
    /// OAGW alias.
    pub upstream_alias: String,
}

/// Attachment cleanup event (`mini-chat.attachment_cleanup`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentCleanupEvent {
    /// `attachment_deleted`, `attachment_upload_abandoned` or `attachment_indexing_failed`.
    pub event_type: String,
    /// Tenant.
    pub tenant_id: Uuid,
    /// Chat.
    pub chat_id: Uuid,
    /// Attachment.
    pub attachment_id: Uuid,
    /// Primary provider file id.
    pub provider_file_id: Option<String>,
    /// Always `null` in P1.
    pub vector_store_id: Option<String>,
    /// Storage backend label.
    pub storage_backend: String,
    /// `document` or `image`.
    pub attachment_kind: String,
    /// Enqueue time.
    #[serde(with = "time::serde::rfc3339")]
    pub deleted_at: OffsetDateTime,
    /// Secondary copy.
    pub secondary_ref: Option<SecondaryRef>,
}

/// Chat cleanup event (`mini-chat.chat_cleanup`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatCleanupEvent {
    /// Tenant.
    pub tenant_id: Uuid,
    /// Chat.
    pub chat_id: Uuid,
    /// Stable system request id.
    pub system_request_id: Uuid,
    /// `chat_soft_delete`.
    pub reason: String,
    /// Soft delete time.
    #[serde(with = "time::serde::rfc3339")]
    pub chat_deleted_at: OffsetDateTime,
}

/// Thread summary task (`mini-chat.thread_summary`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadSummaryTask {
    /// Tenant.
    pub tenant_id: Uuid,
    /// Chat.
    pub chat_id: Uuid,
    /// Stable system request id.
    pub system_request_id: Uuid,
    /// Base frontier `created_at` (`None` when no summary exists).
    #[serde(with = "time::serde::rfc3339::option")]
    pub base_frontier_created_at: Option<OffsetDateTime>,
    /// Base frontier message id.
    pub base_frontier_message_id: Option<Uuid>,
    /// Frozen target `created_at`.
    #[serde(with = "time::serde::rfc3339")]
    pub frozen_target_created_at: OffsetDateTime,
    /// Frozen target message id.
    pub frozen_target_message_id: Uuid,
    /// `thread_summary_update`.
    pub system_task_type: String,
}

fn partition_of(id: Uuid, partitions: u32) -> u32 {
    let b = id.as_bytes();
    let v = u32::from_be_bytes([b[12], b[13], b[14], b[15]]);
    v % partitions.max(1)
}

/// Late-bound outbox enqueuer (the pipeline starts in `start()`).
pub struct Enqueuer {
    cfg: OutboxConfig,
    outbox: OnceLock<Weak<Outbox>>,
}

impl Enqueuer {
    /// New enqueuer.
    #[must_use]
    pub fn new(cfg: OutboxConfig) -> Self {
        Self { cfg, outbox: OnceLock::new() }
    }

    /// Queue configuration.
    #[must_use]
    pub fn config(&self) -> &OutboxConfig {
        &self.cfg
    }

    /// Binds the running outbox.
    pub fn bind(&self, outbox: &Arc<Outbox>) {
        if self.outbox.set(Arc::downgrade(outbox)).is_err() {
            tracing::warn!("mini-chat outbox already bound; keeping the first binding");
        }
    }

    async fn enqueue(
        &self,
        tx: &DbTx<'_>,
        queue: &str,
        partition_key: Uuid,
        payload: Vec<u8>,
        payload_type: &str,
    ) -> Result<Wake, DomainError> {
        let outbox = self
            .outbox
            .get()
            .and_then(Weak::upgrade)
            .ok_or_else(|| DomainError::internal("outbox is not running"))?;
        let record = Record::to(queue, partition_of(partition_key, self.cfg.num_partitions))
            .payload(payload, payload_type)
            .build()
            .map_err(|e| match e {
                toolkit_db::outbox::OutboxError::PayloadTooLarge { size, max } => {
                    DomainError::CleanupPayloadTooLarge(format!(
                        "outbox payload of {size} bytes exceeds the limit of {max} bytes"
                    ))
                }
                other => DomainError::internal(format!("outbox record: {other}")),
            })?;
        outbox
            .enqueue(tx, record)
            .await
            .map_err(|e| DomainError::internal(format!("outbox enqueue: {e}")))
    }

    /// Enqueues a usage event.
    ///
    /// # Errors
    /// Internal on outbox failure.
    pub async fn usage(&self, tx: &DbTx<'_>, event: &UsageEvent) -> Result<Wake, DomainError> {
        let payload = serde_json::to_vec(event).map_err(|e| DomainError::internal(e.to_string()))?;
        self.enqueue(tx, &self.cfg.queue_name, event.tenant_id, payload, USAGE_PAYLOAD_TYPE).await
    }

    /// Enqueues an audit event.
    ///
    /// # Errors
    /// Internal on outbox failure.
    pub async fn audit(&self, tx: &DbTx<'_>, tenant_id: Uuid, event: &MiniChatAuditEvent) -> Result<Wake, DomainError> {
        let payload = serde_json::to_vec(event).map_err(|e| DomainError::internal(e.to_string()))?;
        self.enqueue(tx, &self.cfg.audit_queue_name, tenant_id, payload, AUDIT_PAYLOAD_TYPE).await
    }

    /// Enqueues an attachment cleanup event.
    ///
    /// # Errors
    /// Internal / payload too large.
    pub async fn attachment_cleanup(&self, tx: &DbTx<'_>, event: &AttachmentCleanupEvent) -> Result<Wake, DomainError> {
        let payload = serde_json::to_vec(event).map_err(|e| DomainError::internal(e.to_string()))?;
        self.enqueue(tx, &self.cfg.cleanup_queue_name, event.tenant_id, payload, ATTACHMENT_CLEANUP_PAYLOAD_TYPE)
            .await
    }

    /// Enqueues a chat cleanup event.
    ///
    /// # Errors
    /// Internal / payload too large.
    pub async fn chat_cleanup(&self, tx: &DbTx<'_>, event: &ChatCleanupEvent) -> Result<Wake, DomainError> {
        let payload = serde_json::to_vec(event).map_err(|e| DomainError::internal(e.to_string()))?;
        self.enqueue(tx, &self.cfg.chat_cleanup_queue_name, event.chat_id, payload, CHAT_CLEANUP_PAYLOAD_TYPE)
            .await
    }

    /// Enqueues a thread summary task.
    ///
    /// # Errors
    /// Internal on outbox failure.
    pub async fn thread_summary(&self, tx: &DbTx<'_>, task: &ThreadSummaryTask) -> Result<Wake, DomainError> {
        let payload = serde_json::to_vec(task).map_err(|e| DomainError::internal(e.to_string()))?;
        self.enqueue(tx, &self.cfg.thread_summary_queue_name, task.chat_id, payload, THREAD_SUMMARY_PAYLOAD_TYPE)
            .await
    }
}

/// Builds the turn usage dedupe key `{tenant}/{turn}/{request}` (simple UUID form).
#[must_use]
pub fn turn_dedupe_key(tenant_id: Uuid, turn_id: Uuid, request_id: Uuid) -> String {
    format!("{}/{}/{}", tenant_id.simple(), turn_id.simple(), request_id.simple())
}

/// Builds the system task dedupe key `{tenant}/{task_type}/{system_request_id}`.
#[must_use]
pub fn system_dedupe_key(tenant_id: Uuid, task_type: &str, system_request_id: Uuid) -> String {
    format!("{}/{task_type}/{}", tenant_id.simple(), system_request_id.simple())
}
