//! The gear's transactional outbox: five leased queues on the platform outbox
//! (`toolkit_outbox` table prefix, DESIGN section 5.7).
//!
//! Enqueue inside the business transaction; fire the returned [`Wake`] after
//! the transaction commits.

pub mod attachment_cleanup;
pub mod audit_handler;
pub mod chat_cleanup;
pub mod payloads;
pub mod thread_summary;
pub mod usage_handler;

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use mini_chat_sdk::{MiniChatAuditEvent, UsageEvent};
use serde::Serialize;
use tokio::sync::watch;
use toolkit_db::Db;
use toolkit_db::outbox::{
    LeaseConfig, LeasedMessageHandler, MessageResult, Outbox, OutboxHandle, OutboxMessage,
    OutboxProfile, Partitions, Record, Wake, WorkerTuning,
};
use toolkit_db::secure::DBRunner;
use uuid::Uuid;

pub use self::attachment_cleanup::{AttachmentCleanupHandler, CleanupDeps};
pub use self::audit_handler::AuditHandler;
pub use self::chat_cleanup::ChatCleanupHandler;
use self::payloads::{AttachmentCleanupPayload, ChatCleanupPayload, ThreadSummaryPayload};
pub use self::thread_summary::ThreadSummaryHandler;
pub use self::usage_handler::UsageHandler;
use crate::config::OutboxConfig;
use crate::domain::error::DomainError;

/// Table prefix of the platform outbox migrations (`outbox_migrations()`).
pub const TABLE_PREFIX: &str = "toolkit_outbox";

pub const USAGE_PAYLOAD_TYPE: &str = "mini-chat.usage.v1";
pub const AUDIT_PAYLOAD_TYPE: &str = "mini-chat.audit.v1";
pub const ATTACHMENT_CLEANUP_PAYLOAD_TYPE: &str = "mini-chat.attachment_cleanup.v1";
pub const CHAT_CLEANUP_PAYLOAD_TYPE: &str = "mini-chat.chat_cleanup.v1";
pub const THREAD_SUMMARY_PAYLOAD_TYPE: &str = "mini-chat.thread_summary.v1";

/// Lease of the usage and cleanup queues.
const DEFAULT_LEASE: Duration = Duration::from_secs(30);
/// Lease of the audit queue (covers the 30 s plugin call timeout).
const AUDIT_LEASE: Duration = Duration::from_secs(60);
const LEASE_HEADROOM: Duration = Duration::from_secs(2);

/// Partition of `key` (tenant or chat id): the last two bytes, big-endian,
/// modulo the partition count.
#[must_use]
pub fn partition_for(key: Uuid, num_partitions: u32) -> u32 {
    let b = key.as_bytes();
    u32::from(u16::from_be_bytes([b[14], b[15]])) % num_partitions.max(1)
}

/// One leased handler per queue.
pub struct OutboxHandlers {
    pub usage: Arc<dyn LeasedMessageHandler>,
    pub audit: Arc<dyn LeasedMessageHandler>,
    pub attachment_cleanup: Arc<dyn LeasedMessageHandler>,
    pub chat_cleanup: Arc<dyn LeasedMessageHandler>,
    pub thread_summary: Arc<dyn LeasedMessageHandler>,
}

/// How long [`DeferredHandler`] waits for its handler before `Retry`.
const DEFERRED_INSTALL_WAIT: Duration = Duration::from_secs(30);

/// A queue handler installed after the outbox starts (the thread-summary
/// service enqueues through the outbox it is a handler of). Deliveries before
/// [`install`](Self::install) wait up to 30 s, then `Retry`.
pub struct DeferredHandler {
    tx: watch::Sender<Option<Arc<dyn LeasedMessageHandler>>>,
}

impl Default for DeferredHandler {
    fn default() -> Self {
        Self {
            tx: watch::Sender::new(None),
        }
    }
}

impl DeferredHandler {
    pub fn install(&self, handler: Arc<dyn LeasedMessageHandler>) {
        self.tx.send_replace(Some(handler));
    }
}

#[async_trait]
impl LeasedMessageHandler for DeferredHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let mut rx = self.tx.subscribe();
        let handler = tokio::time::timeout(DEFERRED_INSTALL_WAIT, async {
            rx.wait_for(Option::is_some)
                .await
                .ok()
                .and_then(|h| h.clone())
        })
        .await
        .ok()
        .flatten();
        if let Some(h) = handler {
            h.handle(msg).await
        } else {
            tracing::warn!(seq = msg.seq, "outbox handler not installed yet; retrying");
            MessageResult::Retry
        }
    }
}

/// Adapts a shared handler to the builder's `impl LeasedHandler` parameter.
struct Shared(Arc<dyn LeasedMessageHandler>);

#[async_trait]
impl LeasedMessageHandler for Shared {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        self.0.handle(msg).await
    }
}

fn lease(duration: Duration) -> LeaseConfig {
    LeaseConfig {
        duration,
        headroom: LEASE_HEADROOM,
    }
}

pub struct MiniChatOutbox {
    outbox: Arc<Outbox>,
    handle: Mutex<Option<OutboxHandle>>,
    cfg: OutboxConfig,
}

impl MiniChatOutbox {
    /// Registers the five queues and starts the pipeline.
    ///
    /// # Errors
    /// `Internal` when the partition count is invalid or the outbox fails to start.
    pub async fn start(
        db: Db,
        cfg: &OutboxConfig,
        thread_summary_lease: Duration,
        handlers: OutboxHandlers,
    ) -> Result<Self, DomainError> {
        let partitions = u16::try_from(cfg.num_partitions)
            .ok()
            .filter(|n| (1..=64).contains(n) && n.is_power_of_two())
            .map(Partitions::of)
            .ok_or_else(|| {
                DomainError::Internal(format!(
                    "outbox.num_partitions must be a power of two in 1..=64, got {}",
                    cfg.num_partitions
                ))
            })?;
        let handle = Outbox::builder(db)
            .table_prefix(TABLE_PREFIX)?
            .profile(OutboxProfile::low_latency())
            .processors(2)
            .maintenance(1, 1)
            .processor_tuning(WorkerTuning::processor_low_latency().batch_size(1))
            .queue(&cfg.queue_name, partitions)
            .leased(Shared(handlers.usage))
            .lease(lease(DEFAULT_LEASE))
            .queue(&cfg.cleanup_queue_name, partitions)
            .leased(Shared(handlers.attachment_cleanup))
            .lease(lease(DEFAULT_LEASE))
            .queue(&cfg.chat_cleanup_queue_name, partitions)
            .leased(Shared(handlers.chat_cleanup))
            .lease(lease(DEFAULT_LEASE))
            .queue(&cfg.thread_summary_queue_name, partitions)
            .leased(Shared(handlers.thread_summary))
            .lease(lease(thread_summary_lease))
            .queue(&cfg.audit_queue_name, partitions)
            .leased(Shared(handlers.audit))
            .lease(lease(AUDIT_LEASE))
            .start()
            .await?;
        Ok(Self {
            outbox: Arc::clone(handle.outbox()),
            handle: Mutex::new(Some(handle)),
            cfg: cfg.clone(),
        })
    }

    /// Stops the pipeline (idempotent). Already enqueued messages stay in the
    /// tables and are delivered after the next start.
    pub async fn stop(&self) {
        let handle = self
            .handle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(handle) = handle {
            handle.stop().await;
        }
    }

    async fn enqueue<T: Serialize + ?Sized>(
        &self,
        tx: &(impl DBRunner + Sync),
        queue: &str,
        key: Uuid,
        payload_type: &str,
        payload: &T,
    ) -> Result<Wake, DomainError> {
        let bytes = serde_json::to_vec(payload)
            .map_err(|e| DomainError::Internal(format!("outbox payload serialization: {e}")))?;
        let record = Record::to(queue, partition_for(key, self.cfg.num_partitions))
            .payload(bytes, payload_type)
            .build()?;
        Ok(self.outbox.enqueue(tx, record).await?)
    }

    /// Usage settlement, partitioned by tenant.
    ///
    /// # Errors
    /// Serialization, size or database failure.
    pub async fn enqueue_usage(
        &self,
        tx: &(impl DBRunner + Sync),
        ev: &UsageEvent,
    ) -> Result<Wake, DomainError> {
        self.enqueue(
            tx,
            &self.cfg.queue_name,
            ev.tenant_id,
            USAGE_PAYLOAD_TYPE,
            ev,
        )
        .await
    }

    /// Audit event, partitioned by tenant.
    ///
    /// # Errors
    /// Serialization, size or database failure.
    pub async fn enqueue_audit(
        &self,
        tx: &(impl DBRunner + Sync),
        tenant_id: Uuid,
        ev: &MiniChatAuditEvent,
    ) -> Result<Wake, DomainError> {
        self.enqueue(
            tx,
            &self.cfg.audit_queue_name,
            tenant_id,
            AUDIT_PAYLOAD_TYPE,
            ev,
        )
        .await
    }

    /// Attachment cleanup, partitioned by tenant.
    ///
    /// # Errors
    /// Serialization, size or database failure.
    pub async fn enqueue_attachment_cleanup(
        &self,
        tx: &(impl DBRunner + Sync),
        p: &AttachmentCleanupPayload,
    ) -> Result<Wake, DomainError> {
        self.enqueue(
            tx,
            &self.cfg.cleanup_queue_name,
            p.tenant_id,
            ATTACHMENT_CLEANUP_PAYLOAD_TYPE,
            p,
        )
        .await
    }

    /// Chat cleanup, partitioned by chat.
    ///
    /// # Errors
    /// Serialization, size or database failure.
    pub async fn enqueue_chat_cleanup(
        &self,
        tx: &(impl DBRunner + Sync),
        p: &ChatCleanupPayload,
    ) -> Result<Wake, DomainError> {
        self.enqueue(
            tx,
            &self.cfg.chat_cleanup_queue_name,
            p.chat_id,
            CHAT_CLEANUP_PAYLOAD_TYPE,
            p,
        )
        .await
    }

    /// Thread summary task, partitioned by chat.
    ///
    /// # Errors
    /// Serialization, size or database failure.
    pub async fn enqueue_thread_summary(
        &self,
        tx: &(impl DBRunner + Sync),
        p: &ThreadSummaryPayload,
    ) -> Result<Wake, DomainError> {
        self.enqueue(
            tx,
            &self.cfg.thread_summary_queue_name,
            p.chat_id,
            THREAD_SUMMARY_PAYLOAD_TYPE,
            p,
        )
        .await
    }
}

#[cfg(test)]
#[path = "outbox_tests.rs"]
mod outbox_tests;

#[cfg(test)]
#[path = "handlers_tests.rs"]
mod handlers_tests;
