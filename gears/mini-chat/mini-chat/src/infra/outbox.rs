//! Outbox integration: payload types and the enqueuer (DESIGN §5.6, B.9.3).
//! Handlers live in `infra::outbox_handlers`.

use std::sync::{Arc, OnceLock};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use toolkit_db::outbox::{Outbox, Record, Wake};
use toolkit_db::secure::DbTx;
use uuid::Uuid;

use crate::config::OutboxConfig;
use crate::domain::error::{DomainError, DomainResult};

pub const PT_USAGE: &str = "mini_chat.usage_event.v1";
pub const PT_AUDIT: &str = "mini_chat.audit_event.v1";
pub const PT_ATTACHMENT_CLEANUP: &str = "mini_chat.attachment_cleanup.v1";
pub const PT_CHAT_CLEANUP: &str = "mini_chat.chat_cleanup.v1";
pub const PT_THREAD_SUMMARY: &str = "mini_chat.thread_summary.v1";

/// Secondary (Anthropic) file reference of an attachment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecondaryRef {
    pub file_id: String,
    pub provider_kind: String,
    pub upstream_alias: String,
}

/// `mini-chat.attachment_cleanup` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentCleanupEvent {
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

/// `mini-chat.chat_cleanup` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatCleanupEvent {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub system_request_id: Uuid,
    pub reason: String,
    #[serde(with = "time::serde::rfc3339")]
    pub chat_deleted_at: OffsetDateTime,
}

/// `mini-chat.thread_summary` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadSummaryTask {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub system_request_id: Uuid,
    #[serde(with = "time::serde::rfc3339::option", default)]
    pub base_frontier_created_at: Option<OffsetDateTime>,
    pub base_frontier_message_id: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    pub frozen_target_created_at: OffsetDateTime,
    pub frozen_target_message_id: Uuid,
    pub system_task_type: String,
}

/// Enqueues payloads; bound to the running outbox at gear start.
pub struct OutboxEnqueuer {
    outbox: OnceLock<Arc<Outbox>>,
    pub cfg: OutboxConfig,
}

fn partition_of(key: Uuid, partitions: u32) -> u32 {
    let b = key.as_bytes();
    let h = u32::from_be_bytes([b[12], b[13], b[14], b[15]]);
    h % partitions.max(1)
}

impl OutboxEnqueuer {
    #[must_use]
    pub fn new(cfg: OutboxConfig) -> Self {
        Self { outbox: OnceLock::new(), cfg }
    }

    pub fn bind(&self, outbox: Arc<Outbox>) {
        drop(self.outbox.set(outbox));
    }

    #[must_use]
    pub fn is_bound(&self) -> bool {
        self.outbox.get().is_some()
    }

    async fn enqueue_json<T: Serialize>(
        &self,
        tx: &DbTx<'_>,
        queue: &str,
        key: Uuid,
        payload_type: &str,
        payload: &T,
    ) -> DomainResult<Wake> {
        let outbox = self
            .outbox
            .get()
            .ok_or_else(|| DomainError::internal("outbox pipeline is not running"))?;
        let bytes = serde_json::to_vec(payload).map_err(|e| DomainError::internal(format!("serialize outbox payload: {e}")))?;
        let record = Record::to(queue, partition_of(key, self.cfg.num_partitions))
            .payload(bytes, payload_type)
            .build()
            .map_err(|e| DomainError::internal(format!("outbox record: {e}")))?;
        outbox.enqueue(tx, record).await.map_err(|e| DomainError::internal(format!("outbox enqueue: {e}")))
    }

    /// # Errors
    /// Outbox not running or enqueue failure.
    pub async fn usage(&self, tx: &DbTx<'_>, ev: &mini_chat_sdk::UsageEvent) -> DomainResult<Wake> {
        self.enqueue_json(tx, &self.cfg.queue_name, ev.tenant_id, PT_USAGE, ev).await
    }

    /// # Errors
    /// Outbox not running or enqueue failure.
    pub async fn audit(&self, tx: &DbTx<'_>, ev: &mini_chat_sdk::AuditEvent) -> DomainResult<Wake> {
        self.enqueue_json(tx, &self.cfg.audit_queue_name, ev.tenant_id(), PT_AUDIT, ev).await
    }

    /// # Errors
    /// Outbox not running or enqueue failure.
    pub async fn attachment_cleanup(&self, tx: &DbTx<'_>, ev: &AttachmentCleanupEvent) -> DomainResult<Wake> {
        self.enqueue_json(tx, &self.cfg.cleanup_queue_name, ev.tenant_id, PT_ATTACHMENT_CLEANUP, ev).await
    }

    /// # Errors
    /// Outbox not running or enqueue failure.
    pub async fn chat_cleanup(&self, tx: &DbTx<'_>, ev: &ChatCleanupEvent) -> DomainResult<Wake> {
        self.enqueue_json(tx, &self.cfg.chat_cleanup_queue_name, ev.chat_id, PT_CHAT_CLEANUP, ev).await
    }

    /// # Errors
    /// Outbox not running or enqueue failure.
    pub async fn thread_summary(&self, tx: &DbTx<'_>, ev: &ThreadSummaryTask) -> DomainResult<Wake> {
        self.enqueue_json(tx, &self.cfg.thread_summary_queue_name, ev.chat_id, PT_THREAD_SUMMARY, ev).await
    }
}

/// Fire collected wakes after a commit.
pub fn fire(wakes: Vec<Wake>) {
    for w in wakes {
        w.fire();
    }
}
