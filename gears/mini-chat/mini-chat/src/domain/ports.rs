//! Ports: abstractions the domain services depend on; the infra layer
//! implements them.
//!
//! The persistence ports are implemented for every secure database runner
//! (`DbConn` and `DbTx`), so a service calls `conn.find_live_chat(..)` or
//! `tx.soft_delete_chat(..)` and the same code serves a plain read and a
//! transaction. Every method takes the [`AccessScope`] the caller is allowed
//! to use; the implementation applies it to the query (Secure ORM).

use std::collections::HashMap;

use async_trait::async_trait;
use bytes::Bytes;
use mini_chat_sdk::{AuditEvent, PolicySnapshot, UsageEvent, UserLimits};
use time::OffsetDateTime;
use toolkit_db::DbTx;
use toolkit_db::outbox::Wake;
use toolkit_odata::{ODataQuery, Page};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::domain::attachments::{
    AttachmentCleanupEvent, AttachmentRow, AttachmentUsage, NewAttachment, NewThumbnail,
    VectorStoreRow,
};
use crate::domain::chats::{ChatCleanupEvent, ChatRow, NewChat};
use crate::domain::error::DomainError;
use crate::domain::messages::{
    AttachmentSummaryRow, MessageRow, NewMessage, ReadyAttachment, SummaryFrontier,
};
use crate::domain::provider::{ChatRequest, ProviderEventStream, ProviderFailure};
use crate::domain::quota::{Bucket, PeriodStarts, QuotaRow};
use crate::domain::reactions::{NewReaction, ReactionRow};
use crate::domain::thread_summary::{NewSummary, SummaryRow, ThreadSummaryTask};
use crate::domain::turns::{
    NewTurn, NewUnstartedTurn, OrphanCandidate, ToolCounts, ToolKind, TurnFinalization,
    TurnPreflightColumns, TurnRow,
};

/// Source of the policy snapshot and the per-user limits (the model-policy
/// plugin behind its gateway).
#[async_trait]
pub trait ModelPolicyPort: Send + Sync {
    /// Current policy snapshot for the user; never cached.
    ///
    /// # Errors
    /// `Internal` when the policy source cannot be resolved or fails.
    async fn current_snapshot(&self, user_id: Uuid) -> Result<PolicySnapshot, DomainError>;

    /// The current policy version for the user, without the snapshot.
    ///
    /// # Errors
    /// `Internal` when the policy source cannot be resolved or fails.
    async fn current_policy_version(&self, user_id: Uuid) -> Result<u64, DomainError>;

    /// The immutable snapshot of one policy version (the settlement of a turn
    /// reads the version it was planned under, never the current one).
    ///
    /// # Errors
    /// `Internal` when the policy source cannot be resolved, fails or does not
    /// know the version.
    async fn snapshot_at(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, DomainError>;

    /// Per-user limits under a policy version.
    ///
    /// # Errors
    /// `Internal` when the policy source cannot be resolved or fails.
    async fn user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, DomainError>;
}

/// Durable hand-off of the chat-cleanup work of a soft-deleted chat.
///
/// The enqueue runs inside the caller's transaction, so the soft delete and
/// the cleanup message commit or roll back together. The returned [`Wake`]
/// must be fired after the commit (the caller uses
/// `toolkit_db::outbox::in_transaction`).
#[async_trait]
pub trait ChatCleanupPort: Send + Sync + 'static {
    /// Enqueues one chat-cleanup message.
    ///
    /// # Errors
    /// `OutboxPayloadTooLarge` when the payload exceeds the outbox limit,
    /// `Internal` for every other outbox failure.
    async fn enqueue_chat_cleanup(
        &self,
        tx: &DbTx<'_>,
        event: &ChatCleanupEvent,
    ) -> Result<Wake, DomainError>;
}

/// Persistence port of the `chats` aggregate.
///
/// Chat reads and writes take the owner-narrowed chat scope; the child tables
/// (`messages`, `attachments`) take a tenant scope derived from it after the
/// owner-scoped chat lookup succeeded.
#[async_trait]
pub trait ChatRepository: Send + Sync {
    /// A live (not soft-deleted) chat visible in `scope`.
    async fn find_live_chat(
        &self,
        scope: &AccessScope,
        id: Uuid,
    ) -> Result<Option<ChatRow>, DomainError>;

    /// Live chats visible in `scope`, filtered / ordered / paginated by `query`.
    async fn list_live_chats(
        &self,
        scope: &AccessScope,
        query: &ODataQuery,
    ) -> Result<Page<ChatRow>, DomainError>;

    /// Number of non-soft-deleted messages per chat (`scope` is a tenant scope).
    async fn count_live_messages(
        &self,
        scope: &AccessScope,
        chat_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, i64>, DomainError>;

    /// Inserts a chat; the row must satisfy `scope`.
    async fn insert_chat(&self, scope: &AccessScope, chat: NewChat)
    -> Result<ChatRow, DomainError>;

    /// Sets `title` and `updated_at` of a live chat; `None` when no live chat
    /// is visible in `scope`.
    async fn update_chat_title(
        &self,
        scope: &AccessScope,
        id: Uuid,
        title: &str,
        now: OffsetDateTime,
    ) -> Result<Option<ChatRow>, DomainError>;

    /// Soft-deletes a live chat (`deleted_at`, `updated_at`) and returns the
    /// deleted row; `None` when no live chat is visible in `scope`.
    async fn soft_delete_chat(
        &self,
        scope: &AccessScope,
        id: Uuid,
        now: OffsetDateTime,
    ) -> Result<Option<ChatRow>, DomainError>;

    /// Bumps `updated_at` of a live chat (a message was sent); `false` when no
    /// live chat is visible in `scope`.
    async fn touch_chat(
        &self,
        scope: &AccessScope,
        id: Uuid,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError>;

    /// Takes the write lock of a live chat without changing it (`false` when
    /// no live chat is visible in `scope`). The first statement of the upload
    /// transaction: it serializes the per-chat limit checks and keeps the
    /// transaction write-first.
    async fn lock_live_chat(&self, scope: &AccessScope, id: Uuid) -> Result<bool, DomainError>;

    /// Whether the chat is soft-deleted; `None` when no such chat is visible in
    /// `scope`. Unlike [`Self::find_live_chat`] it sees deleted chats (the
    /// cleanup handlers need them; `scope` is a tenant scope).
    async fn chat_is_deleted(
        &self,
        scope: &AccessScope,
        id: Uuid,
    ) -> Result<Option<bool>, DomainError>;

    /// Sets `cleanup_status = 'pending'` on every attachment of the chat whose
    /// cleanup has not started (`scope` is a tenant scope). Returns the number
    /// of rows marked.
    async fn mark_attachments_for_cleanup(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<u64, DomainError>;
}

/// Persistence port of the message list (`scope` is a tenant scope).
#[async_trait]
pub trait MessageRepository: Send + Sync {
    /// Live messages of a chat, filtered / ordered / paginated by `query`.
    async fn list_live_messages(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        query: &ODataQuery,
    ) -> Result<Page<MessageRow>, DomainError>;

    /// A live message of the chat.
    async fn find_live_message(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        id: Uuid,
    ) -> Result<Option<MessageRow>, DomainError>;

    /// The newest `query.limit` messages of the chat that belong to a turn
    /// (`request_id IS NOT NULL`, and not the turn `query.excluded_request`),
    /// are not soft-deleted, not compressed and, when the chat has a summary,
    /// strictly after its frontier `(created_at, id)`, returned in
    /// chronological `(created_at, id)` order. `limit = 0` returns nothing.
    async fn recent_messages(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        query: &RecentQuery,
    ) -> Result<Vec<MessageRow>, DomainError>;

    /// `input_tokens + output_tokens` of the newest non-deleted assistant
    /// message of the chat with a non-zero count (`prior_context_tokens`).
    async fn latest_assistant_tokens(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<u64>, DomainError>;

    /// Inserts a message; the row must satisfy `scope`.
    async fn insert_message(
        &self,
        scope: &AccessScope,
        message: NewMessage,
    ) -> Result<(), DomainError>;

    /// Non-deleted attachments linked to the given messages, grouped by message.
    async fn attachment_summaries(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        message_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, Vec<AttachmentSummaryRow>>, DomainError>;
}

/// Persistence port of `message_reactions` (`scope` is tenant + owner).
#[async_trait]
pub trait ReactionRepository: Send + Sync {
    /// The reaction value of `user_id` per message.
    async fn reactions_of_user(
        &self,
        scope: &AccessScope,
        user_id: Uuid,
        message_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, String>, DomainError>;

    /// Inserts or replaces the reaction of `(message_id, user_id)`.
    async fn upsert_reaction(
        &self,
        scope: &AccessScope,
        reaction: NewReaction,
    ) -> Result<ReactionRow, DomainError>;

    /// Removes the reaction of `(message_id, user_id)`; absent is not an error.
    async fn delete_reaction(
        &self,
        scope: &AccessScope,
        user_id: Uuid,
        message_id: Uuid,
    ) -> Result<(), DomainError>;
}

/// Persistence port of `chat_turns` (`scope` is a tenant scope derived from
/// the owner-scoped chat lookup).
#[async_trait]
pub trait TurnRepository: Send + Sync {
    /// The turn `(chat_id, request_id)`, soft-deleted ones included (the key
    /// stays taken).
    async fn find_turn(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<TurnRow>, DomainError>;

    /// The turn `(chat_id, request_id)` unless it is soft-deleted.
    async fn find_live_turn(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<TurnRow>, DomainError>;

    /// The running, non-deleted turn of the chat, if any.
    async fn find_running_turn(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<TurnRow>, DomainError>;

    /// Inserts a `running` turn. A violated `(chat_id, request_id)` or
    /// one-running-turn-per-chat constraint is `DomainError::Conflict`.
    async fn insert_turn(&self, scope: &AccessScope, turn: NewTurn) -> Result<(), DomainError>;

    /// Refreshes `last_progress_at` / `updated_at` of a running turn.
    async fn touch_turn_progress(
        &self,
        scope: &AccessScope,
        turn_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<(), DomainError>;

    /// Counts one completed tool call on a running turn and refreshes its
    /// progress timestamps.
    async fn count_tool_done(
        &self,
        scope: &AccessScope,
        turn_id: Uuid,
        tool: ToolKind,
        now: OffsetDateTime,
    ) -> Result<(), DomainError>;

    /// The finalization CAS: moves the turn out of `running`
    /// (`WHERE id = :id AND state = 'running'`). `true` when this call won.
    async fn finalize_turn(
        &self,
        scope: &AccessScope,
        finalization: &TurnFinalization,
    ) -> Result<bool, DomainError>;
}

/// Persistence port of the turn lifecycle writes of the turn mutations and the
/// orphan watchdog (`scope` is a tenant scope derived from the owner-scoped
/// chat lookup; the watchdog scans with a system scope and settles every turn
/// with the scope of its own tenant).
#[async_trait]
pub trait TurnLifecycleRepository: Send + Sync {
    /// The non-deleted turn of the chat with the greatest `(started_at, id)`.
    async fn find_latest_turn(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<TurnRow>, DomainError>;

    /// A non-deleted turn of the chat that is newer than `than` by
    /// `(started_at, id)`.
    async fn has_newer_live_turn(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        than: &TurnRow,
    ) -> Result<bool, DomainError>;

    /// The mutation CAS: soft-deletes a non-deleted, non-running turn and, for
    /// a retry / edit, points it at its replacement. `true` when this call
    /// deleted it.
    async fn soft_delete_turn(
        &self,
        scope: &AccessScope,
        turn_id: Uuid,
        replaced_by: Option<Uuid>,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError>;

    /// Inserts a `running` retry / edit turn without its preflight columns. A
    /// violated `(chat_id, request_id)` or one-running-turn-per-chat constraint
    /// is `DomainError::Conflict`.
    async fn insert_unstarted_turn(
        &self,
        scope: &AccessScope,
        turn: NewUnstartedTurn,
    ) -> Result<(), DomainError>;

    /// Writes the preflight columns of a running turn (`WHERE id = :id AND
    /// state = 'running'`). `true` when the turn was still running.
    async fn record_turn_reserve(
        &self,
        scope: &AccessScope,
        turn_id: Uuid,
        columns: &TurnPreflightColumns,
    ) -> Result<bool, DomainError>;

    /// At most `limit` running, non-deleted turns whose last progress
    /// (`last_progress_at`, else `started_at`) is at or before `cutoff`,
    /// oldest first.
    async fn stale_running_turns(
        &self,
        scope: &AccessScope,
        cutoff: OffsetDateTime,
        limit: u64,
    ) -> Result<Vec<OrphanCandidate>, DomainError>;

    /// The orphan finalization CAS: moves the turn to `failed` /
    /// `orphan_timeout` only while it is still running, not deleted and stale
    /// at `cutoff`. `true` when this call won.
    async fn finalize_orphan_turn(
        &self,
        scope: &AccessScope,
        turn_id: Uuid,
        cutoff: OffsetDateTime,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError>;
}

/// Persistence port of the message writes and reads of the turn mutations
/// (`scope` is a tenant scope).
#[async_trait]
pub trait MessageMutationRepository: Send + Sync {
    /// The non-deleted user message of the turn `request_id`.
    async fn find_turn_user_message(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<MessageRow>, DomainError>;

    /// Soft-deletes every message of the turn `request_id` (user and
    /// assistant); returns the number of rows deleted.
    async fn soft_delete_turn_messages(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<u64, DomainError>;

    /// Clears `is_compressed` on every message of the chat; returns the number
    /// of rows changed.
    async fn clear_compression(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<u64, DomainError>;

    /// [`MessageRepository::latest_assistant_tokens`] without the messages of
    /// the turn `request_id` (the turn a mutation replaces).
    async fn latest_assistant_tokens_excluding(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<u64>, DomainError>;

    /// Ids of the non-deleted attachments linked to `message_id`, in link
    /// order.
    async fn live_linked_attachment_ids(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> Result<Vec<Uuid>, DomainError>;
}

/// Which messages the recent-history query returns.
#[derive(Debug, Clone, Copy)]
pub struct RecentQuery {
    /// Newest messages to return.
    pub limit: u64,
    /// The turn being started (a retry / edit stores its user message before
    /// the context is assembled): its messages are left out.
    pub excluded_request: Option<Uuid>,
    /// Frontier of the chat's thread summary: only messages strictly after it.
    pub after: Option<SummaryFrontier>,
}

/// Persistence port of `thread_summaries` and of the message reads and flags the
/// summary needs (`scope` is a tenant scope).
#[async_trait]
pub trait ThreadSummaryRepository: Send + Sync {
    /// The frontier of the chat's summary, when it has one.
    async fn find_summary_frontier(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<SummaryFrontier>, DomainError>;

    /// The chat's summary row, when it has one.
    async fn find_summary(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<SummaryRow>, DomainError>;

    /// Deletes the chat's summary row; returns the number of rows deleted.
    async fn delete_summary(&self, scope: &AccessScope, chat_id: Uuid) -> Result<u64, DomainError>;

    /// `(created_at, id)` of the latest non-deleted message of the chat that
    /// does not belong to the turn `excluded_request` (the frozen target of a
    /// summary).
    async fn latest_message_frontier(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        excluded_request: Uuid,
    ) -> Result<Option<SummaryFrontier>, DomainError>;

    /// The messages a summary of `(base, target]` covers: non-deleted,
    /// not compressed, role `user` / `assistant`, ordered `(created_at ASC,
    /// id ASC)`; `target` is inclusive, `base` (when any) exclusive.
    async fn summary_range(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        base: Option<SummaryFrontier>,
        target: SummaryFrontier,
    ) -> Result<Vec<MessageRow>, DomainError>;

    /// Makes `summary` the chat's only summary row (the previous one, if any,
    /// is replaced).
    async fn replace_summary(
        &self,
        scope: &AccessScope,
        summary: NewSummary,
    ) -> Result<(), DomainError>;

    /// Locks the live message `id` of the chat (a no-op write: the row lock on
    /// `PostgreSQL`) and tells whether it exists and is not soft-deleted. The
    /// summary commit takes it after the chat lock, so it is ordered against a
    /// turn mutation that deletes the message.
    async fn lock_live_message(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        id: Uuid,
    ) -> Result<bool, DomainError>;

    /// Sets `is_compressed` on the given messages of the chat; returns the
    /// number of rows changed.
    async fn mark_compressed(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        ids: &[Uuid],
    ) -> Result<u64, DomainError>;
}

/// Durable hand-off of a thread-summary work item to the shared outbox.
///
/// The enqueue runs inside the finalization transaction of the causing turn
/// (DESIGN 3.6 "Thread Summary Update", "Durable scheduling"); the returned
/// [`Wake`] must be fired after the commit.
#[async_trait]
pub trait ThreadSummaryPort: Send + Sync + 'static {
    /// Enqueues one work item inside the caller's transaction.
    ///
    /// # Errors
    /// `Internal` for any outbox failure (the transaction must roll back).
    async fn enqueue_thread_summary(
        &self,
        tx: &DbTx<'_>,
        task: &ThreadSummaryTask,
    ) -> Result<Wake, DomainError>;
}

/// Durable hand-off of the usage event of a settled turn to the model-policy
/// plugin.
///
/// The enqueue runs inside the finalization transaction, so the quota
/// settlement and the usage message commit or roll back together (DESIGN 5.7
/// "Settlement transaction invariant"). The returned [`Wake`] must be fired
/// after the commit.
#[async_trait]
pub trait UsagePort: Send + Sync + 'static {
    /// Enqueues one usage message.
    ///
    /// # Errors
    /// `Internal` for any outbox failure (the transaction must roll back).
    async fn enqueue_usage(&self, tx: &DbTx<'_>, event: &UsageEvent) -> Result<Wake, DomainError>;
}

/// Durable hand-off of an audit event to the audit plugin; same transactional
/// contract as [`UsagePort`].
#[async_trait]
pub trait AuditPort: Send + Sync + 'static {
    /// Enqueues one audit message.
    ///
    /// # Errors
    /// `Internal` for any outbox failure (the transaction must roll back).
    async fn enqueue_audit(&self, tx: &DbTx<'_>, event: &AuditEvent) -> Result<Wake, DomainError>;
}

/// The user a quota row belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaOwner {
    /// Tenant of the user.
    pub tenant_id: Uuid,
    /// The user.
    pub user_id: Uuid,
}

/// What a settlement does to one bucket row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaSettlement {
    /// The turn's reserve, released from `reserved_credits_micro`.
    pub release_credits_micro: i64,
    /// The committed credits, added to `spent_credits_micro`.
    pub spent_credits_micro: i64,
    /// Provider input tokens (telemetry of bucket `total`, actual settlements).
    pub input_tokens: u64,
    /// Provider output tokens (telemetry of bucket `total`, actual settlements).
    pub output_tokens: u64,
    /// Completed web search calls (bucket `total`).
    pub web_search_calls: u32,
    /// Completed code interpreter calls (bucket `total`).
    pub code_interpreter_calls: u32,
}

/// Persistence port of `quota_usage` (`scope` is tenant + owner: the owner
/// column is `user_id`).
///
/// Every write is an atomic upsert on the unique key `(tenant, user, period
/// type, period start, bucket)`: it never raises a unique violation, so the
/// reserve transaction can classify the one it does expect.
#[async_trait]
pub trait QuotaRepository: Send + Sync {
    /// The rows of the user for the periods starting at `starts`.
    async fn quota_rows(
        &self,
        scope: &AccessScope,
        owner: QuotaOwner,
        starts: PeriodStarts,
    ) -> Result<Vec<QuotaRow>, DomainError>;

    /// Adds `reserve_credits_micro` to the reserved credits of every
    /// `(bucket, period)`, creating the rows on first use.
    async fn reserve_quota(
        &self,
        scope: &AccessScope,
        owner: QuotaOwner,
        buckets: &[Bucket],
        starts: PeriodStarts,
        reserve_credits_micro: i64,
        now: OffsetDateTime,
    ) -> Result<(), DomainError>;

    /// Settles a turn against every `(bucket, period)`: releases its reserve,
    /// commits its credits and counts the call. Token and tool counters move in
    /// bucket `total` only.
    async fn settle_quota(
        &self,
        scope: &AccessScope,
        owner: QuotaOwner,
        buckets: &[Bucket],
        starts: PeriodStarts,
        settlement: &QuotaSettlement,
        now: OffsetDateTime,
    ) -> Result<(), DomainError>;
}

/// Persistence port of the per-turn facts the settlement reads (`scope` is a
/// tenant scope).
#[async_trait]
pub trait TurnLedgerRepository: Send + Sync {
    /// Completed tool calls counted on the turn.
    async fn tool_counts(
        &self,
        scope: &AccessScope,
        turn_id: Uuid,
    ) -> Result<ToolCounts, DomainError>;
}

/// Persistence port of the attachments a turn reads and links (`scope` is a
/// tenant scope derived from the owner-scoped chat lookup).
#[async_trait]
pub trait AttachmentRepository: Send + Sync {
    /// The attachments of `ids` that belong to `chat_id`, were uploaded by
    /// `user_id`, are `ready` and not soft-deleted (any subset of `ids`).
    async fn find_ready_attachments(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        user_id: Uuid,
        ids: &[Uuid],
    ) -> Result<Vec<ReadyAttachment>, DomainError>;

    /// Ready, non-deleted document attachments of the chat that were indexed
    /// for `file_search`.
    async fn ready_documents(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Vec<ReadyAttachment>, DomainError>;

    /// Ready, non-deleted attachments of the chat that were uploaded for the
    /// code interpreter (spreadsheets), chat-wide.
    async fn ready_code_interpreter_files(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Vec<ReadyAttachment>, DomainError>;

    /// Links attachments to a message (`message_attachments` rows).
    async fn link_attachments(
        &self,
        scope: &AccessScope,
        links: &[NewMessageAttachment],
    ) -> Result<(), DomainError>;
}

/// A `message_attachments` row to insert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMessageAttachment {
    /// Tenant of the chat.
    pub tenant_id: Uuid,
    /// The chat.
    pub chat_id: Uuid,
    /// The user message.
    pub message_id: Uuid,
    /// The attachment.
    pub attachment_id: Uuid,
    /// Link time.
    pub now: OffsetDateTime,
}

/// Persistence port of `chat_vector_stores` (`scope` is a tenant scope derived
/// from the owner-scoped chat lookup, or the tenant of a cleanup message).
#[async_trait]
pub trait VectorStoreRepository: Send + Sync {
    /// The provider vector store id of the chat; `None` when the chat has no
    /// row or the store is still being created.
    async fn find_vector_store_id(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<String>, DomainError>;

    /// The vector store row of the chat (a placeholder included).
    async fn find_vector_store(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<VectorStoreRow>, DomainError>;

    /// Inserts the placeholder row (`vector_store_id = NULL`) that makes the
    /// caller the creator of the chat's store. A violated `(tenant, chat)`
    /// unique index is `DomainError::Conflict`.
    async fn insert_vector_store_placeholder(
        &self,
        scope: &AccessScope,
        row: &VectorStoreRow,
    ) -> Result<(), DomainError>;

    /// The compare-and-set of the creator: sets `vector_store_id` only while it
    /// is still `NULL`. `true` when this call set it.
    async fn set_vector_store_id(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        row_id: Uuid,
        vector_store_id: &str,
    ) -> Result<bool, DomainError>;

    /// Deletes a placeholder that never received its store id; `true` when a
    /// row was deleted.
    async fn delete_vector_store_placeholder(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        row_id: Uuid,
    ) -> Result<bool, DomainError>;

    /// Deletes the vector store row of the chat (the durable marker of the
    /// provider cleanup, removed once the provider store is gone).
    async fn delete_vector_store_row(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<bool, DomainError>;
}

/// Persistence port of the attachment lifecycle: upload, read, delete and the
/// background indexing (`scope` is a tenant scope derived from the
/// owner-scoped chat lookup).
///
/// Every status change is a conditional update (`rows_affected` tells the
/// winner), so the request, the background task, the reaper and the cleanup
/// handlers never overwrite each other's decision.
#[async_trait]
pub trait AttachmentLifecycleRepository: Send + Sync {
    /// Document count and byte total of the chat's live (non-failed,
    /// non-deleted) attachments.
    async fn attachment_usage(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<AttachmentUsage, DomainError>;

    /// Inserts a `pending` attachment.
    async fn insert_attachment(
        &self,
        scope: &AccessScope,
        attachment: NewAttachment,
    ) -> Result<(), DomainError>;

    /// An attachment of the chat in any state (soft-deleted ones included).
    async fn find_attachment(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        id: Uuid,
    ) -> Result<Option<AttachmentRow>, DomainError>;

    /// `pending` -> `uploaded` with the provider file id; `false` when the row
    /// is no longer a live `pending` row or a cleanup (chat deletion) owns it.
    async fn mark_attachment_uploaded(
        &self,
        scope: &AccessScope,
        id: Uuid,
        provider_file_id: &str,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError>;

    /// `uploaded` -> `ready` (with the thumbnail of an image); `false` when the
    /// row is deleted, marked for cleanup or no longer `uploaded`.
    async fn mark_attachment_ready(
        &self,
        scope: &AccessScope,
        id: Uuid,
        thumbnail: Option<&NewThumbnail>,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError>;

    /// `pending` / `uploaded` -> `failed` with `error_code`; `false` when the
    /// row left those states.
    async fn mark_attachment_failed(
        &self,
        scope: &AccessScope,
        id: Uuid,
        error_code: &str,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError>;

    /// Refreshes `updated_at` of a live `uploaded` row that is not marked for
    /// cleanup; `false` when the row is gone, finished or taken.
    async fn refresh_uploaded_attachment(
        &self,
        scope: &AccessScope,
        id: Uuid,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError>;

    /// The background indexing failure: `uploaded` -> `failed`
    /// (`indexing_failed`) with `cleanup_status = 'pending'`, guarded like
    /// [`Self::mark_attachment_ready`]. `true` when this call won.
    async fn fail_indexing_with_cleanup(
        &self,
        scope: &AccessScope,
        id: Uuid,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError>;

    /// Sets `deleted_at` of a live attachment; `true` when this call deleted it.
    async fn soft_delete_attachment(
        &self,
        scope: &AccessScope,
        id: Uuid,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError>;

    /// Sets `cleanup_status = 'pending'` while it is unset; `true` when this
    /// call set it (the provider file still needs its cleanup message).
    async fn mark_attachment_cleanup_pending(
        &self,
        scope: &AccessScope,
        id: Uuid,
    ) -> Result<bool, DomainError>;

    /// Whether any message of the chat references the attachment.
    async fn is_attachment_linked(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        id: Uuid,
    ) -> Result<bool, DomainError>;
}

/// Persistence port of the provider cleanup bookkeeping and the upload reaper.
/// The reaper scans with a system scope; the cleanup handlers use the scope of
/// the tenant of their message.
#[async_trait]
pub trait AttachmentCleanupRepository: Send + Sync {
    /// At most `limit` `pending` / `uploaded` attachments that are live, not
    /// marked for cleanup and last updated before `cutoff`, oldest first.
    async fn stale_attachments(
        &self,
        scope: &AccessScope,
        cutoff: OffsetDateTime,
        limit: u64,
    ) -> Result<Vec<AttachmentRow>, DomainError>;

    /// The reaper CAS: `status = seen status` -> `failed`
    /// (`upload_abandoned`, `updated_at = now`), setting
    /// `cleanup_status = 'pending'` when `cleanup` is set, only while the row is
    /// still live, unmarked and stale at `cutoff`. `true` when this call won.
    async fn reap_attachment(
        &self,
        scope: &AccessScope,
        candidate: &AttachmentRow,
        cutoff: OffsetDateTime,
        cleanup: bool,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError>;

    /// The attachments of the chat whose provider cleanup is `pending`
    /// (soft-deleted ones included), oldest first.
    async fn pending_cleanup_attachments(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Vec<AttachmentRow>, DomainError>;

    /// `pending` -> `done`; `false` when the row is not `pending`.
    async fn mark_cleanup_done(
        &self,
        scope: &AccessScope,
        id: Uuid,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError>;

    /// Counts one failed provider delete on a `pending` row (attempts + 1,
    /// `last_cleanup_error`, `cleanup_updated_at`) and returns the new count;
    /// `None` when the row is not `pending`.
    async fn record_cleanup_attempt(
        &self,
        scope: &AccessScope,
        id: Uuid,
        error: &str,
        now: OffsetDateTime,
    ) -> Result<Option<i32>, DomainError>;

    /// `pending` -> `failed`: the retry budget is spent.
    async fn mark_cleanup_failed(
        &self,
        scope: &AccessScope,
        id: Uuid,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError>;
}

/// Durable hand-off of the provider file cleanup of an attachment (deleted,
/// abandoned or failed to index) to the attachment-cleanup handler.
///
/// The enqueue runs inside the caller's transaction (the status change and the
/// message commit or roll back together); the returned [`Wake`] is fired after
/// the commit.
#[async_trait]
pub trait AttachmentCleanupPort: Send + Sync + 'static {
    /// Enqueues one attachment-cleanup message.
    ///
    /// # Errors
    /// `Internal` for any outbox failure (the transaction must roll back).
    async fn enqueue_attachment_cleanup(
        &self,
        tx: &DbTx<'_>,
        event: &AttachmentCleanupEvent,
    ) -> Result<Wake, DomainError>;
}

/// Where a provider storage call goes: the tenant (a tenant override selects
/// its own alias) and the storage label of the backend that holds the files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageTarget {
    /// Tenant of the call.
    pub tenant_id: Uuid,
    /// Storage label (`attachments.storage_backend`).
    pub backend: String,
}

/// A file to upload to the provider Files API.
#[derive(Debug, Clone)]
pub struct FileUpload {
    /// Provider side file name (`{chat_id}_{attachment_id}.{ext}`).
    pub filename: String,
    /// Content type of the bytes.
    pub content_type: String,
    /// The file.
    pub data: Bytes,
}

/// Indexing state of a vector store file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexStatus {
    /// Still being indexed (also an answer without a status).
    InProgress,
    /// Searchable.
    Completed,
    /// `failed`, `cancelled` or any other status.
    Failed,
}

/// A failed provider storage call. The text is for logs only: it never carries
/// a provider body and never reaches a client.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("provider storage call failed: {message}")]
pub struct StorageError {
    /// A later attempt may succeed (provider 5xx / 429, gateway failure).
    pub transient: bool,
    /// Short diagnostic.
    pub message: String,
}

/// Provider Files and Vector Stores operations (DESIGN 3.5 "Provider API
/// Mapping") through the OAGW proxy. Every call carries the tenant and the
/// storage label of the backend, plus the security context of the caller.
#[async_trait]
pub trait StoragePort: Send + Sync {
    /// The storage label that serves the files of the catalog provider
    /// `provider_id` (its `rag_provider`, else itself).
    ///
    /// # Errors
    /// `Internal` for an unknown provider.
    fn backend_for(&self, provider_id: &str) -> Result<String, DomainError>;

    /// The OAGW alias a call to `target` goes through.
    fn upstream_alias(&self, target: &StorageTarget) -> Option<String>;

    /// Uploads a file (`purpose = assistants`); returns the provider file id.
    ///
    /// # Errors
    /// [`StorageError`] for any non-2xx answer or gateway failure.
    async fn upload_file(
        &self,
        ctx: &SecurityContext,
        target: &StorageTarget,
        file: FileUpload,
    ) -> Result<String, StorageError>;

    /// Deletes a provider file; a `404` is success.
    ///
    /// # Errors
    /// [`StorageError`] for any other non-2xx answer or gateway failure.
    async fn delete_file(
        &self,
        ctx: &SecurityContext,
        target: &StorageTarget,
        file_id: &str,
    ) -> Result<(), StorageError>;

    /// Creates a vector store; returns its provider id.
    ///
    /// # Errors
    /// [`StorageError`] for any non-2xx answer or gateway failure.
    async fn create_vector_store(
        &self,
        ctx: &SecurityContext,
        target: &StorageTarget,
    ) -> Result<String, StorageError>;

    /// Adds a file to a vector store with the attribute `attachment_id`;
    /// returns the first indexing status.
    ///
    /// # Errors
    /// [`StorageError`] for any non-2xx answer or gateway failure.
    async fn add_file_to_store(
        &self,
        ctx: &SecurityContext,
        target: &StorageTarget,
        store_id: &str,
        file_id: &str,
        attachment_id: Uuid,
    ) -> Result<IndexStatus, StorageError>;

    /// Reads the indexing status of a vector store file.
    ///
    /// # Errors
    /// [`StorageError`]; `transient` for a provider 5xx / 429 or a gateway
    /// failure, permanent for any other error status.
    async fn index_status(
        &self,
        ctx: &SecurityContext,
        target: &StorageTarget,
        store_id: &str,
        file_id: &str,
    ) -> Result<IndexStatus, StorageError>;

    /// Deletes a vector store; a `404` is success.
    ///
    /// # Errors
    /// [`StorageError`] for any other non-2xx answer or gateway failure.
    async fn delete_vector_store(
        &self,
        ctx: &SecurityContext,
        target: &StorageTarget,
        store_id: &str,
    ) -> Result<(), StorageError>;
}

/// The LLM provider layer as the stream service sees it (ADR-0001, ADR-0005).
///
/// The implementation resolves the provider entry (and tenant override) of a
/// catalog `provider_id`, selects the adapter by provider kind and talks to
/// the provider through OAGW.
#[async_trait]
pub trait LlmGateway: Send + Sync {
    /// Pre-stream resolution check: the provider of `provider_id` has an
    /// adapter and an OAGW alias for `tenant_id`.
    ///
    /// # Errors
    /// `Internal` for an unknown provider or an unsupported provider kind.
    fn check_provider(&self, tenant_id: Uuid, provider_id: &str) -> Result<(), DomainError>;

    /// Starts a streaming chat call. Dropping the returned stream cancels the
    /// provider request.
    ///
    /// # Errors
    /// A [`ProviderFailure`] when the provider rejects the request or cannot
    /// be reached (the gear never retries).
    async fn stream_chat(
        &self,
        ctx: &SecurityContext,
        request: &ChatRequest,
    ) -> Result<ProviderEventStream, ProviderFailure>;
}
