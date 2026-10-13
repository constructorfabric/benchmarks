//! Shared outbox integration: queue names, payloads, enqueuer and pipeline.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use toolkit_db::outbox::{
    LeaseConfig, LeasedHandler, Outbox, OutboxError, OutboxHandle, OutboxProfile, Partitions,
    Record, Wake, WorkerTuning,
};
use toolkit_db::secure::DBRunner;
use uuid::Uuid;

use crate::config::OutboxConfig;
use crate::domain::error::DomainError;

/// Payload type of every mini-chat outbox message.
pub const PAYLOAD_TYPE: &str = "application/json";

/// `mini-chat.attachment_cleanup` payload (DESIGN §4 "Attachment Deletion").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentCleanupEvent {
    /// `attachment_deleted` | `attachment_upload_abandoned` | `attachment_indexing_failed`
    pub event_type: String,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub attachment_id: Uuid,
    pub provider_file_id: Option<String>,
    pub vector_store_id: Option<String>,
    pub storage_backend: String,
    pub attachment_kind: String,
    pub deleted_at: String,
    pub secondary_ref: Option<SecondaryRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecondaryRef {
    pub file_id: String,
    pub provider_kind: String,
    pub upstream_alias: String,
}

/// `mini-chat.chat_cleanup` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatCleanupEvent {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub system_request_id: Uuid,
    /// Always `chat_soft_delete`.
    pub reason: String,
    pub chat_deleted_at: String,
}

/// `mini-chat.thread_summary` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadSummaryTask {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub system_request_id: Uuid,
    pub base_frontier_created_at: Option<String>,
    pub base_frontier_message_id: Option<Uuid>,
    pub frozen_target_created_at: String,
    pub frozen_target_message_id: Uuid,
    /// Always `thread_summary_update`.
    pub system_task_type: String,
}

/// Queue selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Queue {
    Usage,
    AttachmentCleanup,
    ChatCleanup,
    ThreadSummary,
    Audit,
}

/// Enqueues JSON payloads inside the caller's transaction.
#[derive(Clone)]
pub struct OutboxEnqueuer {
    outbox: Arc<Outbox>,
    cfg: OutboxConfig,
}

/// Partition of `key` among `n` partitions (stable per UUID).
#[must_use]
pub fn partition_of(key: Uuid, n: u32) -> u32 {
    let b = key.as_bytes();
    u32::from(u16::from_be_bytes([b[14], b[15]])) % n.max(1)
}

/// Error raised when a payload exceeds the outbox size limit.
#[derive(Debug)]
pub struct PayloadTooLarge(pub String);

impl OutboxEnqueuer {
    #[must_use]
    pub fn new(outbox: Arc<Outbox>, cfg: OutboxConfig) -> Self {
        Self { outbox, cfg }
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

    /// Enqueue `payload` on `queue`, partitioned by `key`.
    ///
    /// # Errors
    /// `Err(Ok(PayloadTooLarge))` when the payload is over the outbox limit,
    /// `Err(Err(DomainError))` for other failures.
    pub async fn enqueue_checked<T: Serialize + Sync>(
        &self,
        runner: &(impl DBRunner + Sync + ?Sized),
        queue: Queue,
        key: Uuid,
        payload: &T,
    ) -> Result<Wake, Result<PayloadTooLarge, DomainError>> {
        let bytes = serde_json::to_vec(payload)
            .map_err(|e| Err(DomainError::Internal(format!("outbox payload: {e}"))))?;
        let partition = partition_of(key, self.cfg.num_partitions);
        let record = Record::to(self.queue_name(queue), partition)
            .payload(bytes, PAYLOAD_TYPE)
            .build()
            .map_err(map_outbox_err)?;
        self.outbox.enqueue(runner, record).await.map_err(map_outbox_err)
    }

    /// Enqueue, mapping a payload-size error to an internal error.
    ///
    /// # Errors
    /// Any enqueue failure.
    pub async fn enqueue<T: Serialize + Sync>(
        &self,
        runner: &(impl DBRunner + Sync + ?Sized),
        queue: Queue,
        key: Uuid,
        payload: &T,
    ) -> Result<Wake, DomainError> {
        self.enqueue_checked(runner, queue, key, payload)
            .await
            .map_err(|e| match e {
                Ok(PayloadTooLarge(msg)) => DomainError::Internal(msg),
                Err(d) => d,
            })
    }
}

fn map_outbox_err(e: OutboxError) -> Result<PayloadTooLarge, DomainError> {
    match e {
        OutboxError::PayloadTooLarge { size, max } => Ok(PayloadTooLarge(format!(
            "outbox payload of {size} bytes exceeds the limit of {max} bytes"
        ))),
        OutboxError::Database(db) => Err(DomainError::Db(db)),
        other => Err(DomainError::Internal(format!("outbox: {other}"))),
    }
}

/// The five leased handlers of the mini-chat queues.
pub struct OutboxHandlers {
    pub usage: Box<dyn LeasedHandler>,
    pub attachment_cleanup: Box<dyn LeasedHandler>,
    pub chat_cleanup: Box<dyn LeasedHandler>,
    pub thread_summary: Box<dyn LeasedHandler>,
    pub audit: Box<dyn LeasedHandler>,
}

struct BoxedHandler(Box<dyn LeasedHandler>);

#[async_trait::async_trait]
impl LeasedHandler for BoxedHandler {
    async fn handle(
        &self,
        batch: &mut toolkit_db::outbox::Batch<'_>,
    ) -> toolkit_db::outbox::HandlerResult {
        self.0.handle(batch).await
    }
}

/// Start the outbox pipeline with the five mini-chat queues.
///
/// # Errors
/// Pipeline start failure.
pub async fn start_pipeline(
    db: toolkit_db::Db,
    cfg: &OutboxConfig,
    thread_summary_lease_secs: u64,
    handlers: OutboxHandlers,
) -> Result<OutboxHandle, OutboxError> {
    let parts = u16::try_from(cfg.num_partitions).unwrap_or(4);
    let partitions = Partitions::of(parts);
    let summary_lease = LeaseConfig {
        duration: Duration::from_secs(thread_summary_lease_secs),
        headroom: Duration::from_secs(2),
    };
    let audit_lease = LeaseConfig {
        duration: Duration::from_secs(60),
        headroom: Duration::from_secs(2),
    };
    Outbox::builder(db)
        .profile(OutboxProfile::low_latency())
        .processor_tuning(WorkerTuning::processor_low_latency().batch_size(1))
        .queue(&cfg.queue_name, partitions)
        .leased(BoxedHandler(handlers.usage))
        .queue(&cfg.cleanup_queue_name, partitions)
        .leased(BoxedHandler(handlers.attachment_cleanup))
        .queue(&cfg.chat_cleanup_queue_name, partitions)
        .leased(BoxedHandler(handlers.chat_cleanup))
        .queue(&cfg.thread_summary_queue_name, partitions)
        .leased(BoxedHandler(handlers.thread_summary))
        .lease(summary_lease)
        .queue(&cfg.audit_queue_name, partitions)
        .leased(BoxedHandler(handlers.audit))
        .lease(audit_lease)
        .start()
        .await
}
