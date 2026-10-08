//! Shared toolkit-db outbox wiring: five leased queues and typed enqueue helpers.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use mini_chat_sdk::{AuditEvent, UsageEvent};
use serde::Serialize;
use toolkit_db::outbox::{
    LeaseConfig, LeasedMessageHandler, Outbox, OutboxHandle, Partitions, Record, Wake,
};
use toolkit_db::{Db, DbTx};
use uuid::Uuid;

use crate::config::OutboxConfig;
use crate::domain::error::DomainError;
use crate::domain::outbox_payloads::{AttachmentCleanupEvent, ChatCleanupEvent, ThreadSummaryTask};

/// Payload type tags.
pub mod payload_types {
    pub const USAGE: &str = "mini_chat.usage_event.v1";
    pub const AUDIT: &str = "mini_chat.audit_event.v1";
    pub const ATTACHMENT_CLEANUP: &str = "mini_chat.attachment_cleanup.v1";
    pub const CHAT_CLEANUP: &str = "mini_chat.chat_cleanup.v1";
    pub const THREAD_SUMMARY: &str = "mini_chat.thread_summary.v1";
}

/// Typed enqueue facade. The underlying `Outbox` is bound once the pipeline started.
pub struct MiniChatOutbox {
    cfg: OutboxConfig,
    outbox: OnceLock<Arc<Outbox>>,
}

/// Stable partition of a key.
#[must_use]
pub fn partition_of(key: Uuid, partitions: u32) -> u32 {
    let b = key.as_bytes();
    let v = u32::from_le_bytes([b[12], b[13], b[14], b[15]]);
    v % partitions.max(1)
}

impl MiniChatOutbox {
    #[must_use]
    pub fn new(cfg: OutboxConfig) -> Self {
        Self {
            cfg,
            outbox: OnceLock::new(),
        }
    }

    #[must_use]
    pub const fn config(&self) -> &OutboxConfig {
        &self.cfg
    }

    /// Binds the started outbox.
    pub fn bind(&self, outbox: Arc<Outbox>) {
        let _ = self.outbox.set(outbox);
    }

    fn outbox(&self) -> Result<&Arc<Outbox>, DomainError> {
        self.outbox
            .get()
            .ok_or_else(|| DomainError::internal("outbox pipeline is not started"))
    }

    async fn enqueue_json<T: Serialize + Sync>(
        &self,
        tx: &DbTx<'_>,
        queue: &str,
        partition_key: Uuid,
        payload_type: &str,
        payload: &T,
    ) -> Result<Wake, DomainError> {
        let bytes = serde_json::to_vec(payload).map_err(DomainError::internal)?;
        if bytes.len() > 64 * 1024 {
            return Err(DomainError::InvalidFormat {
                resource: crate::domain::error::resource_types::CHAT,
                message: "outbox payload exceeds the outbox size limit".to_owned(),
            });
        }
        let record = Record::to(queue, partition_of(partition_key, self.cfg.num_partitions))
            .payload(bytes, payload_type)
            .build()
            .map_err(DomainError::internal)?;
        self.outbox()?
            .enqueue(tx, record)
            .await
            .map_err(DomainError::internal)
    }

    /// Usage event (partitioned by tenant).
    ///
    /// # Errors
    /// Outbox / serialization failure.
    pub async fn enqueue_usage(&self, tx: &DbTx<'_>, ev: &UsageEvent) -> Result<Wake, DomainError> {
        self.enqueue_json(tx, &self.cfg.queue_name, ev.tenant_id, payload_types::USAGE, ev)
            .await
    }

    /// Audit event (partitioned by tenant).
    ///
    /// # Errors
    /// Outbox / serialization failure.
    pub async fn enqueue_audit(&self, tx: &DbTx<'_>, ev: &AuditEvent) -> Result<Wake, DomainError> {
        self.enqueue_json(tx, &self.cfg.audit_queue_name, ev.tenant_id(), payload_types::AUDIT, ev)
            .await
    }

    /// Attachment cleanup (partitioned by tenant).
    ///
    /// # Errors
    /// Outbox / serialization failure.
    pub async fn enqueue_attachment_cleanup(
        &self,
        tx: &DbTx<'_>,
        ev: &AttachmentCleanupEvent,
    ) -> Result<Wake, DomainError> {
        self.enqueue_json(
            tx,
            &self.cfg.cleanup_queue_name,
            ev.tenant_id,
            payload_types::ATTACHMENT_CLEANUP,
            ev,
        )
        .await
    }

    /// Chat cleanup (partitioned by chat).
    ///
    /// # Errors
    /// Outbox / serialization failure.
    pub async fn enqueue_chat_cleanup(
        &self,
        tx: &DbTx<'_>,
        ev: &ChatCleanupEvent,
    ) -> Result<Wake, DomainError> {
        self.enqueue_json(
            tx,
            &self.cfg.chat_cleanup_queue_name,
            ev.chat_id,
            payload_types::CHAT_CLEANUP,
            ev,
        )
        .await
    }

    /// Thread summary task (partitioned by chat).
    ///
    /// # Errors
    /// Outbox / serialization failure.
    pub async fn enqueue_thread_summary(
        &self,
        tx: &DbTx<'_>,
        ev: &ThreadSummaryTask,
    ) -> Result<Wake, DomainError> {
        self.enqueue_json(
            tx,
            &self.cfg.thread_summary_queue_name,
            ev.chat_id,
            payload_types::THREAD_SUMMARY,
            ev,
        )
        .await
    }
}

/// Handlers of the five queues.
pub struct OutboxHandlers<U, A, C, H, T> {
    pub usage: U,
    pub audit: A,
    pub attachment_cleanup: C,
    pub chat_cleanup: H,
    pub thread_summary: T,
}

/// Starts the outbox pipeline with the five mini-chat queues.
///
/// # Errors
/// Outbox start failure.
pub async fn start<U, A, C, H, T>(
    db: Db,
    cfg: &OutboxConfig,
    thread_summary_lease_secs: u64,
    handlers: OutboxHandlers<U, A, C, H, T>,
) -> anyhow::Result<OutboxHandle>
where
    U: LeasedMessageHandler + 'static,
    A: LeasedMessageHandler + 'static,
    C: LeasedMessageHandler + 'static,
    H: LeasedMessageHandler + 'static,
    T: LeasedMessageHandler + 'static,
{
    let n = u16::try_from(cfg.num_partitions).unwrap_or(4);
    let partitions = Partitions::of(n);
    let handle = Outbox::builder(db)
        .queue(&cfg.queue_name, partitions)
        .leased(handlers.usage)
        .queue(&cfg.cleanup_queue_name, partitions)
        .leased(handlers.attachment_cleanup)
        .queue(&cfg.chat_cleanup_queue_name, partitions)
        .leased(handlers.chat_cleanup)
        .queue(&cfg.thread_summary_queue_name, partitions)
        .leased(handlers.thread_summary)
        .lease(LeaseConfig {
            duration: Duration::from_secs(thread_summary_lease_secs),
            headroom: Duration::from_secs(5),
        })
        .queue(&cfg.audit_queue_name, partitions)
        .leased(handlers.audit)
        .lease(LeaseConfig {
            duration: Duration::from_secs(60),
            headroom: Duration::from_secs(5),
        })
        .start()
        .await?;
    Ok(handle)
}
