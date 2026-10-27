//! Transactional outbox integration (shared `ToolKit` DB outbox).
//!
//! Five leased queues: usage (`mini-chat.usage_snapshot`), attachment cleanup
//! (`mini-chat.attachment_cleanup`), chat cleanup (`mini-chat.chat_cleanup`),
//! thread summary (`mini-chat.thread_summary`) and audit (`mini-chat.audit`).

pub mod handlers;
pub mod pipeline;

use std::sync::Arc;
use serde::{Deserialize, Serialize};
use toolkit_db::outbox::{Outbox, Record, Wake};
use toolkit_db::secure::DBRunner;
use uuid::Uuid;

use crate::error::DomainError;

/// Partition of a key (UUID bytes 14..16 mod partition count).
#[must_use]
pub fn partition_of(id: Uuid, num_partitions: u32) -> u32 {
    let bytes = id.as_bytes();
    u32::from(u16::from_be_bytes([bytes[14], bytes[15]])) % num_partitions
}

/// Attachment cleanup outbox payload (`mini-chat.attachment_cleanup`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentCleanupPayload {
    /// `attachment_deleted` | `attachment_upload_abandoned` |
    /// `attachment_indexing_failed`.
    pub event_type: String,
    /// Tenant.
    pub tenant_id: Uuid,
    /// Parent chat.
    pub chat_id: Uuid,
    /// Attachment.
    pub attachment_id: Uuid,
    /// Primary provider file id (`null` when the upload never reached the
    /// provider).
    pub provider_file_id: Option<String>,
    /// Always `null` in P1.
    pub vector_store_id: Option<String>,
    /// Upstream used to delete the primary file.
    pub storage_backend: String,
    /// `document` or `image`.
    pub attachment_kind: String,
    /// Enqueue time (RFC 3339).
    pub deleted_at: String,
    /// Secondary (Anthropic) copy reference, when a secondary upload
    /// succeeded.
    pub secondary_ref: Option<SecondaryRef>,
}

/// Secondary file reference (Anthropic Files API).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecondaryRef {
    /// Anthropic file id.
    pub file_id: String,
    /// Provider kind (`anthropic`).
    pub provider_kind: String,
    /// Upstream alias for the deletion call.
    pub upstream_alias: String,
}

/// Chat cleanup outbox payload (`mini-chat.chat_cleanup`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCleanupPayload {
    /// Tenant.
    pub tenant_id: Uuid,
    /// Chat.
    pub chat_id: Uuid,
    /// Stable system request id (generated once at enqueue time).
    pub system_request_id: Uuid,
    /// Always `chat_soft_delete`.
    pub reason: String,
    /// Soft-delete time (RFC 3339).
    pub chat_deleted_at: String,
}

/// Thread summary outbox payload (`mini-chat.thread_summary`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadSummaryPayload {
    /// Tenant.
    pub tenant_id: Uuid,
    /// Chat.
    pub chat_id: Uuid,
    /// Stable system request id (generated once at enqueue time).
    pub system_request_id: Uuid,
    /// Base frontier `created_at` (RFC 3339); `null` when no summary exists.
    pub base_frontier_created_at: Option<String>,
    /// Base frontier message id.
    pub base_frontier_message_id: Option<Uuid>,
    /// Frozen target frontier `created_at` (RFC 3339).
    pub frozen_target_created_at: String,
    /// Frozen target frontier message id.
    pub frozen_target_message_id: Uuid,
    /// Always `thread_summary_update`.
    pub system_task_type: String,
}

/// The outbox enqueue port used by the domain services.
pub trait OutboxEnqueuer: Send + Sync {
    /// Enqueue a usage event (partitioned by tenant).
    fn enqueue_usage<'a>(
        &'a self,
        tx: &'a (impl DBRunner + Sync),
        event: &'a mini_chat_sdk::UsageEvent,
    ) -> impl Future<Output = Result<Wake, DomainError>> + Send + 'a;

    /// Enqueue an audit event (partitioned by tenant).
    fn enqueue_audit<'a>(
        &'a self,
        tx: &'a (impl DBRunner + Sync),
        event: &'a mini_chat_sdk::TurnAuditEvent,
    ) -> impl Future<Output = Result<Wake, DomainError>> + Send + 'a;

    /// Enqueue an attachment cleanup event (partitioned by chat).
    fn enqueue_attachment_cleanup<'a>(
        &'a self,
        tx: &'a (impl DBRunner + Sync),
        payload: &'a AttachmentCleanupPayload,
    ) -> impl Future<Output = Result<Wake, DomainError>> + Send + 'a;

    /// Enqueue a chat cleanup event (partitioned by chat).
    fn enqueue_chat_cleanup<'a>(
        &'a self,
        tx: &'a (impl DBRunner + Sync),
        payload: &'a ChatCleanupPayload,
    ) -> impl Future<Output = Result<Wake, DomainError>> + Send + 'a;

    /// Enqueue a thread summary work item (partitioned by chat).
    fn enqueue_thread_summary<'a>(
        &'a self,
        tx: &'a (impl DBRunner + Sync),
        payload: &'a ThreadSummaryPayload,
    ) -> impl Future<Output = Result<Wake, DomainError>> + Send + 'a;
}

/// DB-backed enqueuer over the shared outbox.
pub struct DbEnqueuer {
    outbox: Arc<Outbox>,
    queue_usage: String,
    queue_audit: String,
    queue_attachment_cleanup: String,
    queue_chat_cleanup: String,
    queue_thread_summary: String,
    num_partitions: u32,
}

impl DbEnqueuer {
    /// Create the enqueuer.
    #[must_use]
    pub fn new(outbox: Arc<Outbox>, cfg: &crate::config::OutboxConfig) -> Self {
        Self {
            outbox,
            queue_usage: cfg.queue_name.clone(),
            queue_audit: cfg.audit_queue_name.clone(),
            queue_attachment_cleanup: cfg.cleanup_queue_name.clone(),
            queue_chat_cleanup: cfg.chat_cleanup_queue_name.clone(),
            queue_thread_summary: cfg.thread_summary_queue_name.clone(),
            num_partitions: cfg.num_partitions,
        }
    }

    async fn enqueue_json(
        &self,
        tx: &(impl DBRunner + Sync),
        queue: &str,
        partition: u32,
        value: &impl Serialize,
    ) -> Result<Wake, DomainError> {
        let bytes = serde_json::to_vec(value).map_err(|e| {
            tracing::error!("mini-chat: outbox payload serialization failed: {e}");
            DomainError::Internal
        })?;
        let record = Record::to(queue, partition)
            .payload(bytes, "application/json")
            .build()
            .map_err(|e| {
                tracing::debug!("mini-chat: outbox record build failed: {e}");
                DomainError::from(e)
            })?;
        let wake = self.outbox.enqueue(tx, record).await.map_err(|e| {
            tracing::debug!("mini-chat: outbox enqueue failed: {e}");
            DomainError::from(e)
        })?;
        Ok(wake)
    }
}

impl OutboxEnqueuer for DbEnqueuer {
    async fn enqueue_usage(
        &self,
        tx: &(impl DBRunner + Sync),
        event: &mini_chat_sdk::UsageEvent,
    ) -> Result<Wake, DomainError> {
        self.enqueue_json(
            tx,
            &self.queue_usage,
            partition_of(event.tenant_id, self.num_partitions),
            event,
        )
        .await
    }

    async fn enqueue_audit(
        &self,
        tx: &(impl DBRunner + Sync),
        event: &mini_chat_sdk::TurnAuditEvent,
    ) -> Result<Wake, DomainError> {
        self.enqueue_json(
            tx,
            &self.queue_audit,
            partition_of(event.tenant_id, self.num_partitions),
            event,
        )
        .await
    }

    async fn enqueue_attachment_cleanup(
        &self,
        tx: &(impl DBRunner + Sync),
        payload: &AttachmentCleanupPayload,
    ) -> Result<Wake, DomainError> {
        self.enqueue_json(
            tx,
            &self.queue_attachment_cleanup,
            partition_of(payload.chat_id, self.num_partitions),
            payload,
        )
        .await
    }

    async fn enqueue_chat_cleanup(
        &self,
        tx: &(impl DBRunner + Sync),
        payload: &ChatCleanupPayload,
    ) -> Result<Wake, DomainError> {
        self.enqueue_json(
            tx,
            &self.queue_chat_cleanup,
            partition_of(payload.chat_id, self.num_partitions),
            payload,
        )
        .await
    }

    async fn enqueue_thread_summary(
        &self,
        tx: &(impl DBRunner + Sync),
        payload: &ThreadSummaryPayload,
    ) -> Result<Wake, DomainError> {
        self.enqueue_json(
            tx,
            &self.queue_thread_summary,
            partition_of(payload.chat_id, self.num_partitions),
            payload,
        )
        .await
    }
}

/// An enqueuer whose backend is installed when the outbox pipeline starts.
/// Enqueues before installation fail with `DomainError::Internal`.
pub struct SharedEnqueuer {
    inner: std::sync::Arc<parking_lot::RwLock<Option<std::sync::Arc<DbEnqueuer>>>>,
}

impl SharedEnqueuer {
    /// Create an uninstalled enqueuer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: std::sync::Arc::new(parking_lot::RwLock::new(None)),
        }
    }

    /// Install the DB-backed enqueuer.
    pub fn install(&self, enqueuer: DbEnqueuer) {
        *self.inner.write() = Some(std::sync::Arc::new(enqueuer));
    }

    fn get(&self) -> Result<std::sync::Arc<DbEnqueuer>, DomainError> {
        self.inner.read().clone().ok_or_else(|| {
            tracing::error!("mini-chat: outbox enqueue before pipeline start");
            DomainError::Internal
        })
    }
}

macro_rules! shared_enqueue {
    ($name:ident, $ty:ty) => {
        async fn $name(
            &self,
            tx: &(impl DBRunner + Sync),
            value: &$ty,
        ) -> Result<Wake, DomainError> {
            self.get()?.$name(tx, value).await
        }
        // Forward to the installed enqueuer.
    };
}

impl OutboxEnqueuer for SharedEnqueuer {
    shared_enqueue!(enqueue_usage, mini_chat_sdk::UsageEvent);
    shared_enqueue!(enqueue_audit, mini_chat_sdk::TurnAuditEvent);
    shared_enqueue!(enqueue_attachment_cleanup, AttachmentCleanupPayload);
    shared_enqueue!(enqueue_chat_cleanup, ChatCleanupPayload);
    shared_enqueue!(enqueue_thread_summary, ThreadSummaryPayload);
}

/// A no-op enqueuer for tests and for construction before the outbox
/// pipeline starts.
pub struct NoopEnqueuer;

impl OutboxEnqueuer for NoopEnqueuer {
    async fn enqueue_usage(
        &self,
        _tx: &(impl DBRunner + Sync),
        _event: &mini_chat_sdk::UsageEvent,
    ) -> Result<Wake, DomainError> {
        Ok(Wake::empty())
    }

    async fn enqueue_audit(
        &self,
        _tx: &(impl DBRunner + Sync),
        _event: &mini_chat_sdk::TurnAuditEvent,
    ) -> Result<Wake, DomainError> {
        Ok(Wake::empty())
    }

    async fn enqueue_attachment_cleanup(
        &self,
        _tx: &(impl DBRunner + Sync),
        _payload: &AttachmentCleanupPayload,
    ) -> Result<Wake, DomainError> {
        Ok(Wake::empty())
    }

    async fn enqueue_chat_cleanup(
        &self,
        _tx: &(impl DBRunner + Sync),
        _payload: &ChatCleanupPayload,
    ) -> Result<Wake, DomainError> {
        Ok(Wake::empty())
    }

    async fn enqueue_thread_summary(
        &self,
        _tx: &(impl DBRunner + Sync),
        _payload: &ThreadSummaryPayload,
    ) -> Result<Wake, DomainError> {
        Ok(Wake::empty())
    }
}
