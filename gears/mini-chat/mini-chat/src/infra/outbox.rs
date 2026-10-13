//! Producer side of the gear's shared-outbox queues (DESIGN section 5.7
//! "Shared outbox implementation", "Cleanup on Chat Deletion", section 4
//! "Attachment Deletion", section 3.6 "Thread Summary" durable scheduling).
//!
//! The gear owns five leased queues, all with `outbox.num_partitions`
//! partitions ([`MiniChatQueue`], names from [`OutboxQueues`]):
//!
//! | Queue | Payload | Partition key |
//! |---|---|---|
//! | usage (`outbox.queue_name`) | SDK [`UsageEvent`] | `tenant_id` |
//! | attachment cleanup (`outbox.cleanup_queue_name`) | [`AttachmentCleanupPayload`] | `tenant_id` |
//! | chat cleanup (`outbox.chat_cleanup_queue_name`) | [`ChatCleanupPayload`] | `chat_id` |
//! | thread summary (`outbox.thread_summary_queue_name`) | [`ThreadSummaryPayload`] | `chat_id` |
//! | audit (`outbox.audit_queue_name`) | SDK [`AuditEvent`] | `tenant_id` |
//!
//! Payloads are JSON (`application/json`). Enqueue runs inside the caller's
//! business transaction and returns the outbox [`Wake`], which is fired only
//! after the commit (`toolkit_db::outbox::in_transaction` does that).
//!
//! The pipeline itself (queue registration, handlers, workers) is started by
//! the gear lifecycle; [`OutboxEnqueuer::attach`] hands its [`Outbox`] to the
//! producers once it runs. Enqueueing before that is an internal error.

use std::sync::{Arc, OnceLock};

use mini_chat_sdk::{AuditEvent, UsageEvent};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use toolkit_db::outbox::{Outbox, OutboxError, Partitions, Record, Wake};
use toolkit_db::secure::DBRunner;
use uuid::Uuid;

use crate::config::OutboxConfig;
use crate::domain::error::DomainError;

/// Payload type of every mini-chat outbox message.
pub const JSON_PAYLOAD_TYPE: &str = "application/json";

// ── Queues ───────────────────────────────────────────────────────────────

/// The gear's outbox queues.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MiniChatQueue {
    /// Usage events (`UsageEvent`), partitioned by tenant.
    Usage,
    /// Provider file cleanup of one attachment, partitioned by tenant.
    AttachmentCleanup,
    /// Provider cleanup of a soft-deleted chat, partitioned by chat.
    ChatCleanup,
    /// Thread summary update, partitioned by chat.
    ThreadSummary,
    /// Audit events (`AuditEvent`), partitioned by tenant.
    Audit,
}

impl MiniChatQueue {
    /// Every queue, in registration order.
    pub const ALL: [Self; 5] = [
        Self::Usage,
        Self::AttachmentCleanup,
        Self::ChatCleanup,
        Self::ThreadSummary,
        Self::Audit,
    ];
}

/// Configured queue names and the shared partition count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxQueues {
    pub usage: String,
    pub attachment_cleanup: String,
    pub chat_cleanup: String,
    pub thread_summary: String,
    pub audit: String,
    /// `outbox.num_partitions` (validated power of two in 1..=64).
    pub num_partitions: u16,
}

impl OutboxQueues {
    /// Queue names and partition count of `config`.
    #[must_use]
    pub fn from_config(config: &OutboxConfig) -> Self {
        Self {
            usage: config.queue_name.clone(),
            attachment_cleanup: config.cleanup_queue_name.clone(),
            chat_cleanup: config.chat_cleanup_queue_name.clone(),
            thread_summary: config.thread_summary_queue_name.clone(),
            audit: config.audit_queue_name.clone(),
            num_partitions: u16::try_from(config.num_partitions.max(1)).unwrap_or(u16::MAX),
        }
    }

    /// Configured name of `queue`.
    #[must_use]
    pub fn name(&self, queue: MiniChatQueue) -> &str {
        match queue {
            MiniChatQueue::Usage => &self.usage,
            MiniChatQueue::AttachmentCleanup => &self.attachment_cleanup,
            MiniChatQueue::ChatCleanup => &self.chat_cleanup,
            MiniChatQueue::ThreadSummary => &self.thread_summary,
            MiniChatQueue::Audit => &self.audit,
        }
    }

    /// Partition count to register every queue with.
    #[must_use]
    pub fn partitions(&self) -> Partitions {
        Partitions::of(self.num_partitions)
    }

    /// Stable partition of `key` (a tenant or chat id) in `0..num_partitions`.
    #[must_use]
    pub fn partition_for(&self, key: Uuid) -> u32 {
        let bits = key.as_u128();
        #[allow(
            clippy::cast_possible_truncation,
            reason = "folding the two halves, then taking the remainder of a u16 count"
        )]
        let folded = (bits as u64) ^ ((bits >> 64) as u64);
        let count = u64::from(self.num_partitions.max(1));
        u32::try_from(folded % count).unwrap_or(0)
    }
}

impl Default for OutboxQueues {
    fn default() -> Self {
        Self::from_config(&OutboxConfig::default())
    }
}

// ── Payloads ─────────────────────────────────────────────────────────────

/// Usage queue payload: the SDK usage event (no `event_type` field).
pub type UsagePayload = UsageEvent;

/// Audit queue payload: the SDK audit event (flat, with `event_type`).
pub type AuditPayload = AuditEvent;

/// `reason` of a chat-cleanup message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatCleanupReason {
    /// `DELETE /v1/chats/{id}`.
    ChatSoftDelete,
}

/// Chat-cleanup queue payload, serialized in the soft-delete transaction.
///
/// `system_request_id` is generated once at enqueue time and reused by every
/// retry or replay of the message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatCleanupPayload {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub system_request_id: Uuid,
    pub reason: ChatCleanupReason,
    #[serde(with = "time::serde::rfc3339")]
    pub chat_deleted_at: OffsetDateTime,
}

/// `event_type` of an attachment-cleanup message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttachmentCleanupEvent {
    /// `attachment_deleted`: `DELETE /v1/chats/{id}/attachments/{attachment_id}`.
    #[serde(rename = "attachment_deleted")]
    Deleted,
    /// `attachment_upload_abandoned`: upload reaper, abandoned upload with a
    /// provider file.
    #[serde(rename = "attachment_upload_abandoned")]
    UploadAbandoned,
    /// `attachment_indexing_failed`: background indexing failed or timed out.
    #[serde(rename = "attachment_indexing_failed")]
    IndexingFailed,
}

/// Secondary (Anthropic) provider copy of an attachment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecondaryFileRef {
    pub file_id: String,
    pub provider_kind: String,
    pub upstream_alias: String,
}

/// Attachment-cleanup queue payload (DESIGN "Attachment Deletion" field
/// table). Nullable fields are serialized as `null`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentCleanupPayload {
    pub event_type: AttachmentCleanupEvent,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub attachment_id: Uuid,
    /// Primary provider file; `None` when the upload never reached the provider.
    pub provider_file_id: Option<String>,
    /// Always `None` in P1; the handler does not read it.
    pub vector_store_id: Option<String>,
    /// Upstream used to delete the primary file.
    pub storage_backend: String,
    /// `document` or `image`.
    pub attachment_kind: String,
    /// Enqueue time.
    #[serde(with = "time::serde::rfc3339")]
    pub deleted_at: OffsetDateTime,
    /// Set when a secondary upload succeeded.
    pub secondary_ref: Option<SecondaryFileRef>,
}

/// `system_task_type` of a thread-summary message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemTaskType {
    ThreadSummaryUpdate,
}

/// Thread-summary queue payload (DESIGN section 3.6 "Durable scheduling").
///
/// The base frontier is the summary frontier at enqueue time (`None` before
/// the first summary); the frozen target is the inclusive upper bound of the
/// range to summarize, captured at enqueue time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadSummaryPayload {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub system_request_id: Uuid,
    pub system_task_type: SystemTaskType,
    #[serde(with = "time::serde::rfc3339::option")]
    pub base_frontier_created_at: Option<OffsetDateTime>,
    pub base_frontier_message_id: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    pub frozen_target_created_at: OffsetDateTime,
    pub frozen_target_message_id: Uuid,
}

// ── Enqueue ──────────────────────────────────────────────────────────────

/// Why an enqueue failed.
#[derive(Debug, thiserror::Error)]
pub enum EnqueueError {
    /// The outbox pipeline has not been attached yet.
    #[error("outbox is not started")]
    NotStarted,
    /// The payload could not be serialized.
    #[error("payload serialization failed: {0}")]
    Serialize(String),
    /// The serialized payload exceeds the outbox size limit.
    #[error("payload size {size} exceeds the outbox limit {max}")]
    PayloadTooLarge { size: usize, max: usize },
    /// Queue, partition or database failure.
    #[error("outbox enqueue failed")]
    Outbox(#[source] OutboxError),
}

impl EnqueueError {
    /// Fixed, driver-free classification of the failure.
    #[must_use]
    pub const fn class(&self) -> &'static str {
        match self {
            Self::NotStarted => "outbox is not started",
            Self::Serialize(_) => "payload serialization failed",
            Self::PayloadTooLarge { .. } => "payload exceeds the outbox size limit",
            Self::Outbox(_) => "queue, partition or database failure",
        }
    }
}

impl From<OutboxError> for EnqueueError {
    fn from(err: OutboxError) -> Self {
        match err {
            OutboxError::PayloadTooLarge { size, max } => Self::PayloadTooLarge { size, max },
            other => Self::Outbox(other),
        }
    }
}

impl From<EnqueueError> for DomainError {
    /// Every enqueue failure is internal by default (a fixed diagnostic, as
    /// for database errors); callers whose contract names a client error for
    /// an oversized payload map [`EnqueueError::PayloadTooLarge`] themselves.
    fn from(err: EnqueueError) -> Self {
        Self::internal(format!("outbox enqueue failed: {}", err.class()))
    }
}

/// Transactional producer of the gear's outbox queues.
#[derive(Default)]
pub struct OutboxEnqueuer {
    queues: OutboxQueues,
    outbox: OnceLock<Arc<Outbox>>,
}

impl std::fmt::Debug for OutboxEnqueuer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutboxEnqueuer")
            .field("queues", &self.queues)
            .field("attached", &self.outbox.get().is_some())
            .finish()
    }
}

impl OutboxEnqueuer {
    /// A producer for `queues`, not yet attached to a running outbox.
    #[must_use]
    pub fn new(queues: OutboxQueues) -> Self {
        Self {
            queues,
            outbox: OnceLock::new(),
        }
    }

    /// Attaches the running outbox (from `OutboxHandle::outbox()`); every
    /// queue of [`Self::queues`] must be registered on it. A second call
    /// keeps the first outbox and returns `false`.
    pub fn attach(&self, outbox: Arc<Outbox>) -> bool {
        self.outbox.set(outbox).is_ok()
    }

    /// Whether an outbox is attached.
    #[must_use]
    pub fn is_attached(&self) -> bool {
        self.outbox.get().is_some()
    }

    /// Configured queues.
    #[must_use]
    pub const fn queues(&self) -> &OutboxQueues {
        &self.queues
    }

    /// Serializes `payload` as JSON and enqueues it to `queue`, on the
    /// partition of `partition_key`, through `tx` (normally the business
    /// transaction). Fire the returned [`Wake`] after the commit.
    ///
    /// # Errors
    ///
    /// [`EnqueueError`] when the outbox is not attached, the payload cannot be
    /// serialized or is too large, the queue is not registered, or the
    /// database rejects the write.
    pub async fn enqueue_in_tx<P>(
        &self,
        tx: &(impl DBRunner + Sync),
        queue: MiniChatQueue,
        partition_key: Uuid,
        payload: &P,
    ) -> Result<Wake, EnqueueError>
    where
        P: Serialize + Sync,
    {
        let outbox = self.outbox.get().ok_or(EnqueueError::NotStarted)?;
        let bytes =
            serde_json::to_vec(payload).map_err(|err| EnqueueError::Serialize(err.to_string()))?;
        let name = self.queues.name(queue);
        let partition = self.queues.partition_for(partition_key);
        let record = Record::to(name, partition)
            .payload(bytes, JSON_PAYLOAD_TYPE)
            .build()?;
        Ok(outbox.enqueue(tx, record).await?)
    }

    /// Enqueues a usage event (partition: tenant).
    ///
    /// # Errors
    ///
    /// See [`Self::enqueue_in_tx`].
    pub async fn enqueue_usage(
        &self,
        tx: &(impl DBRunner + Sync),
        event: &UsagePayload,
    ) -> Result<Wake, EnqueueError> {
        self.enqueue_in_tx(tx, MiniChatQueue::Usage, event.tenant_id, event)
            .await
    }

    /// Enqueues an audit event (partition: tenant).
    ///
    /// # Errors
    ///
    /// See [`Self::enqueue_in_tx`].
    pub async fn enqueue_audit(
        &self,
        tx: &(impl DBRunner + Sync),
        event: &AuditPayload,
    ) -> Result<Wake, EnqueueError> {
        self.enqueue_in_tx(tx, MiniChatQueue::Audit, event.tenant_id(), event)
            .await
    }

    /// Enqueues a chat-cleanup message (partition: chat).
    ///
    /// # Errors
    ///
    /// See [`Self::enqueue_in_tx`].
    pub async fn enqueue_chat_cleanup(
        &self,
        tx: &(impl DBRunner + Sync),
        payload: &ChatCleanupPayload,
    ) -> Result<Wake, EnqueueError> {
        self.enqueue_in_tx(tx, MiniChatQueue::ChatCleanup, payload.chat_id, payload)
            .await
    }

    /// Enqueues an attachment-cleanup message (partition: tenant).
    ///
    /// # Errors
    ///
    /// See [`Self::enqueue_in_tx`].
    pub async fn enqueue_attachment_cleanup(
        &self,
        tx: &(impl DBRunner + Sync),
        payload: &AttachmentCleanupPayload,
    ) -> Result<Wake, EnqueueError> {
        self.enqueue_in_tx(
            tx,
            MiniChatQueue::AttachmentCleanup,
            payload.tenant_id,
            payload,
        )
        .await
    }

    /// Enqueues a thread-summary message (partition: chat).
    ///
    /// # Errors
    ///
    /// See [`Self::enqueue_in_tx`].
    pub async fn enqueue_thread_summary(
        &self,
        tx: &(impl DBRunner + Sync),
        payload: &ThreadSummaryPayload,
    ) -> Result<Wake, EnqueueError> {
        self.enqueue_in_tx(tx, MiniChatQueue::ThreadSummary, payload.chat_id, payload)
            .await
    }
}

#[cfg(test)]
#[path = "outbox_tests.rs"]
mod tests;
