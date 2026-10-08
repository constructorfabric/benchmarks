//! Enqueue side of the shared toolkit outbox. The pipeline is started in
//! `gear.rs` and bound here after `start()`.

use std::sync::{Arc, OnceLock, Weak};

use serde::Serialize;
use toolkit_db::outbox::{Outbox, OutboxError, Record, Wake};
use toolkit_db::secure::DBRunner;
use uuid::Uuid;

use crate::config::OutboxConfig;
use crate::domain::errors::{DomainError, DomainResult};

pub const PAYLOAD_USAGE: &str = "mini_chat.usage_event";
pub const PAYLOAD_AUDIT: &str = "mini_chat.audit_event";
pub const PAYLOAD_ATTACHMENT_CLEANUP: &str = "mini_chat.attachment_cleanup";
pub const PAYLOAD_CHAT_CLEANUP: &str = "mini_chat.chat_cleanup";
pub const PAYLOAD_THREAD_SUMMARY: &str = "mini_chat.thread_summary";

/// Logical queues of the gear.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Queue {
    Usage,
    Audit,
    AttachmentCleanup,
    ChatCleanup,
    ThreadSummary,
}

pub struct OutboxDispatch {
    cfg: OutboxConfig,
    outbox: OnceLock<Weak<Outbox>>,
}

impl OutboxDispatch {
    #[must_use]
    pub fn new(cfg: OutboxConfig) -> Self {
        Self {
            cfg,
            outbox: OnceLock::new(),
        }
    }

    pub fn bind(&self, outbox: &Arc<Outbox>) {
        let _ = self.outbox.set(Arc::downgrade(outbox));
    }

    #[must_use]
    pub fn queue_name(&self, q: Queue) -> &str {
        match q {
            Queue::Usage => &self.cfg.queue_name,
            Queue::Audit => &self.cfg.audit_queue_name,
            Queue::AttachmentCleanup => &self.cfg.cleanup_queue_name,
            Queue::ChatCleanup => &self.cfg.chat_cleanup_queue_name,
            Queue::ThreadSummary => &self.cfg.thread_summary_queue_name,
        }
    }

    fn payload_type(q: Queue) -> &'static str {
        match q {
            Queue::Usage => PAYLOAD_USAGE,
            Queue::Audit => PAYLOAD_AUDIT,
            Queue::AttachmentCleanup => PAYLOAD_ATTACHMENT_CLEANUP,
            Queue::ChatCleanup => PAYLOAD_CHAT_CLEANUP,
            Queue::ThreadSummary => PAYLOAD_THREAD_SUMMARY,
        }
    }

    #[must_use]
    pub fn partition(&self, key: Uuid) -> u32 {
        let b = key.as_bytes();
        let n = u32::from(u16::from_be_bytes([b[14], b[15]]));
        n % self.cfg.num_partitions.max(1)
    }

    /// Serialize and enqueue one message inside the caller's transaction.
    ///
    /// # Errors
    /// Payload too large (400 format), outbox not running, DB failure.
    pub async fn enqueue<T: Serialize + Sync>(
        &self,
        runner: &(impl DBRunner + Sync),
        q: Queue,
        partition_key: Uuid,
        payload: &T,
    ) -> DomainResult<Wake> {
        let outbox = self
            .outbox
            .get()
            .and_then(Weak::upgrade)
            .ok_or_else(|| DomainError::internal("outbox is not running"))?;
        let bytes = serde_json::to_vec(payload)
            .map_err(|e| DomainError::internal(format!("outbox payload: {e}")))?;
        let record = Record::to(self.queue_name(q), self.partition(partition_key))
            .payload(bytes, Self::payload_type(q))
            .build()
            .map_err(DomainError::from)?;
        outbox
            .enqueue(runner, record)
            .await
            .map_err(|e: OutboxError| DomainError::from(e))
    }
}

/// Accumulated wakes of a transaction, fired after commit.
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
