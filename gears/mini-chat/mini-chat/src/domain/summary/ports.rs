//! The repository port of the thread-summary handler. Background work runs outside any user
//! scope: every method carries the tenant of the work item.

use async_trait::async_trait;
use uuid::Uuid;

use super::model::{CommitOutcome, CommitSummary, Frontier, RangeMessage, StoredSummary};
use crate::domain::error::DomainError;

/// Persistence of thread summaries.
#[async_trait]
pub trait SummaryRepository: Send + Sync {
    /// The chat's stored summary.
    async fn current(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Option<StoredSummary>, DomainError>;

    /// Whether the message exists in the chat and is not soft-deleted.
    async fn message_alive(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> Result<bool, DomainError>;

    /// The non-deleted, non-compressed user and assistant messages with
    /// `(created_at, id)` in `(base, target]`, ordered by `(created_at, id)`.
    async fn load_range(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        base: Option<Frontier>,
        target: Frontier,
    ) -> Result<Vec<RangeMessage>, DomainError>;

    /// The atomic commit: lock and check the target message, compare-and-set the frontier, upsert
    /// the summary, mark the exact range compressed and enqueue the usage event.
    async fn commit(
        &self,
        tenant_id: Uuid,
        command: CommitSummary,
    ) -> Result<CommitOutcome, DomainError>;
}
