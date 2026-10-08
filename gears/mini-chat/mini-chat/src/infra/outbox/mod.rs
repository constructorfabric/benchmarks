//! Transactional outbox integration: enqueuer (producers) and handlers.

pub mod handlers;
pub mod payloads;

use std::sync::{Arc, OnceLock};

use mini_chat_sdk::{AuditEvent, UsageEvent};
use serde::Serialize;
use toolkit_db::outbox::{Outbox, Record, Wake};
use toolkit_db::secure::DBRunner;
use uuid::Uuid;

use crate::config::OutboxConfig;
use crate::domain::error::DomainError;
use payloads::{AttachmentCleanupPayload, ChatCleanupPayload, ThreadSummaryPayload};

const PAYLOAD_TYPE: &str = "application/json";

/// Producer side of the five mini-chat queues.
pub struct OutboxEnqueuer {
    outbox: OnceLock<Arc<Outbox>>,
    cfg: OutboxConfig,
}

/// Partition of a UUID key.
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

    /// Binds the started outbox.
    pub fn bind(&self, outbox: Arc<Outbox>) {
        self.outbox.set(outbox).ok();
    }

    #[must_use]
    pub fn config(&self) -> &OutboxConfig {
        &self.cfg
    }

    fn outbox(&self) -> Result<&Arc<Outbox>, DomainError> {
        self.outbox
            .get()
            .ok_or_else(|| DomainError::internal("outbox pipeline is not started"))
    }

    async fn enqueue<T: Serialize + Sync>(
        &self,
        runner: &(impl DBRunner + Sync),
        queue: &str,
        key: Uuid,
        payload: &T,
    ) -> Result<Wake, DomainError> {
        let bytes = serde_json::to_vec(payload)?;
        let rec = Record::to(queue, partition_of(key, self.cfg.num_partitions))
            .payload(bytes, PAYLOAD_TYPE)
            .build()?;
        Ok(self.outbox()?.enqueue(runner, rec).await?)
    }

    /// Usage event (partitioned by tenant).
    ///
    /// # Errors
    /// Outbox failure.
    pub async fn usage(&self, runner: &(impl DBRunner + Sync), ev: &UsageEvent) -> Result<Wake, DomainError> {
        self.enqueue(runner, &self.cfg.queue_name, ev.tenant_id, ev).await
    }

    /// Audit event (partitioned by tenant).
    ///
    /// # Errors
    /// Outbox failure.
    pub async fn audit(
        &self,
        runner: &(impl DBRunner + Sync),
        tenant_id: Uuid,
        ev: &AuditEvent,
    ) -> Result<Wake, DomainError> {
        self.enqueue(runner, &self.cfg.audit_queue_name, tenant_id, ev).await
    }

    /// Attachment cleanup (partitioned by tenant).
    ///
    /// # Errors
    /// Outbox failure.
    pub async fn attachment_cleanup(
        &self,
        runner: &(impl DBRunner + Sync),
        p: &AttachmentCleanupPayload,
    ) -> Result<Wake, DomainError> {
        self.enqueue(runner, &self.cfg.cleanup_queue_name, p.tenant_id, p).await
    }

    /// Chat cleanup (partitioned by chat).
    ///
    /// # Errors
    /// Outbox failure.
    pub async fn chat_cleanup(&self, runner: &(impl DBRunner + Sync), p: &ChatCleanupPayload) -> Result<Wake, DomainError> {
        self.enqueue(runner, &self.cfg.chat_cleanup_queue_name, p.chat_id, p).await
    }

    /// Thread summary task (partitioned by chat).
    ///
    /// # Errors
    /// Outbox failure.
    pub async fn thread_summary(
        &self,
        runner: &(impl DBRunner + Sync),
        p: &ThreadSummaryPayload,
    ) -> Result<Wake, DomainError> {
        self.enqueue(runner, &self.cfg.thread_summary_queue_name, p.chat_id, p).await
    }
}
