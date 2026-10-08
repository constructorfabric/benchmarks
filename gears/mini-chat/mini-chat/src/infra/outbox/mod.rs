//! Shared toolkit-db outbox integration: queues, payloads and enqueueing.

use std::sync::{Arc, OnceLock};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use toolkit_db::outbox::{Outbox, OutboxError, Record, Wake};
use toolkit_db::secure::DBRunner;
use uuid::Uuid;

use crate::config::OutboxConfig;
use crate::domain::error::DomainError;

/// Outbox payload size limit (toolkit-db `MAX_PAYLOAD_SIZE`).
pub const MAX_PAYLOAD_SIZE: usize = 64 * 1024;

/// Queue of a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Queue {
    Usage,
    AttachmentCleanup,
    ChatCleanup,
    ThreadSummary,
    Audit,
}

/// Attachment cleanup message (`attachment_deleted`,
/// `attachment_upload_abandoned`, `attachment_indexing_failed`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentCleanupMsg {
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

/// Secondary (Anthropic) copy of an image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecondaryRef {
    pub file_id: String,
    pub provider_kind: String,
    pub upstream_alias: String,
}

/// Chat cleanup message (chat soft delete).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatCleanupMsg {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub system_request_id: Uuid,
    pub reason: String,
    #[serde(with = "time::serde::rfc3339")]
    pub chat_deleted_at: OffsetDateTime,
}

/// Thread summary work item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadSummaryMsg {
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

/// Late-bound handle to the running outbox (started in the gear `start`
/// phase).
pub struct OutboxBridge {
    cfg: OutboxConfig,
    outbox: OnceLock<Arc<Outbox>>,
}

impl OutboxBridge {
    #[must_use]
    pub fn new(cfg: OutboxConfig) -> Self {
        Self {
            cfg,
            outbox: OnceLock::new(),
        }
    }

    pub fn bind(&self, outbox: Arc<Outbox>) {
        if self.outbox.set(outbox).is_err() {
            tracing::warn!("outbox bridge is already bound");
        }
    }

    #[must_use]
    pub fn queue_name(&self, q: Queue) -> &str {
        match q {
            Queue::Usage => &self.cfg.queue_name,
            Queue::AttachmentCleanup => &self.cfg.cleanup_queue_name,
            Queue::ChatCleanup => &self.cfg.chat_cleanup_queue_name,
            Queue::ThreadSummary => &self.cfg.thread_summary_queue_name,
            Queue::Audit => &self.cfg.audit_queue_name,
        }
    }

    /// Partition index of a key.
    #[must_use]
    pub fn partition(&self, key: Uuid) -> u32 {
        let b = key.as_bytes();
        let n = u32::from_be_bytes([b[12], b[13], b[14], b[15]]);
        n % self.cfg.num_partitions.max(1)
    }

    /// Serialize and enqueue a message inside the caller's transaction.
    pub async fn enqueue<T: Serialize>(
        &self,
        runner: &(impl DBRunner + Sync),
        queue: Queue,
        partition_key: Uuid,
        payload: &T,
    ) -> Result<Wake, DomainError> {
        let bytes = serde_json::to_vec(payload)
            .map_err(|e| DomainError::internal(format!("outbox payload serialization: {e}")))?;
        if bytes.len() > MAX_PAYLOAD_SIZE {
            return Err(DomainError::InvalidFormat(format!(
                "outbox payload of {} bytes exceeds the {MAX_PAYLOAD_SIZE}-byte limit",
                bytes.len()
            )));
        }
        let outbox = self
            .outbox
            .get()
            .ok_or_else(|| DomainError::internal("outbox is not running"))?;
        let name = self.queue_name(queue).to_owned();
        let record = Record::to(&name, self.partition(partition_key))
            .payload(bytes, &name)
            .build()
            .map_err(map_outbox_err)?;
        outbox.enqueue(runner, record).await.map_err(map_outbox_err)
    }
}

fn map_outbox_err(e: OutboxError) -> DomainError {
    match e {
        OutboxError::PayloadTooLarge { size, max } => DomainError::InvalidFormat(format!(
            "outbox payload of {size} bytes exceeds the {max}-byte limit"
        )),
        other => DomainError::internal(format!("outbox enqueue: {other}")),
    }
}

/// Fire a set of wakes after the commit.
pub fn fire(wakes: Vec<Wake>) {
    for w in wakes {
        w.fire();
    }
}
