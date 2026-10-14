//! The persistence port of the thread-summary handler: the frontier pre-check, the range
//! selection and the compare-and-set commit (`infra::storage::summary_store`). Every method
//! filters on `tenant_id` and `chat_id` of the work item: the handler runs outside a user scope.

use std::sync::Arc;

use async_trait::async_trait;
use mini_chat_sdk::UsageEvent;
use time::OffsetDateTime;
use toolkit_db::outbox::Wake;
use toolkit_macros::domain_model;

use crate::domain::error::DomainError;
use crate::domain::maintenance::summary_prompt::RangeMessage;
use crate::domain::quota::UsageQueue;
use crate::domain::streaming::summary::ThreadSummaryWork;

/// The result of the frontier pre-check.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreCheck {
    /// Go on; `base_summary` is the stored text of the base (`None` for an empty base).
    Ready { base_summary: Option<String> },
    /// The chat is missing or soft-deleted.
    ChatGone,
    /// The work item has a base and no `thread_summaries` row exists.
    BaseMissing,
    /// The stored frontier differs from the base (an empty base with a row included).
    FrontierMoved,
    /// The target message is missing or soft-deleted.
    TargetDeleted,
}

/// What a commit writes.
#[domain_model]
#[derive(Debug, Clone)]
pub struct SummaryCommit {
    pub work: ThreadSummaryWork,
    pub summary_text: String,
    pub token_estimate: i32,
    /// The system-task usage event, enqueued in the commit transaction.
    pub usage_event: UsageEvent,
    pub now: OffsetDateTime,
}

/// The result of the commit.
#[domain_model]
#[derive(Debug)]
pub enum CommitOutcome {
    /// Committed; fire the wake after the commit.
    Committed(Wake),
    /// The target message is missing or soft-deleted: nothing was written.
    TargetDeleted,
    /// The work item has a base and the chat has no `thread_summaries` row.
    BaseMissing,
    /// Another commit already advanced the frontier (or won the first insert).
    FrontierMoved,
}

/// The persistence of the thread-summary handler.
#[async_trait]
pub trait SummaryStore: Send + Sync {
    /// The frontier pre-check.
    ///
    /// # Errors
    ///
    /// `Internal` on a database failure.
    async fn precheck(&self, work: &ThreadSummaryWork) -> Result<PreCheck, DomainError>;

    /// The messages of the frozen range in the order `(created_at, id)`: the chat's
    /// non-deleted, non-compressed messages after the base frontier (when not empty) and at or
    /// before the frozen target.
    ///
    /// # Errors
    ///
    /// `Internal` on a database failure.
    async fn load_range(&self, work: &ThreadSummaryWork) -> Result<Vec<RangeMessage>, DomainError>;

    /// The CAS commit in one transaction: lock and re-check the target, insert the first row or
    /// advance the row whose frontier equals the base, mark the range `is_compressed` and
    /// enqueue `commit.usage_event` through `usage`.
    ///
    /// # Errors
    ///
    /// `Internal` on a database or enqueue failure; nothing of the attempt remains.
    async fn commit(
        &self,
        commit: &SummaryCommit,
        usage: Arc<dyn UsageQueue>,
    ) -> Result<CommitOutcome, DomainError>;
}
