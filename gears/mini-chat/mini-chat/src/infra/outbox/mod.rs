//! Outbox integration: enqueuer, payloads and the five leased queue handlers.

pub mod handlers;

use std::sync::{Arc, OnceLock, Weak};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use toolkit_db::DbTx;
use toolkit_db::outbox::{Outbox, Record, Wake};
use uuid::Uuid;

use crate::config::OutboxConfig;
use crate::domain::error::{DomainError, DomainResult};

pub const PT_USAGE: &str = "mini_chat.usage_event.v1";
pub const PT_AUDIT: &str = "mini_chat.audit_event.v1";
pub const PT_ATTACHMENT_CLEANUP: &str = "mini_chat.attachment_cleanup.v1";
pub const PT_CHAT_CLEANUP: &str = "mini_chat.chat_cleanup.v1";
pub const PT_THREAD_SUMMARY: &str = "mini_chat.thread_summary.v1";

/// Attachment cleanup message (`attachment_deleted`, `attachment_upload_abandoned`,
/// `attachment_indexing_failed`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttachmentCleanupPayload {
    pub event_type: String,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub attachment_id: Uuid,
    pub provider_file_id: Option<String>,
    pub vector_store_id: Option<String>,
    pub storage_backend: String,
    pub attachment_kind: String,
    #[serde(with = "time::serde::rfc3339")]
    pub deleted_at: OffsetDateTime,
    pub secondary_ref: Option<SecondaryRef>,
}

/// Secondary (Anthropic) copy of an attachment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecondaryRef {
    pub file_id: String,
    pub provider_kind: String,
    pub upstream_alias: String,
}

/// Chat cleanup message written by the chat soft delete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatCleanupPayload {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub system_request_id: Uuid,
    pub reason: String,
    #[serde(with = "time::serde::rfc3339")]
    pub chat_deleted_at: OffsetDateTime,
}

/// Thread summary work item with its frozen range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadSummaryPayload {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub system_request_id: Uuid,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub base_frontier_created_at: Option<OffsetDateTime>,
    pub base_frontier_message_id: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    pub frozen_target_created_at: OffsetDateTime,
    pub frozen_target_message_id: Uuid,
    pub system_task_type: String,
}

fn partition_of(id: Uuid, partitions: u32) -> u32 {
    let b = id.as_bytes();
    u32::from(u16::from_be_bytes([b[14], b[15]])) % partitions.max(1)
}

/// Enqueues mini-chat outbox messages inside a caller transaction.
pub struct OutboxEnqueuer {
    outbox: OnceLock<Weak<Outbox>>,
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

    /// Binds the started pipeline.
    pub fn bind(&self, outbox: &Arc<Outbox>) {
        // Only the first bind wins; later binds are ignored, as before.
        self.outbox.set(Arc::downgrade(outbox)).ok();
    }

    #[must_use]
    pub fn config(&self) -> &OutboxConfig {
        &self.cfg
    }

    async fn enqueue<T: Serialize>(
        &self,
        tx: &DbTx<'_>,
        queue: &str,
        partition_key: Uuid,
        payload_type: &str,
        payload: &T,
    ) -> DomainResult<Wake> {
        let outbox = self
            .outbox
            .get()
            .and_then(Weak::upgrade)
            .ok_or_else(|| DomainError::internal("outbox pipeline is not running"))?;
        let bytes = serde_json::to_vec(payload)?;
        let record = Record::to(queue, partition_of(partition_key, self.cfg.num_partitions))
            .payload(bytes, payload_type)
            .build()?;
        Ok(outbox.enqueue(tx, record).await?)
    }

    pub async fn usage(&self, tx: &DbTx<'_>, ev: &mini_chat_sdk::UsageEvent) -> DomainResult<Wake> {
        self.enqueue(tx, &self.cfg.queue_name, ev.tenant_id, PT_USAGE, ev)
            .await
    }

    pub async fn audit(
        &self,
        tx: &DbTx<'_>,
        ev: &mini_chat_sdk::MiniChatAuditEvent,
    ) -> DomainResult<Wake> {
        self.enqueue(tx, &self.cfg.audit_queue_name, ev.tenant_id(), PT_AUDIT, ev)
            .await
    }

    pub async fn attachment_cleanup(
        &self,
        tx: &DbTx<'_>,
        p: &AttachmentCleanupPayload,
    ) -> DomainResult<Wake> {
        self.enqueue(
            tx,
            &self.cfg.cleanup_queue_name,
            p.tenant_id,
            PT_ATTACHMENT_CLEANUP,
            p,
        )
        .await
    }

    pub async fn chat_cleanup(&self, tx: &DbTx<'_>, p: &ChatCleanupPayload) -> DomainResult<Wake> {
        self.enqueue(
            tx,
            &self.cfg.chat_cleanup_queue_name,
            p.chat_id,
            PT_CHAT_CLEANUP,
            p,
        )
        .await
    }

    pub async fn thread_summary(
        &self,
        tx: &DbTx<'_>,
        p: &ThreadSummaryPayload,
    ) -> DomainResult<Wake> {
        self.enqueue(
            tx,
            &self.cfg.thread_summary_queue_name,
            p.chat_id,
            PT_THREAD_SUMMARY,
            p,
        )
        .await
    }
}

/// Wakes collected inside a transaction, fired after commit.
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
