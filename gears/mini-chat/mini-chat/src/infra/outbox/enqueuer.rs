//! [`OutboxEnqueuer`]: the [`OutboxPort`] adapter over
//! `toolkit_db::outbox::Outbox`.
//!
//! Each method serializes the payload to JSON and writes it with
//! `Outbox::enqueue` on the caller's transaction, pushing the returned wake
//! into the caller's [`PendingWakes`] (fired after commit). The `Outbox` is
//! installed once the pipeline is started ([`OutboxEnqueuer::set_outbox`]);
//! enqueueing before that is `DomainError::Internal`. A payload above the
//! platform size limit is `DomainError::OutboxPayloadTooLarge`.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use mini_chat_sdk::{MiniChatAuditEvent, UsageEvent};
use serde::Serialize;
use toolkit_db::DbTx;
use toolkit_db::outbox::{Outbox, OutboxError, Record};
use uuid::Uuid;

use super::payloads::{AttachmentCleanupPayload, ChatCleanupPayload, ThreadSummaryPayload};
use super::{PAYLOAD_TYPE, partition_for};
use crate::config::OutboxConfig;
use crate::domain::error::DomainError;
use crate::domain::ports::{OutboxPort, PendingWakes};

/// Outbox-backed implementation of [`OutboxPort`].
pub struct OutboxEnqueuer {
    cfg: OutboxConfig,
    outbox: OnceLock<Arc<Outbox>>,
}

impl std::fmt::Debug for OutboxEnqueuer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutboxEnqueuer")
            .field("cfg", &self.cfg)
            .field("ready", &self.is_ready())
            .finish_non_exhaustive()
    }
}

impl OutboxEnqueuer {
    /// Enqueuer for the queues named in `cfg` (not ready until `set_outbox`).
    #[must_use]
    pub fn new(cfg: OutboxConfig) -> Self {
        Self {
            cfg,
            outbox: OnceLock::new(),
        }
    }

    /// Install the started pipeline's `Outbox` (once).
    ///
    /// # Errors
    ///
    /// `DomainError::Internal` when an outbox was already installed.
    pub fn set_outbox(&self, outbox: Arc<Outbox>) -> Result<(), DomainError> {
        self.outbox
            .set(outbox)
            .map_err(|_| DomainError::Internal("mini-chat outbox already installed".to_owned()))
    }

    /// Whether the pipeline's `Outbox` is installed.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.outbox.get().is_some()
    }

    async fn enqueue<T: Serialize + Sync>(
        &self,
        tx: &DbTx<'_>,
        queue: &str,
        key: Uuid,
        payload: &T,
        wakes: &mut PendingWakes,
    ) -> Result<(), DomainError> {
        let outbox = self.outbox.get().ok_or_else(|| {
            DomainError::Internal(format!(
                "mini-chat outbox pipeline not started; cannot enqueue on {queue}"
            ))
        })?;
        let bytes = serde_json::to_vec(payload)
            .map_err(|e| DomainError::Internal(format!("serialize {queue} payload: {e}")))?;
        let record = Record::to(queue, partition_for(key, self.cfg.num_partitions))
            .payload(bytes, PAYLOAD_TYPE)
            .build()
            .map_err(|e| match e {
                OutboxError::PayloadTooLarge { .. } => {
                    DomainError::OutboxPayloadTooLarge(format!("{queue}: {e}"))
                }
                e => DomainError::Internal(format!("build {queue} record: {e}")),
            })?;
        let wake = outbox.enqueue(tx, record).await.map_err(|e| match e {
            // Keep the driver error: lock contention retries the transaction.
            OutboxError::Database(db) => DomainError::from(db),
            e => DomainError::Database(format!("enqueue on {queue}: {e}")),
        })?;
        wakes.push(wake);
        Ok(())
    }
}

fn audit_tenant(ev: &MiniChatAuditEvent) -> Uuid {
    match ev {
        MiniChatAuditEvent::Turn(e) => e.tenant_id,
        MiniChatAuditEvent::Mutation(e) => e.tenant_id,
    }
}

#[async_trait]
impl OutboxPort for OutboxEnqueuer {
    async fn enqueue_usage(
        &self,
        tx: &DbTx<'_>,
        ev: &UsageEvent,
        wakes: &mut PendingWakes,
    ) -> Result<(), DomainError> {
        self.enqueue(tx, &self.cfg.queue_name, ev.tenant_id, ev, wakes)
            .await
    }

    async fn enqueue_audit(
        &self,
        tx: &DbTx<'_>,
        ev: &MiniChatAuditEvent,
        wakes: &mut PendingWakes,
    ) -> Result<(), DomainError> {
        self.enqueue(tx, &self.cfg.audit_queue_name, audit_tenant(ev), ev, wakes)
            .await
    }

    async fn enqueue_attachment_cleanup(
        &self,
        tx: &DbTx<'_>,
        p: &AttachmentCleanupPayload,
        wakes: &mut PendingWakes,
    ) -> Result<(), DomainError> {
        self.enqueue(tx, &self.cfg.cleanup_queue_name, p.tenant_id, p, wakes)
            .await
    }

    async fn enqueue_chat_cleanup(
        &self,
        tx: &DbTx<'_>,
        p: &ChatCleanupPayload,
        wakes: &mut PendingWakes,
    ) -> Result<(), DomainError> {
        self.enqueue(tx, &self.cfg.chat_cleanup_queue_name, p.chat_id, p, wakes)
            .await
    }

    async fn enqueue_thread_summary(
        &self,
        tx: &DbTx<'_>,
        p: &ThreadSummaryPayload,
        wakes: &mut PendingWakes,
    ) -> Result<(), DomainError> {
        self.enqueue(tx, &self.cfg.thread_summary_queue_name, p.chat_id, p, wakes)
            .await
    }
}
