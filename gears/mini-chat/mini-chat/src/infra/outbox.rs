//! Outbox integration: enqueue dispatch (queues, partitions, payloads) and handler registration.

use std::sync::{Arc, OnceLock, Weak};

use serde::Serialize;
use toolkit_db::DbTx;
use toolkit_db::outbox::{Outbox, Record, Wake};
use uuid::Uuid;

use crate::config::OutboxConfig;
use crate::domain::error::DomainError;

pub const PAYLOAD_TYPE: &str = "application/json";

/// Logical queues of the gear.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueKind {
    Usage,
    Audit,
    AttachmentCleanup,
    ChatCleanup,
    ThreadSummary,
}

/// Holds the started outbox (weak) and the configured queue names.
pub struct OutboxDispatch {
    outbox: OnceLock<Weak<Outbox>>,
    cfg: OutboxConfig,
}

impl OutboxDispatch {
    #[must_use]
    pub fn new(cfg: OutboxConfig) -> Self {
        Self {
            outbox: OnceLock::new(),
            cfg,
        }
    }

    /// Binds the started pipeline.
    pub fn bind(&self, outbox: &Arc<Outbox>) {
        if self.outbox.set(Arc::downgrade(outbox)).is_err() {
            tracing::debug!("mini-chat: outbox pipeline already bound; keeping the first binding");
        }
    }

    #[must_use]
    pub fn queue_name(&self, q: QueueKind) -> &str {
        match q {
            QueueKind::Usage => &self.cfg.queue_name,
            QueueKind::Audit => &self.cfg.audit_queue_name,
            QueueKind::AttachmentCleanup => &self.cfg.cleanup_queue_name,
            QueueKind::ChatCleanup => &self.cfg.chat_cleanup_queue_name,
            QueueKind::ThreadSummary => &self.cfg.thread_summary_queue_name,
        }
    }

    #[must_use]
    pub fn partition(&self, key: Uuid) -> u32 {
        let n = u128::from(self.cfg.num_partitions.max(1));
        u32::try_from(key.as_u128() % n).unwrap_or(0)
    }

    /// Enqueues a JSON payload in the caller's transaction. Fire the returned wake after commit.
    ///
    /// # Errors
    /// Serialization or outbox failure.
    pub async fn enqueue<T: Serialize + Sync>(
        &self,
        tx: &DbTx<'_>,
        queue: QueueKind,
        partition_key: Uuid,
        payload: &T,
    ) -> Result<Wake, DomainError> {
        let outbox = self
            .outbox
            .get()
            .and_then(Weak::upgrade)
            .ok_or_else(|| DomainError::internal("outbox pipeline is not running"))?;
        let bytes =
            serde_json::to_vec(payload).map_err(|e| DomainError::internal(e.to_string()))?;
        let record = Record::to(self.queue_name(queue), self.partition(partition_key))
            .payload(bytes, PAYLOAD_TYPE)
            .build()?;
        Ok(outbox.enqueue(tx, record).await?)
    }
}
