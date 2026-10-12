//! Outbox adapters: the enqueue side of the chat-cleanup, attachment-cleanup,
//! usage, audit and thread-summary queues and the handlers of all five.
//!
//! Every enqueue runs inside the caller's transaction and returns the `Wake`
//! the caller fires after the commit. The handlers deliver one message to a
//! plugin through its gateway and map the outcome to `Ok` / `Retry` /
//! `Reject` (DESIGN 5.6, 3.2 "Audit plugin and audit outbox", B.9.3).

use std::sync::Arc;

use async_trait::async_trait;
use mini_chat_sdk::{AuditEvent, UsageEvent};
use serde_json::json;
use time::format_description::well_known::Rfc3339;
use toolkit_db::outbox::{
    LeaseConfig, LeasedMessageHandler, MessageResult, Outbox, OutboxMessage, Record, Wake,
};
use toolkit_db::{DBProvider, DbTx};
use uuid::Uuid;

use crate::config::MiniChatConfig;
use crate::domain::attachments::AttachmentCleanupEvent;
use crate::domain::chats::ChatCleanupEvent;
use crate::domain::cleanup::{CleanupOutcome, CleanupService};
use crate::domain::error::DomainError;
use crate::domain::ports::{
    AttachmentCleanupPort, AuditPort, ChatCleanupPort, LlmGateway, ModelPolicyPort, StoragePort,
    ThreadSummaryPort, UsagePort,
};
use crate::domain::thread_summary::{SummaryOutcome, ThreadSummaryService, ThreadSummaryTask};
use crate::infra::audit::{AuditDeliveryOutcome, AuditGateway};
use crate::infra::model_policy::{ModelPolicyGateway, UsageDeliveryOutcome};

/// Payload type of the usage message (the SDK `UsageEvent` JSON).
pub const USAGE_PAYLOAD_TYPE: &str = "application/vnd.mini-chat.usage-snapshot+json";

/// Payload type of the audit message (the SDK `AuditEvent` JSON, tagged by
/// `event_type`).
pub const AUDIT_PAYLOAD_TYPE: &str = "application/vnd.mini-chat.audit+json";

/// The audit handler dead-letters a retry on this attempt (the 120th,
/// `attempts` being zero on the first), so a misconfigured plugin cannot block
/// its partition for ever.
pub const AUDIT_MAX_ATTEMPTS: u32 = 120;

/// Payload type of the chat-cleanup message.
pub const CHAT_CLEANUP_PAYLOAD_TYPE: &str = "application/vnd.mini-chat.chat-cleanup+json";

/// Payload type of the attachment-cleanup message (the
/// [`AttachmentCleanupEvent`] JSON).
pub const ATTACHMENT_CLEANUP_PAYLOAD_TYPE: &str =
    "application/vnd.mini-chat.attachment-cleanup+json";

/// Payload type of the thread-summary message (the work item JSON, DESIGN 3.6
/// "Durable scheduling").
pub const THREAD_SUMMARY_PAYLOAD_TYPE: &str = "application/vnd.mini-chat.thread-summary+json";

/// Headroom of the thread-summary lease: the time reserved for the ack
/// transaction after the handler was cancelled at `duration - headroom`.
const THREAD_SUMMARY_HEADROOM: std::time::Duration = std::time::Duration::from_secs(2);

/// Reason carried by the chat-cleanup payload of a soft-deleted chat.
const REASON_CHAT_SOFT_DELETE: &str = "chat_soft_delete";

/// Enqueues chat-cleanup messages on the configured queue, inside the
/// caller's transaction (DESIGN 3.6 "Cleanup on Chat Deletion").
///
/// The queue is partitioned by `chat_id`, so every message of a chat lands in
/// the same partition and is processed sequentially.
pub struct ChatCleanupOutbox {
    outbox: Arc<Outbox>,
    queue_name: String,
    num_partitions: u16,
}

impl ChatCleanupOutbox {
    /// Creates the adapter for `queue_name` with `num_partitions` partitions
    /// (the queue must be registered with the same count).
    #[must_use]
    pub fn new(outbox: Arc<Outbox>, queue_name: impl Into<String>, num_partitions: u16) -> Self {
        Self {
            outbox,
            queue_name: queue_name.into(),
            num_partitions: num_partitions.max(1),
        }
    }
}

/// Stable partition of an id (a chat for the cleanup queue, a tenant for the
/// usage and audit queues): its low 32 bits modulo the count.
fn partition_for(id: Uuid, num_partitions: u16) -> u32 {
    let low = u32::try_from(id.as_u128() & u128::from(u32::MAX)).unwrap_or(0);
    low % u32::from(num_partitions.max(1))
}

#[async_trait]
impl ChatCleanupPort for ChatCleanupOutbox {
    async fn enqueue_chat_cleanup(
        &self,
        tx: &DbTx<'_>,
        event: &ChatCleanupEvent,
    ) -> Result<Wake, DomainError> {
        let deleted_at = event
            .chat_deleted_at
            .format(&Rfc3339)
            .map_err(|e| DomainError::internal(format!("chat_deleted_at format: {e}")))?;
        let payload = serde_json::to_vec(&json!({
            "tenant_id": event.tenant_id,
            "chat_id": event.chat_id,
            // Stable identity of the message, reused unchanged across retries.
            "system_request_id": Uuid::new_v4(),
            "reason": REASON_CHAT_SOFT_DELETE,
            "chat_deleted_at": deleted_at,
        }))
        .map_err(|e| DomainError::internal(format!("chat cleanup payload: {e}")))?;

        let record = Record::to(
            &self.queue_name,
            partition_for(event.chat_id, self.num_partitions),
        )
        .payload(payload, CHAT_CLEANUP_PAYLOAD_TYPE)
        .build()?;
        Ok(self.outbox.enqueue(tx, record).await?)
    }
}

/// Enqueues attachment-cleanup messages on the configured queue, inside the
/// caller's transaction (DESIGN "Attachment Deletion"). Partitioned by
/// `tenant_id`.
pub struct AttachmentCleanupOutbox {
    outbox: Arc<Outbox>,
    queue_name: String,
    num_partitions: u16,
}

impl AttachmentCleanupOutbox {
    /// Creates the adapter for `queue_name` with `num_partitions` partitions
    /// (the queue must be registered with the same count).
    #[must_use]
    pub fn new(outbox: Arc<Outbox>, queue_name: impl Into<String>, num_partitions: u16) -> Self {
        Self {
            outbox,
            queue_name: queue_name.into(),
            num_partitions: num_partitions.max(1),
        }
    }
}

#[async_trait]
impl AttachmentCleanupPort for AttachmentCleanupOutbox {
    async fn enqueue_attachment_cleanup(
        &self,
        tx: &DbTx<'_>,
        event: &AttachmentCleanupEvent,
    ) -> Result<Wake, DomainError> {
        let payload = serde_json::to_vec(event)
            .map_err(|e| DomainError::internal(format!("attachment cleanup payload: {e}")))?;
        let record = Record::to(
            &self.queue_name,
            partition_for(event.tenant_id, self.num_partitions),
        )
        .payload(payload, ATTACHMENT_CLEANUP_PAYLOAD_TYPE)
        .build()
        .map_err(attachment_outbox_error)?;
        self.outbox
            .enqueue(tx, record)
            .await
            .map_err(attachment_outbox_error)
    }
}

/// An attachment write has no client-visible outbox failure (only the chat
/// delete reports an oversized payload): every failure is an opaque internal
/// error and the transaction rolls back.
fn attachment_outbox_error(error: toolkit_db::outbox::OutboxError) -> DomainError {
    match DomainError::from(error) {
        DomainError::OutboxPayloadTooLarge { message } => DomainError::internal(message),
        other => other,
    }
}

/// Enqueues usage messages on the configured queue, inside the finalization
/// transaction (DESIGN 5.6). Partitioned by `tenant_id`.
pub struct UsageOutbox {
    outbox: Arc<Outbox>,
    queue_name: String,
    num_partitions: u16,
}

impl UsageOutbox {
    /// Creates the adapter for `queue_name` with `num_partitions` partitions
    /// (the queue must be registered with the same count).
    #[must_use]
    pub fn new(outbox: Arc<Outbox>, queue_name: impl Into<String>, num_partitions: u16) -> Self {
        Self {
            outbox,
            queue_name: queue_name.into(),
            num_partitions: num_partitions.max(1),
        }
    }
}

#[async_trait]
impl UsagePort for UsageOutbox {
    async fn enqueue_usage(&self, tx: &DbTx<'_>, event: &UsageEvent) -> Result<Wake, DomainError> {
        let payload = serde_json::to_vec(event)
            .map_err(|e| DomainError::internal(format!("usage payload: {e}")))?;
        let record = Record::to(
            &self.queue_name,
            partition_for(event.tenant_id, self.num_partitions),
        )
        .payload(payload, USAGE_PAYLOAD_TYPE)
        .build()?;
        Ok(self.outbox.enqueue(tx, record).await?)
    }
}

/// Enqueues audit messages on the configured queue, inside the finalization
/// (or mutation) transaction. Partitioned by `tenant_id`.
pub struct AuditOutbox {
    outbox: Arc<Outbox>,
    queue_name: String,
    num_partitions: u16,
}

impl AuditOutbox {
    /// Creates the adapter for `queue_name` with `num_partitions` partitions
    /// (the queue must be registered with the same count).
    #[must_use]
    pub fn new(outbox: Arc<Outbox>, queue_name: impl Into<String>, num_partitions: u16) -> Self {
        Self {
            outbox,
            queue_name: queue_name.into(),
            num_partitions: num_partitions.max(1),
        }
    }
}

/// The tenant an audit event belongs to (the partition key).
fn audit_tenant(event: &AuditEvent) -> Uuid {
    match event {
        AuditEvent::TurnCompleted(e) | AuditEvent::TurnFailed(e) => e.tenant_id,
        AuditEvent::TurnRetry { tenant_id, .. }
        | AuditEvent::TurnEdit { tenant_id, .. }
        | AuditEvent::TurnDelete { tenant_id, .. } => *tenant_id,
    }
}

#[async_trait]
impl AuditPort for AuditOutbox {
    async fn enqueue_audit(&self, tx: &DbTx<'_>, event: &AuditEvent) -> Result<Wake, DomainError> {
        let payload = serde_json::to_vec(event)
            .map_err(|e| DomainError::internal(format!("audit payload: {e}")))?;
        let record = Record::to(
            &self.queue_name,
            partition_for(audit_tenant(event), self.num_partitions),
        )
        .payload(payload, AUDIT_PAYLOAD_TYPE)
        .build()?;
        Ok(self.outbox.enqueue(tx, record).await?)
    }
}

/// Enqueues thread-summary work items on the configured queue, inside the
/// finalization transaction of the causing turn (DESIGN 3.6 "Durable
/// scheduling"). Partitioned by `chat_id`, so every work item of a chat is
/// processed sequentially.
pub struct ThreadSummaryOutbox {
    outbox: Arc<Outbox>,
    queue_name: String,
    num_partitions: u16,
}

impl ThreadSummaryOutbox {
    /// Creates the adapter for `queue_name` with `num_partitions` partitions
    /// (the queue must be registered with the same count).
    #[must_use]
    pub fn new(outbox: Arc<Outbox>, queue_name: impl Into<String>, num_partitions: u16) -> Self {
        Self {
            outbox,
            queue_name: queue_name.into(),
            num_partitions: num_partitions.max(1),
        }
    }
}

#[async_trait]
impl ThreadSummaryPort for ThreadSummaryOutbox {
    async fn enqueue_thread_summary(
        &self,
        tx: &DbTx<'_>,
        task: &ThreadSummaryTask,
    ) -> Result<Wake, DomainError> {
        let record = Record::to(
            &self.queue_name,
            partition_for(task.chat_id, self.num_partitions),
        )
        .payload(task.to_payload()?, THREAD_SUMMARY_PAYLOAD_TYPE)
        .build()?;
        Ok(self.outbox.enqueue(tx, record).await?)
    }
}

/// Lease of the thread-summary queue (DESIGN B.9.3):
/// `thread_summary_worker.claim_timeout_secs`, so the non-streaming provider
/// call is not cancelled and redelivered mid-flight.
#[must_use]
pub fn thread_summary_lease(config: &MiniChatConfig) -> LeaseConfig {
    let duration = std::time::Duration::from_secs(config.thread_summary_worker.claim_timeout_secs);
    LeaseConfig {
        duration,
        headroom: THREAD_SUMMARY_HEADROOM.min(duration / 2),
    }
}

/// Delivers usage messages to the model-policy plugin: a payload that is not a
/// `UsageEvent` and a permanent plugin failure are rejected (dead-lettered), a
/// transient failure (including a plugin that cannot be resolved) is retried.
pub struct UsageOutboxHandler {
    policy: Arc<ModelPolicyGateway>,
}

impl UsageOutboxHandler {
    /// Creates the handler over the model-policy gateway.
    #[must_use]
    pub fn new(policy: Arc<ModelPolicyGateway>) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl LeasedMessageHandler for UsageOutboxHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let event: UsageEvent = match serde_json::from_slice(&msg.payload) {
            Ok(event) => event,
            Err(error) => {
                tracing::error!(%error, seq = msg.seq, "usage outbox: corrupt payload rejected");
                return MessageResult::Reject(format!("payload is not a usage event: {error}"));
            }
        };
        match self.policy.publish_usage(event).await {
            UsageDeliveryOutcome::Delivered => MessageResult::Ok,
            UsageDeliveryOutcome::Retry => MessageResult::Retry,
            UsageDeliveryOutcome::Reject(reason) => MessageResult::Reject(reason),
        }
    }
}

/// Delivers audit messages to the audit plugin. The payload is deserialized
/// first (a corrupt one is rejected whether or not a plugin resolves); a
/// delivered or dropped event is acknowledged; a retry is bounded at
/// [`AUDIT_MAX_ATTEMPTS`].
pub struct AuditOutboxHandler {
    audit: Arc<AuditGateway>,
}

impl AuditOutboxHandler {
    /// Creates the handler over the audit gateway.
    #[must_use]
    pub fn new(audit: Arc<AuditGateway>) -> Self {
        Self { audit }
    }
}

#[async_trait]
impl LeasedMessageHandler for AuditOutboxHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let event: AuditEvent = match serde_json::from_slice(&msg.payload) {
            Ok(event) => event,
            Err(error) => {
                tracing::error!(%error, seq = msg.seq, "audit outbox: corrupt payload rejected");
                return MessageResult::Reject(format!("payload is not an audit event: {error}"));
            }
        };
        match self.audit.deliver(&event).await {
            AuditDeliveryOutcome::Delivered | AuditDeliveryOutcome::Dropped => MessageResult::Ok,
            AuditDeliveryOutcome::Reject => {
                MessageResult::Reject("audit plugin rejected the event".to_owned())
            }
            AuditDeliveryOutcome::Retry => {
                if i64::from(msg.attempts) >= i64::from(AUDIT_MAX_ATTEMPTS) - 1 {
                    tracing::error!(
                        attempts = msg.attempts,
                        event_type = event.event_type(),
                        "audit outbox: retries exhausted, event dead-lettered"
                    );
                    MessageResult::Reject("audit delivery retries exhausted".to_owned())
                } else {
                    MessageResult::Retry
                }
            }
        }
    }
}

/// Maps the outcome of the cleanup logic to the shared outbox's result.
fn message_result(outcome: CleanupOutcome) -> MessageResult {
    match outcome {
        CleanupOutcome::Done => MessageResult::Ok,
        CleanupOutcome::Retry => MessageResult::Retry,
        CleanupOutcome::Reject(reason) => MessageResult::Reject(reason),
    }
}

/// Handler of the attachment-cleanup queue (`outbox.cleanup_queue_name`): deletes
/// the provider file of a deleted, abandoned or failed attachment.
pub struct AttachmentCleanupHandler {
    cleanup: CleanupService,
}

impl AttachmentCleanupHandler {
    /// Creates the handler over the database and the provider storage.
    #[must_use]
    pub fn new<S: StoragePort + 'static>(
        db: Arc<DBProvider<DomainError>>,
        storage: Arc<S>,
        config: Arc<MiniChatConfig>,
    ) -> Self {
        Self {
            cleanup: CleanupService::new(db, storage, config),
        }
    }
}

#[async_trait]
impl LeasedMessageHandler for AttachmentCleanupHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        message_result(self.cleanup.attachment(&msg.payload).await)
    }
}

/// Handler of the chat-cleanup queue (`outbox.chat_cleanup_queue_name`): deletes
/// the provider files and the vector store of a soft-deleted chat.
pub struct ChatCleanupHandler {
    cleanup: CleanupService,
}

impl ChatCleanupHandler {
    /// Creates the handler over the database and the provider storage.
    #[must_use]
    pub fn new<S: StoragePort + 'static>(
        db: Arc<DBProvider<DomainError>>,
        storage: Arc<S>,
        config: Arc<MiniChatConfig>,
    ) -> Self {
        Self {
            cleanup: CleanupService::new(db, storage, config),
        }
    }
}

#[async_trait]
impl LeasedMessageHandler for ChatCleanupHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let attempts = u32::try_from(msg.attempts).unwrap_or_default();
        message_result(self.cleanup.chat(&msg.payload, attempts).await)
    }
}

/// Handler of the thread-summary queue (`outbox.thread_summary_queue_name`):
/// generates, stores and commits one summary. Clones share one state: the gear
/// registers one clone with the outbox and keeps another to bind the usage port
/// once the pipeline runs.
#[derive(Clone)]
pub struct ThreadSummaryHandler {
    service: Arc<ThreadSummaryService>,
}

impl ThreadSummaryHandler {
    /// Creates the handler over the database, the model-policy source and the
    /// provider layer. Until [`Self::bind_usage`] is called every delivery is
    /// retried.
    #[must_use]
    pub fn new<P: ModelPolicyPort + 'static, L: LlmGateway + 'static>(
        db: Arc<DBProvider<DomainError>>,
        policy: Arc<P>,
        llm: Arc<L>,
        config: Arc<MiniChatConfig>,
    ) -> Self {
        Self {
            service: Arc::new(ThreadSummaryService::new(db, policy, llm, config)),
        }
    }

    /// Binds the usage port the commit transaction enqueues the system usage
    /// event through (the usage outbox only exists once the pipeline runs).
    pub fn bind_usage<U: UsagePort + 'static>(&self, usage: Arc<U>) {
        self.service.bind_usage(usage);
    }
}

#[async_trait]
impl LeasedMessageHandler for ThreadSummaryHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let attempts = u32::try_from(msg.attempts).unwrap_or_default();
        match self.service.handle(&msg.payload, attempts).await {
            SummaryOutcome::Done => MessageResult::Ok,
            SummaryOutcome::Retry => MessageResult::Retry,
            SummaryOutcome::Reject(reason) => MessageResult::Reject(reason),
        }
    }
}

#[cfg(test)]
#[path = "outbox_tests.rs"]
mod tests;
