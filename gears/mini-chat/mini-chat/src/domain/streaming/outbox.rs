//! The turn outbox port (slice S2-provider-path of `cpt-cf-mini-chat-feature-message-streaming`):
//! the transactional enqueue of the turn audit event and of the thread-summary work, written in
//! the caller's finalization transaction; the returned wakes are fired after the commit. The
//! infrastructure layer implements it over the shared outbox.

use async_trait::async_trait;
use mini_chat_sdk::AuditEvent;
use toolkit_db::outbox::Wake;
use toolkit_db::secure::DbTx;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::streaming::summary::ThreadSummaryWork;

/// The audit and thread-summary queues of the finalization transactions.
#[async_trait]
pub trait TurnOutbox: Send + Sync {
    /// Enqueue the audit event to `outbox.audit_queue_name`, partitioned by `tenant_id`.
    ///
    /// # Errors
    ///
    /// `OutboxPayloadTooLarge` for an oversized payload, `Internal` when the outbox is not
    /// running or the database fails: the caller's transaction must roll back.
    async fn enqueue_turn_audit(
        &self,
        tx: &DbTx<'_>,
        event: &AuditEvent,
    ) -> Result<Wake, DomainError>;

    /// Enqueue the audit event of a turn mutation (`turn_retry`, `turn_edit`, `turn_delete`) to
    /// `outbox.audit_queue_name`, partitioned by the `tenant_id` of the chat
    /// (`cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-audit-event`,
    /// `inst-ae-enqueue`): the mutation events carry no tenant, so the caller passes it.
    ///
    /// # Errors
    ///
    /// `OutboxPayloadTooLarge` for an oversized payload (500, not the 400 of chat deletion),
    /// `Internal` when the outbox is not running or the database fails: the caller's
    /// transaction must roll back.
    async fn enqueue_mutation_audit(
        &self,
        tx: &DbTx<'_>,
        tenant_id: Uuid,
        event: &AuditEvent,
    ) -> Result<Wake, DomainError>;

    /// Enqueue the work item to `outbox.thread_summary_queue_name`, partitioned by `chat_id`.
    ///
    /// # Errors
    ///
    /// As [`enqueue_turn_audit`](Self::enqueue_turn_audit).
    async fn enqueue_thread_summary(
        &self,
        tx: &DbTx<'_>,
        work: &ThreadSummaryWork,
    ) -> Result<Wake, DomainError>;
}
