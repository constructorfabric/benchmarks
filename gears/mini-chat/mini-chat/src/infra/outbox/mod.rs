//! Outbox enqueuer (late-bound to the started pipeline) and queue payloads / handlers.

pub mod handlers;
pub mod payloads;

use std::sync::{Arc, OnceLock};

use serde::Serialize;
use toolkit_db::DbTx;
use toolkit_db::outbox::{Outbox, OutboxError, Record, Wake};
use uuid::Uuid;

use crate::config::OutboxConfig;
use crate::domain::error::DomainError;

pub const PAYLOAD_USAGE: &str = "mini_chat.usage_event.v1";
pub const PAYLOAD_AUDIT: &str = "mini_chat.audit_event.v1";
pub const PAYLOAD_ATTACHMENT_CLEANUP: &str = "mini_chat.attachment_cleanup.v1";
pub const PAYLOAD_CHAT_CLEANUP: &str = "mini_chat.chat_cleanup.v1";
pub const PAYLOAD_THREAD_SUMMARY: &str = "mini_chat.thread_summary.v1";

/// Enqueues JSON payloads to the mini-chat queues inside business transactions.
pub struct OutboxPort {
    cfg: OutboxConfig,
    outbox: OnceLock<Arc<Outbox>>,
}

impl OutboxPort {
    #[must_use]
    pub fn new(cfg: OutboxConfig) -> Self {
        Self { cfg, outbox: OnceLock::new() }
    }

    /// Binds the started pipeline (called once).
    pub fn bind(&self, outbox: Arc<Outbox>) {
        let _ = self.outbox.set(outbox);
    }

    #[must_use]
    pub fn config(&self) -> &OutboxConfig {
        &self.cfg
    }

    /// Partition number for a key (`0..num_partitions`).
    #[must_use]
    pub fn partition_for(&self, key: Uuid) -> u32 {
        let b = key.as_bytes();
        let v = u32::from(u16::from_be_bytes([b[14], b[15]]));
        v % self.cfg.num_partitions.max(1)
    }

    /// Serializes `payload` and enqueues it in `tx`. The returned `Wake` must be fired after commit.
    ///
    /// # Errors
    /// `InvalidFormat` when the payload exceeds the outbox size limit, `Internal` otherwise.
    pub async fn enqueue_json<T: Serialize + Sync>(
        &self,
        tx: &DbTx<'_>,
        queue: &str,
        partition_key: Uuid,
        payload_type: &str,
        payload: &T,
    ) -> Result<Wake, DomainError> {
        let outbox = self.outbox.get().ok_or_else(|| DomainError::internal("outbox pipeline not started"))?;
        let bytes = serde_json::to_vec(payload).map_err(|e| DomainError::internal(format!("serialize payload: {e}")))?;
        let record = Record::to(queue, self.partition_for(partition_key))
            .payload(bytes, payload_type)
            .build()
            .map_err(map_outbox_err)?;
        outbox.enqueue(tx, record).await.map_err(map_outbox_err)
    }

    #[must_use]
    pub fn usage_queue(&self) -> &str {
        &self.cfg.queue_name
    }
    #[must_use]
    pub fn audit_queue(&self) -> &str {
        &self.cfg.audit_queue_name
    }
    #[must_use]
    pub fn attachment_cleanup_queue(&self) -> &str {
        &self.cfg.cleanup_queue_name
    }
    #[must_use]
    pub fn chat_cleanup_queue(&self) -> &str {
        &self.cfg.chat_cleanup_queue_name
    }
    #[must_use]
    pub fn thread_summary_queue(&self) -> &str {
        &self.cfg.thread_summary_queue_name
    }
}

fn map_outbox_err(e: OutboxError) -> DomainError {
    match e {
        OutboxError::PayloadTooLarge { size, max } => {
            DomainError::InvalidFormat(format!("outbox payload too large: {size} bytes (max {max})"))
        }
        other => DomainError::internal(format!("outbox error: {other}")),
    }
}
