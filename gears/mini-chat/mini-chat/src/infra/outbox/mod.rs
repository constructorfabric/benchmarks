//! Outbox integration: the enqueuer port used inside domain transactions and
//! the five queue handlers (DESIGN §5.6).

pub mod handlers;

use std::sync::{Arc, OnceLock};

use serde::Serialize;
use toolkit_db::DbTx;
use toolkit_db::outbox::{Outbox, Record, Wake};
use uuid::Uuid;

use crate::config::OutboxConfig;
use crate::domain::error::DomainError;

pub const PT_USAGE: &str = "mini-chat.usage_event.v1";
pub const PT_AUDIT: &str = "mini-chat.audit_event.v1";
pub const PT_ATTACHMENT_CLEANUP: &str = "mini-chat.attachment_cleanup.v1";
pub const PT_CHAT_CLEANUP: &str = "mini-chat.chat_cleanup.v1";
pub const PT_THREAD_SUMMARY: &str = "mini-chat.thread_summary.v1";

/// Collected post-commit wakes.
#[derive(Default)]
pub struct Wakes(Vec<Wake>);

impl Wakes {
    pub fn push(&mut self, w: Wake) {
        self.0.push(w);
    }

    pub fn fire(self) {
        for w in self.0 {
            w.fire();
        }
    }
}

/// Enqueues serialized events into the mini-chat queues.
pub struct OutboxEnqueuer {
    outbox: OnceLock<Arc<Outbox>>,
    pub cfg: OutboxConfig,
}

/// Stable partition of a key.
#[must_use]
pub fn partition_of(key: Uuid, partitions: u32) -> u32 {
    let b = key.as_bytes();
    let v = u32::from(u16::from_be_bytes([b[14], b[15]]));
    v % partitions.max(1)
}

impl OutboxEnqueuer {
    #[must_use]
    pub fn new(cfg: OutboxConfig) -> Self {
        Self {
            outbox: OnceLock::new(),
            cfg,
        }
    }

    /// Binds the running outbox.
    pub fn bind(&self, outbox: Arc<Outbox>) {
        let _ = self.outbox.set(outbox);
    }

    #[must_use]
    pub fn outbox(&self) -> Option<&Arc<Outbox>> {
        self.outbox.get()
    }

    async fn enqueue<T: Serialize + Sync>(
        &self,
        tx: &DbTx<'_>,
        queue: &str,
        key: Uuid,
        payload_type: &str,
        payload: &T,
    ) -> Result<Wake, DomainError> {
        let outbox = self
            .outbox
            .get()
            .ok_or_else(|| DomainError::internal("outbox is not running"))?;
        let bytes = serde_json::to_vec(payload)
            .map_err(|e| DomainError::internal(format!("serialize outbox payload: {e}")))?;
        let record = Record::to(queue, partition_of(key, self.cfg.num_partitions))
            .payload(bytes, payload_type)
            .build()?;
        Ok(outbox.enqueue(tx, record).await?)
    }

    /// Usage event, partitioned by tenant.
    ///
    /// # Errors
    /// Outbox failure.
    pub async fn usage<T: Serialize + Sync>(&self, tx: &DbTx<'_>, tenant: Uuid, payload: &T) -> Result<Wake, DomainError> {
        self.enqueue(tx, &self.cfg.queue_name, tenant, PT_USAGE, payload).await
    }

    /// Audit event, partitioned by tenant.
    ///
    /// # Errors
    /// Outbox failure.
    pub async fn audit<T: Serialize + Sync>(&self, tx: &DbTx<'_>, tenant: Uuid, payload: &T) -> Result<Wake, DomainError> {
        self.enqueue(tx, &self.cfg.audit_queue_name, tenant, PT_AUDIT, payload).await
    }

    /// Attachment cleanup event, partitioned by tenant.
    ///
    /// # Errors
    /// Outbox failure.
    pub async fn attachment_cleanup<T: Serialize + Sync>(
        &self,
        tx: &DbTx<'_>,
        tenant: Uuid,
        payload: &T,
    ) -> Result<Wake, DomainError> {
        self.enqueue(tx, &self.cfg.cleanup_queue_name, tenant, PT_ATTACHMENT_CLEANUP, payload)
            .await
    }

    /// Chat cleanup event, partitioned by chat.
    ///
    /// # Errors
    /// Outbox failure.
    pub async fn chat_cleanup(
        &self,
        tx: &DbTx<'_>,
        payload: &crate::domain::service::chats::ChatCleanupPayload,
    ) -> Result<Wake, DomainError> {
        self.enqueue(
            tx,
            &self.cfg.chat_cleanup_queue_name,
            payload.chat_id,
            PT_CHAT_CLEANUP,
            payload,
        )
        .await
    }

    /// Thread-summary task, partitioned by chat.
    ///
    /// # Errors
    /// Outbox failure.
    pub async fn thread_summary<T: Serialize + Sync>(
        &self,
        tx: &DbTx<'_>,
        chat_id: Uuid,
        payload: &T,
    ) -> Result<Wake, DomainError> {
        self.enqueue(tx, &self.cfg.thread_summary_queue_name, chat_id, PT_THREAD_SUMMARY, payload)
            .await
    }
}
