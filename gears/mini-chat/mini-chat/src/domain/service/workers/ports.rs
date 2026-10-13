//! Ports of the background workers; the infrastructure layer implements them.
//! Every method is one statement or one transaction of the feature; no
//! provider call happens inside a transaction.

use async_trait::async_trait;
use mini_chat_sdk::{ModelCatalogEntry, TurnAuditEvent, UsageTokens};
use time::OffsetDateTime;
use toolkit_macros::domain_model;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::orphan::OrphanCandidate;
use super::summary_prompt::RangeMessage;
use crate::domain::error::DomainError;
use crate::domain::service::attachments::model::{CleanupState, StoreRow};
use crate::domain::service::quota::finalize::{FinalizeRequest, FinalizeResult};
use crate::domain::service::stream::ports::{MessageBoundary, ProviderTarget};

// ---- chat cleanup ---------------------------------------------------------------------------------

/// The chat row the chat cleanup handler reads (soft-deleted rows included).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupChat {
    /// `chats.deleted_at` is set.
    pub deleted: bool,
    /// `chats.model` (the selected model).
    pub model: String,
}

/// A `pending` attachment of the deleted chat.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAttachment {
    /// Attachment id.
    pub id: Uuid,
    /// Provider file id; `None` when the upload never reached the provider.
    pub provider_file_id: Option<String>,
    /// Storage backend label.
    pub storage_backend: String,
    /// `secondary_status` is `uploaded`.
    pub secondary_uploaded: bool,
    /// Provider id of the secondary (Anthropic) copy.
    pub secondary_file_id: Option<String>,
}

/// The cleanup states of the chat's attachments.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CleanupSummary {
    /// An attachment is still `pending`.
    pub any_pending: bool,
    /// An attachment is `failed`.
    pub any_failed: bool,
}

/// Storage of the chat cleanup handler. Every statement is bound to the
/// `tenant_id` and `chat_id` of the payload.
#[async_trait]
pub trait ChatCleanupStore: Send + Sync {
    /// The chat of the tenant, soft-deleted rows included.
    async fn chat(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Option<CleanupChat>, DomainError>;

    /// Sets `cleanup_status` `pending` on the attachments of the chat that have
    /// none (soft-deleted rows included); the number of rows claimed.
    async fn claim_stragglers(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<u64, DomainError>;

    /// The `pending` attachments of the chat, soft-deleted rows included,
    /// ordered by `created_at` and then `id`.
    async fn pending_attachments(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Vec<PendingAttachment>, DomainError>;

    /// `cleanup_attempts + 1`, `last_cleanup_error` and `cleanup_updated_at`
    /// where the row is `pending`; the new attempt count, `None` when the row is
    /// no longer pending.
    async fn record_attempt(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        id: Uuid,
        error: &str,
        now: OffsetDateTime,
    ) -> Result<Option<i32>, DomainError>;

    /// `cleanup_status = state` where the row is `pending`.
    async fn finish(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        id: Uuid,
        state: CleanupState,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError>;

    /// Whether any attachment of the chat is `pending` and whether any is
    /// `failed` (soft-deleted rows included).
    async fn summary(&self, tenant_id: Uuid, chat_id: Uuid) -> Result<CleanupSummary, DomainError>;

    /// The chat's `chat_vector_stores` row, selected by `tenant_id` and
    /// `chat_id`.
    async fn vector_store(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Option<StoreRow>, DomainError>;

    /// DELETE of the row where `id`, `tenant_id` and `chat_id` match; with
    /// `only_placeholder` also where `vector_store_id IS NULL`.
    async fn delete_store_row(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        row_id: Uuid,
        only_placeholder: bool,
    ) -> Result<bool, DomainError>;
}

// ---- orphan watchdog ------------------------------------------------------------------------------

/// Storage of the orphan watchdog.
#[async_trait]
pub trait OrphanStore: Send + Sync {
    /// At most `limit` `running`, non-deleted turns across tenants whose
    /// `COALESCE(last_progress_at, started_at)` is at or before `cutoff`,
    /// ordered by `last_progress_at` ascending with NULL first, then
    /// `started_at`, then `id`.
    async fn scan(
        &self,
        cutoff: OffsetDateTime,
        limit: u32,
    ) -> Result<Vec<OrphanCandidate>, DomainError>;

    /// The finalization transaction of the orphan variant: the CAS, the
    /// settlement and the usage event of the quota helper, then, when a usage
    /// event was enqueued and `audit` is given, the audit event in the same
    /// transaction. The queues are woken after the commit.
    async fn finalize(
        &self,
        request: &FinalizeRequest,
        audit: Option<&TurnAuditEvent>,
    ) -> Result<FinalizeResult, DomainError>;
}

// ---- thread summary -------------------------------------------------------------------------------

/// The stored summary of a chat.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredSummary {
    /// `summary_text`.
    pub text: String,
    /// The frontier.
    pub frontier: MessageBoundary,
}

/// What the commit writes.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct SummaryCommit {
    /// Tenant of the chat.
    pub tenant_id: Uuid,
    /// The chat.
    pub chat_id: Uuid,
    /// The base frontier, none for a first summary.
    pub base: Option<MessageBoundary>,
    /// The frozen target frontier.
    pub target: MessageBoundary,
    /// The summary text.
    pub summary_text: String,
    /// `token_estimate`.
    pub token_estimate: i32,
    /// Id of the new row (a first summary).
    pub summary_id: Uuid,
    /// The system task id of the work item.
    pub system_request_id: Uuid,
    /// The summary model id.
    pub summary_model: String,
    /// Provider usage.
    pub usage: Option<UsageTokens>,
    /// `now`.
    pub now: OffsetDateTime,
}

/// The result of the commit transaction.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitOutcome {
    /// Summary, frontier, `is_compressed` flags and usage event committed.
    Committed,
    /// The frozen target is gone or soft-deleted.
    FrontierDeleted,
    /// The summary the task was based on is gone.
    BaseMissing,
    /// Another commit moved the frontier.
    CasConflict,
}

/// Storage of the thread-summary handler.
#[async_trait]
pub trait SummaryStore: Send + Sync {
    /// `Some(deleted)` for a chat of the tenant, soft-deleted chats included.
    async fn chat_deleted(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Option<bool>, DomainError>;

    /// The summary of the chat.
    async fn summary(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Option<StoredSummary>, DomainError>;

    /// `Some(deleted)` for the message of the chat, `None` when it does not
    /// exist.
    async fn message_deleted(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> Result<Option<bool>, DomainError>;

    /// The live, non-compressed messages in `(base, target]` ordered by
    /// `created_at` and then `id`; messages after the target are never read.
    async fn range(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        base: Option<MessageBoundary>,
        target: MessageBoundary,
    ) -> Result<Vec<RangeMessage>, DomainError>;

    /// The write-first commit transaction
    /// (`cpt-cf-mini-chat-algo-background-workers-commit-summary`).
    async fn commit(&self, commit: &SummaryCommit) -> Result<CommitOutcome, DomainError>;
}

/// A non-streaming summary request.
#[domain_model]
#[derive(Debug, Clone)]
pub struct SummaryRequest {
    /// The provider entry of the summary model.
    pub target: ProviderTarget,
    /// The catalog entry of the summary model.
    pub entry: ModelCatalogEntry,
    /// The system prompt.
    pub system: String,
    /// The user prompt.
    pub user: String,
    /// Tenant of the chat (the `user` value).
    pub tenant_id: Uuid,
    /// The chat (the metadata).
    pub chat_id: Uuid,
}

/// The model answer.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct SummaryAnswer {
    /// The output text (still with the `<analysis>` and `<summary>` blocks).
    pub text: String,
    /// Provider usage (a missing field is 0).
    pub usage: UsageTokens,
}

/// A failed summary call.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummaryCallError {
    /// The provider says the context is too long.
    ContextLength,
    /// Any other failure; the sanitized cause is for the log.
    Failed(String),
}

/// The non-streaming summary call through OAGW.
#[async_trait]
pub trait SummaryModelClient: Send + Sync {
    /// Sends `request` with the S2S context `ctx`.
    async fn summarize(
        &self,
        ctx: &SecurityContext,
        request: &SummaryRequest,
    ) -> Result<SummaryAnswer, SummaryCallError>;
}
