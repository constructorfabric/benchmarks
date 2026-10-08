//! Shared transactional outbox integration: five leased queues (usage,
//! attachment cleanup, chat cleanup, thread summary, audit).

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde::Serialize;
use toolkit_db::Db;
use toolkit_db::outbox::{
    LeaseConfig, LeasedMessageHandler, Outbox, OutboxError, OutboxHandle, OutboxProfile, Partitions, Record,
    Wake, WorkerTuning,
};
use toolkit_db::secure::DBRunner;
use uuid::Uuid;

use crate::config::OutboxConfig;
use crate::domain::error::DomainError;

/// Payload type of each queue.
pub mod payload_types {
    pub const USAGE: &str = "mini_chat.usage_event.v1";
    pub const AUDIT: &str = "mini_chat.audit_event.v1";
    pub const ATTACHMENT_CLEANUP: &str = "mini_chat.attachment_cleanup.v1";
    pub const CHAT_CLEANUP: &str = "mini_chat.chat_cleanup.v1";
    pub const THREAD_SUMMARY: &str = "mini_chat.thread_summary.v1";
}

/// Logical queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboxKind {
    Usage,
    Audit,
    AttachmentCleanup,
    ChatCleanup,
    ThreadSummary,
}

impl OutboxKind {
    const fn payload_type(self) -> &'static str {
        match self {
            Self::Usage => payload_types::USAGE,
            Self::Audit => payload_types::AUDIT,
            Self::AttachmentCleanup => payload_types::ATTACHMENT_CLEANUP,
            Self::ChatCleanup => payload_types::CHAT_CLEANUP,
            Self::ThreadSummary => payload_types::THREAD_SUMMARY,
        }
    }
}

/// Started outbox (set once the pipeline runs) plus queue names.
pub struct OutboxSlot {
    outbox: OnceLock<Arc<Outbox>>,
    cfg: OutboxConfig,
}

impl OutboxSlot {
    #[must_use]
    pub fn new(cfg: OutboxConfig) -> Self {
        Self { outbox: OnceLock::new(), cfg }
    }

    /// Install the started outbox.
    pub fn install(&self, outbox: Arc<Outbox>) {
        if self.outbox.set(outbox).is_err() {
            tracing::debug!("outbox already installed");
        }
    }

    fn get(&self) -> Result<&Arc<Outbox>, DomainError> {
        self.outbox
            .get()
            .ok_or_else(|| DomainError::Internal("outbox pipeline is not started".to_owned()))
    }

    #[must_use]
    pub fn queue_name(&self, kind: OutboxKind) -> &str {
        match kind {
            OutboxKind::Usage => &self.cfg.queue_name,
            OutboxKind::Audit => &self.cfg.audit_queue_name,
            OutboxKind::AttachmentCleanup => &self.cfg.cleanup_queue_name,
            OutboxKind::ChatCleanup => &self.cfg.chat_cleanup_queue_name,
            OutboxKind::ThreadSummary => &self.cfg.thread_summary_queue_name,
        }
    }

    /// Partition of a key (`chat_id` or `tenant_id`).
    #[must_use]
    pub fn partition(&self, key: Uuid) -> u32 {
        let b = key.as_bytes();
        let h = u32::from_le_bytes([b[12], b[13], b[14], b[15]]) ^ u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        h % self.cfg.num_partitions.max(1)
    }
}

/// Enqueue a JSON payload inside the caller's transaction. Fire the returned
/// [`Wake`] after commit.
///
/// # Errors
/// `Internal` (payload too large / pipeline not started / DB errors).
pub async fn enqueue_json<T: Serialize + Sync>(
    slot: &OutboxSlot,
    tx: &(impl DBRunner + Sync),
    kind: OutboxKind,
    partition_key: Uuid,
    payload: &T,
) -> Result<Wake, DomainError> {
    let bytes = serde_json::to_vec(payload).map_err(|e| DomainError::Internal(format!("serialize outbox payload: {e}")))?;
    let queue = slot.queue_name(kind).to_owned();
    let partition = slot.partition(partition_key);
    let record = Record::to(&queue, partition)
        .payload(bytes, kind.payload_type())
        .build()
        .map_err(map_outbox_err)?;
    let outbox = slot.get()?;
    outbox.enqueue(tx, record).await.map_err(map_outbox_err)
}

fn map_outbox_err(e: OutboxError) -> DomainError {
    match e {
        OutboxError::PayloadTooLarge { size, max } => {
            DomainError::Internal(format!("outbox payload too large ({size} > {max} bytes)"))
        }
        other => DomainError::from(other),
    }
}

/// Handlers of the five queues.
pub struct OutboxHandlers {
    pub usage: Box<dyn LeasedMessageHandler>,
    pub audit: Box<dyn LeasedMessageHandler>,
    pub attachment_cleanup: Box<dyn LeasedMessageHandler>,
    pub chat_cleanup: Box<dyn LeasedMessageHandler>,
    pub thread_summary: Box<dyn LeasedMessageHandler>,
}

struct Boxed(Box<dyn LeasedMessageHandler>);

#[async_trait::async_trait]
impl LeasedMessageHandler for Boxed {
    async fn handle(&self, msg: &toolkit_db::outbox::OutboxMessage) -> toolkit_db::outbox::MessageResult {
        self.0.handle(msg).await
    }
}

/// Start the outbox pipeline with the five queues.
///
/// # Errors
/// Outbox registration / start failures.
pub async fn start_pipeline(
    db: Db,
    cfg: &OutboxConfig,
    summary_lease_secs: u64,
    handlers: OutboxHandlers,
) -> Result<OutboxHandle, OutboxError> {
    let parts = Partitions::of(u16::try_from(cfg.num_partitions).unwrap_or(4));
    let tuning = WorkerTuning::processor_low_latency()
        .batch_size(1)
        .retry_base(Duration::from_millis(500))
        .retry_max(Duration::from_secs(30));
    let summary_lease = Duration::from_secs(summary_lease_secs.max(30));
    Outbox::builder(db)
        .profile(OutboxProfile::low_latency())
        .processors(2)
        .maintenance(1, 1)
        .processor_tuning(tuning)
        .queue(&cfg.queue_name, parts)
        .leased(Boxed(handlers.usage))
        .queue(&cfg.cleanup_queue_name, parts)
        .leased(Boxed(handlers.attachment_cleanup))
        .queue(&cfg.chat_cleanup_queue_name, parts)
        .leased(Boxed(handlers.chat_cleanup))
        .queue(&cfg.thread_summary_queue_name, parts)
        .leased(Boxed(handlers.thread_summary))
        .lease(LeaseConfig { duration: summary_lease, headroom: Duration::from_secs(2) })
        .queue(&cfg.audit_queue_name, parts)
        .leased(Boxed(handlers.audit))
        .lease(LeaseConfig { duration: Duration::from_secs(60), headroom: Duration::from_secs(2) })
        .start()
        .await
}
