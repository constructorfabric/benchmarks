//! Transactional enqueue into the gear's outbox queues.
//!
//! Enqueue only inside business transactions and fire the returned
//! [`PendingWakes`] after commit (collect several with [`PendingWakes::push`]).

use std::sync::{Arc, OnceLock};

use serde::Serialize;
use toolkit_db::outbox::{Outbox, OutboxError, Record, Wake};
use toolkit_db::secure::DBRunner;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::config::OutboxConfig;
use crate::domain::error::{DomainError, DomainResult, is_db_contention};

/// Maximum serialized payload size accepted by the outbox.
pub const MAX_PAYLOAD_BYTES: usize = 64 * 1024;

/// The gear's five outbox queues (names from `outbox.*` config).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QueueKind {
    /// Usage settlement events (partition by tenant).
    Usage,
    /// Attachment provider-file cleanup (partition by tenant).
    AttachmentCleanup,
    /// Chat soft-delete cleanup (partition by chat).
    ChatCleanup,
    /// Thread summary updates (partition by chat).
    ThreadSummary,
    /// Audit events (partition by tenant).
    Audit,
}

impl QueueKind {
    pub const ALL: [Self; 5] = [
        Self::Usage,
        Self::AttachmentCleanup,
        Self::ChatCleanup,
        Self::ThreadSummary,
        Self::Audit,
    ];

    /// Configured queue name.
    #[must_use]
    pub fn queue_name(self, cfg: &OutboxConfig) -> &str {
        match self {
            Self::Usage => &cfg.queue_name,
            Self::AttachmentCleanup => &cfg.cleanup_queue_name,
            Self::ChatCleanup => &cfg.chat_cleanup_queue_name,
            Self::ThreadSummary => &cfg.thread_summary_queue_name,
            Self::Audit => &cfg.audit_queue_name,
        }
    }
}

/// Stable partition of `key`: its first 4 bytes (big endian) modulo `partitions`.
#[must_use]
pub fn partition_for(key: Uuid, partitions: u32) -> u32 {
    let b = key.as_bytes();
    u32::from_be_bytes([b[0], b[1], b[2], b[3]]) % partitions.max(1)
}

/// Enqueues JSON payloads into the started outbox pipeline.
pub struct OutboxEnqueuer {
    outbox: OnceLock<Arc<Outbox>>,
    cfg: OutboxConfig,
}

impl OutboxEnqueuer {
    #[must_use]
    pub fn new(cfg: OutboxConfig) -> Self {
        Self {
            outbox: OnceLock::new(),
            cfg,
        }
    }

    /// Attach the started pipeline (first call wins).
    pub fn set_outbox(&self, outbox: Arc<Outbox>) {
        if self.outbox.set(outbox).is_err() {
            warn!("mini-chat outbox already attached; ignoring a second pipeline");
        }
    }

    /// Outbox queue configuration.
    #[must_use]
    pub fn config(&self) -> &OutboxConfig {
        &self.cfg
    }

    /// Serialize `payload` and enqueue it on `queue`, partitioned by `partition_key`.
    ///
    /// Returns the wakes to fire after the transaction commits.
    ///
    /// # Errors
    /// `OutboxPayloadTooLarge` when the JSON exceeds [`MAX_PAYLOAD_BYTES`];
    /// `Internal` when the pipeline is not started or the write fails.
    pub async fn enqueue_json<T: Serialize>(
        &self,
        tx: &(impl DBRunner + Sync),
        queue: QueueKind,
        partition_key: Uuid,
        payload_type: &str,
        payload: &T,
    ) -> DomainResult<PendingWakes> {
        let bytes = serde_json::to_vec(payload)
            .map_err(|e| DomainError::internal(format!("serialize outbox payload: {e}")))?;
        if bytes.len() > MAX_PAYLOAD_BYTES {
            return Err(too_large(bytes.len()));
        }
        let outbox = self
            .outbox
            .get()
            .ok_or_else(|| DomainError::internal("outbox not started"))?;
        let partition = partition_for(partition_key, self.cfg.num_partitions);
        let record = Record::to(queue.queue_name(&self.cfg), partition)
            .payload(bytes, payload_type)
            .build()
            .map_err(map_outbox_err)?;
        outbox
            .enqueue(tx, record)
            .await
            .map(PendingWakes::from)
            .map_err(map_outbox_err)
    }
}

fn too_large(size: usize) -> DomainError {
    DomainError::OutboxPayloadTooLarge(format!(
        "The payload of {size} bytes exceeds the limit of {MAX_PAYLOAD_BYTES} bytes"
    ))
}

fn map_outbox_err(err: OutboxError) -> DomainError {
    match err {
        OutboxError::PayloadTooLarge { size, .. } => too_large(size),
        OutboxError::Database(e) if is_db_contention(&e) => {
            DomainError::DbContention(format!("outbox enqueue: {e}"))
        }
        other => DomainError::internal(format!("outbox enqueue: {other}")),
    }
}

/// Wakes collected inside a transaction, fired together after commit.
///
/// [`fire`](Self::fire) them once the transaction has committed. Dropped
/// unfired, they are discarded: that is the path of a failed transaction
/// attempt (a body error, or a commit that failed after the body returned
/// them, both inside [`with_tx_retry`](crate::infra::db::tx::with_tx_retry)),
/// whose rows were rolled back and must not wake a sequencer.
#[derive(Debug)]
#[must_use = "fire() the pending wakes after the transaction commits"]
pub struct PendingWakes {
    wake: Wake,
}

impl Default for PendingWakes {
    fn default() -> Self {
        Self {
            wake: Wake::empty(),
        }
    }
}

impl PendingWakes {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add the wakes of `other` (e.g. one more enqueue).
    pub fn push(&mut self, mut other: Self) {
        self.wake += std::mem::replace(&mut other.wake, Wake::empty());
    }

    /// Wake the sequencers of every collected partition.
    pub fn fire(mut self) {
        std::mem::replace(&mut self.wake, Wake::empty()).fire();
    }
}

impl From<Wake> for PendingWakes {
    fn from(wake: Wake) -> Self {
        Self { wake }
    }
}

impl Drop for PendingWakes {
    fn drop(&mut self) {
        let wake = std::mem::replace(&mut self.wake, Wake::empty());
        if !wake.partitions().is_empty() {
            debug!(
                messages = wake.ids().len(),
                "outbox wakes of a rolled-back transaction attempt discarded"
            );
        }
        wake.discard();
    }
}
