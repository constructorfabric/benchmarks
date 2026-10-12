//! Persistence ports of the domain services (DESIGN section 3.7 "Database
//! Schemas & Tables").
//!
//! The domain talks to storage only through the traits of this module; the
//! Secure ORM implementations live in `crate::infra::db::repo`. The ports
//! exchange plain records (no `SeaORM` types) and typed value sets.
//!
//! **Scope contract.** Every port method takes the [`AccessScope`] the PEP
//! returned for the parent chat (`AuthzService::authorize_chat`). The chat
//! queries apply it as is; queries of child tables (messages, attachments,
//! turns, reactions) re-target it to the child table's columns (tenant, or
//! tenant + owner for reactions) and always add the parent `chat_id`
//! predicate. Callers therefore load the parent chat through
//! [`ChatRepository::find_active`] (or a list built from it) before touching
//! its children: a child query alone never authorizes access.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use mini_chat_sdk::{AuditEvent, UsageEvent, UsageTokens};
use time::OffsetDateTime;
use toolkit_db::outbox::Wake;
use toolkit_db::secure::DbTx;
use toolkit_odata::{ODataQuery, Page};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::DomainError;

// ── Clock ────────────────────────────────────────────────────────────────

/// Current UTC time truncated to microseconds, the precision every supported
/// backend stores, so a value returned from a write equals the value read
/// back later.
///
/// **Timestamp storage rule.** Ports exchange timestamps as
/// `OffsetDateTime`; the repositories own their storage form. On `SQLite`
/// timestamp columns are `TEXT` compared byte-wise, so every timestamp a
/// repository writes or compares (inserts, updates, filter literals, cursor
/// values) is bound through the database's
/// `crate::infra::db::entities::TimestampForm` (fixed-width
/// `YYYY-MM-DDTHH:MM:SS.ffffffZ` text on `SQLite`, `TIMESTAMPTZ` on
/// PostgreSQL), never as a raw `OffsetDateTime`.
#[must_use]
pub fn storage_now() -> OffsetDateTime {
    let now = OffsetDateTime::now_utc();
    now.replace_microsecond(now.microsecond()).unwrap_or(now)
}

// ── Value sets ───────────────────────────────────────────────────────────

/// `messages.role`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MessageRole {
    User,
    Assistant,
    System,
}

impl MessageRole {
    /// Stored and wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::System => "system",
        }
    }

    /// The role stored as `value`; `None` for a value outside the set.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "user" => Some(Self::User),
            "assistant" => Some(Self::Assistant),
            "system" => Some(Self::System),
            _ => None,
        }
    }
}

/// `message_reactions.reaction`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Reaction {
    Like,
    Dislike,
}

impl Reaction {
    /// Stored and wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Like => "like",
            Self::Dislike => "dislike",
        }
    }

    /// The reaction named `value` (exact, case-sensitive match).
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "like" => Some(Self::Like),
            "dislike" => Some(Self::Dislike),
            _ => None,
        }
    }
}

/// `attachments.attachment_kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AttachmentKind {
    Document,
    Image,
}

impl AttachmentKind {
    /// Stored and wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Document => "document",
            Self::Image => "image",
        }
    }

    /// The kind stored as `value`; `None` for a value outside the set.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "document" => Some(Self::Document),
            "image" => Some(Self::Image),
            _ => None,
        }
    }
}

/// `attachments.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AttachmentStatus {
    Pending,
    Uploaded,
    Ready,
    Failed,
}

impl AttachmentStatus {
    /// Stored and wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Uploaded => "uploaded",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }

    /// The status stored as `value`; `None` for a value outside the set.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "uploaded" => Some(Self::Uploaded),
            "ready" => Some(Self::Ready),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// `attachments.cleanup_status` (no CHECK in the database, ADR-0010).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CleanupStatus {
    Pending,
    Done,
    Failed,
}

impl CleanupStatus {
    /// Stored value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }
}

/// Internal `chat_turns.state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TurnState {
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl TurnState {
    /// Stored value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// The state stored as `value`; `None` for a value outside the set.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "running" => Some(Self::Running),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

// ── Records ──────────────────────────────────────────────────────────────

/// A `chats` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatRecord {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    /// Model locked for the chat lifetime.
    pub model: String,
    pub title: Option<String>,
    pub is_temporary: bool,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// A chat to insert (`created_at = updated_at = now`, not temporary).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewChat {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub model: String,
    pub title: Option<String>,
    pub now: OffsetDateTime,
}

/// The soft delete of one chat ("Cleanup on Chat Deletion"): the chat gets
/// `deleted_at = updated_at = deleted_at`, its live attachments without a
/// cleanup outcome are marked `cleanup_status = pending`, and one chat-cleanup
/// outbox message carrying `system_request_id` is enqueued, all in one
/// transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChatDeletion {
    pub chat_id: Uuid,
    pub deleted_at: OffsetDateTime,
    /// Stable identity of the cleanup work (UUID v4), persisted in the payload.
    pub system_request_id: Uuid,
}

/// A `messages` row (the columns the domain reads).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRecord {
    pub id: Uuid,
    pub chat_id: Uuid,
    /// Nullable in storage; a live message without it is a data error.
    pub request_id: Option<Uuid>,
    pub role: MessageRole,
    pub content: String,
    /// Effective model of an assistant message.
    pub model: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub created_at: OffsetDateTime,
}

/// Stored preview thumbnail of an image attachment (`image/webp`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredThumbnail {
    pub data: Vec<u8>,
    pub width: i32,
    pub height: i32,
}

/// One live attachment linked to a message (`message_attachments` inner join
/// `attachments`), projected to the `AttachmentSummary` columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentSummaryRecord {
    pub message_id: Uuid,
    pub attachment_id: Uuid,
    pub kind: AttachmentKind,
    pub filename: String,
    pub status: AttachmentStatus,
    /// Present when the row stores a thumbnail with both dimensions.
    pub thumbnail: Option<StoredThumbnail>,
}

/// A reaction to upsert for `(message_id, user_id)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewReaction {
    /// Row id used when no reaction exists yet.
    pub id: Uuid,
    pub message_id: Uuid,
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub reaction: Reaction,
    pub created_at: OffsetDateTime,
}

/// A stored `message_reactions` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReactionRecord {
    pub message_id: Uuid,
    pub user_id: Uuid,
    pub reaction: Reaction,
    pub created_at: OffsetDateTime,
}

/// A `chat_turns` row (the columns the turn status reads).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnRecord {
    pub id: Uuid,
    pub chat_id: Uuid,
    pub request_id: Uuid,
    pub state: TurnState,
    pub error_code: Option<String>,
    pub assistant_message_id: Option<Uuid>,
    pub updated_at: OffsetDateTime,
}

// ── Streaming records (DESIGN sections 3.6, 3.7, 5.7) ────────────────────

/// Order key of a message in the strict per-chat order `(created_at, id)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MessageKey {
    pub created_at: OffsetDateTime,
    pub id: Uuid,
}

/// A message of the recent-messages window of a context plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextMessageRecord {
    pub id: Uuid,
    pub role: MessageRole,
    pub content: String,
    pub created_at: OffsetDateTime,
}

/// The committed thread summary of a chat (`thread_summaries`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadSummaryRecord {
    pub text: String,
    /// Inclusive frontier `(summarized_up_to_created_at, summarized_up_to_message_id)`.
    pub frontier: MessageKey,
    pub token_estimate: i32,
}

/// The user message of a new turn (`content_type = text`, `request_kind =
/// chat`, `features_used = []`, no usage).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewUserMessage {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub request_id: Uuid,
    pub content: String,
    pub created_at: OffsetDateTime,
}

/// The assistant message of a finalized turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAssistantMessage {
    /// The id pre-allocated at stream start.
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub request_id: Uuid,
    pub content: String,
    /// Effective model of the turn.
    pub model: String,
    /// Provider-reported usage (zeros when unknown).
    pub usage: UsageTokens,
    /// Provider response id; internal only.
    pub provider_response_id: Option<String>,
    pub created_at: OffsetDateTime,
}

/// A `running` user turn with its preflight columns (send path).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRunningTurn {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub request_id: Uuid,
    pub requester_user_id: Uuid,
    pub reserve_tokens: i64,
    pub max_output_tokens_applied: u32,
    pub reserved_credits_micro: i64,
    pub policy_version_applied: u64,
    pub effective_model: String,
    pub minimal_generation_floor_applied: u32,
    pub web_search_enabled: bool,
    /// `started_at = last_progress_at = updated_at`.
    pub now: OffsetDateTime,
}

/// The idempotency-key view of a turn (soft-deleted turns included).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnKeyRecord {
    pub id: Uuid,
    pub state: TurnState,
    /// `deleted_at IS NOT NULL` (replaced by retry/edit, or deleted).
    pub deleted: bool,
    pub assistant_message_id: Option<Uuid>,
    pub effective_model: Option<String>,
}

/// Order key of a turn in the latest-turn order `(started_at, id)` (DESIGN
/// section 3.9 "Definition").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TurnKey {
    pub started_at: OffsetDateTime,
    pub id: Uuid,
}

/// A turn as the mutation rules see it (DESIGN section 3.9): soft-deleted
/// turns included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationTargetRecord {
    pub id: Uuid,
    pub chat_id: Uuid,
    pub request_id: Uuid,
    pub state: TurnState,
    /// `deleted_at IS NOT NULL`.
    pub deleted: bool,
    pub requester_user_id: Option<Uuid>,
    /// Reused by the replacement turn of a retry / edit.
    pub web_search_enabled: bool,
    pub started_at: OffsetDateTime,
}

impl MutationTargetRecord {
    /// The turn's position in the latest-turn order.
    #[must_use]
    pub const fn key(&self) -> TurnKey {
        TurnKey {
            started_at: self.started_at,
            id: self.id,
        }
    }
}

/// The soft delete of a terminal turn by a mutation: `deleted_at =
/// updated_at = at`, `replaced_by_request_id` for retry / edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnSoftDelete {
    pub turn_id: Uuid,
    pub replaced_by_request_id: Option<Uuid>,
    pub at: OffsetDateTime,
}

/// The `running` replacement turn of a retry / edit, inserted by the
/// mutation commit with NULL preflight columns (DESIGN section 3.7
/// "Preflight columns on retry/edit").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMutationTurn {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    /// Server-generated UUID v4.
    pub request_id: Uuid,
    pub requester_user_id: Uuid,
    pub web_search_enabled: bool,
    /// `started_at = last_progress_at = updated_at`.
    pub now: OffsetDateTime,
}

/// The preflight columns of a retry / edit turn, written once by the reserve
/// transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnPreflightColumns {
    pub reserve_tokens: i64,
    pub max_output_tokens_applied: u32,
    pub reserved_credits_micro: i64,
    pub policy_version_applied: u64,
    pub effective_model: String,
    pub minimal_generation_floor_applied: u32,
}

/// The copy of the live attachment links of one user message to another
/// (`message_attachments`, `created_at = at`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttachmentLinkCopy {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub from_message_id: Uuid,
    pub to_message_id: Uuid,
    pub at: OffsetDateTime,
}

/// Completed tool calls of a turn (`chat_turns.*_completed_count`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ToolCallCounts {
    pub web_search: u32,
    pub code_interpreter: u32,
    pub file_search: u32,
}

/// The terminal update of the finalization CAS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnFinalization {
    pub turn_id: Uuid,
    /// Terminal state (never [`TurnState::Running`]).
    pub state: TurnState,
    pub error_code: Option<String>,
    pub assistant_message_id: Option<Uuid>,
    pub provider_response_id: Option<String>,
    pub counts: ToolCallCounts,
    /// `completed_at = updated_at`.
    pub at: OffsetDateTime,
}

/// The `chat_turns` row returned by a winning finalization CAS: the persisted
/// fields settlement and the outbox events are built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalizedTurnRecord {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub request_id: Uuid,
    pub requester_user_id: Option<Uuid>,
    pub effective_model: Option<String>,
    pub policy_version_applied: Option<i64>,
    pub reserve_tokens: Option<i64>,
    pub max_output_tokens_applied: Option<i32>,
    pub reserved_credits_micro: Option<i64>,
    pub minimal_generation_floor_applied: Option<i32>,
    pub started_at: OffsetDateTime,
    pub counts: ToolCallCounts,
}

/// An `attachments` row as the stream service sees it. `provider_file_id`
/// is internal only and never leaves the gear.
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent stored columns: purpose flags and the soft-delete mark"
)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatAttachmentRecord {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub uploaded_by_user_id: Uuid,
    pub kind: AttachmentKind,
    pub status: AttachmentStatus,
    pub for_file_search: bool,
    pub for_code_interpreter: bool,
    pub filename: String,
    pub provider_file_id: Option<String>,
    /// File id of the secondary (Anthropic Files API) copy; set only when
    /// `secondary_status = uploaded`. Internal only.
    pub secondary_file_id: Option<String>,
    pub deleted: bool,
}

/// The `message_attachments` rows of one user message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageAttachmentLinks {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub message_id: Uuid,
    pub attachment_ids: Vec<Uuid>,
    pub at: OffsetDateTime,
}

/// A durable thread-summary work item (DESIGN section 3.6 "Durable
/// scheduling").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadSummaryTask {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    /// Stable identity of the work, generated once at enqueue time.
    pub system_request_id: Uuid,
    /// Summary frontier at enqueue time (`None` before the first summary).
    pub base_frontier: Option<MessageKey>,
    /// Inclusive upper bound of the range to summarize.
    pub frozen_target: MessageKey,
}

// ── Ports ────────────────────────────────────────────────────────────────

/// `chats` persistence.
#[async_trait]
pub trait ChatRepository: Send + Sync {
    /// Inserts `chat`; the row must satisfy `scope` (tenant + owner).
    async fn insert(&self, scope: &AccessScope, chat: NewChat) -> Result<ChatRecord, DomainError>;

    /// The non-deleted chat `chat_id` visible through `scope`.
    async fn find_active(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<ChatRecord>, DomainError>;

    /// One page of the non-deleted chats visible through `scope`.
    ///
    /// `query` comes from the platform `OData` extractor; filter and order
    /// fields are `updated_at`, `id` and `title`; the default order is
    /// `updated_at desc` with the `id desc` tiebreaker; the page size defaults
    /// to 20 and is clamped to 100. `OData` failures are
    /// [`DomainError::InvalidQuery`].
    async fn list_active(
        &self,
        scope: &AccessScope,
        query: &ODataQuery,
    ) -> Result<Page<ChatRecord>, DomainError>;

    /// Number of non-deleted messages per chat for chats already loaded
    /// through `scope`, in one query; chats without messages are absent.
    async fn count_active_messages(
        &self,
        scope: &AccessScope,
        chat_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, u64>, DomainError>;

    /// Sets the title of the non-deleted chat `chat_id` and bumps
    /// `updated_at`; `None` when no such chat is visible.
    async fn update_title(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        title: &str,
        now: OffsetDateTime,
    ) -> Result<Option<ChatRecord>, DomainError>;

    /// Applies `deletion` in one transaction; `false` when the chat is not
    /// visible or already deleted (nothing changes then).
    ///
    /// # Errors
    ///
    /// [`DomainError::InvalidFormat`] when the chat-cleanup payload exceeds
    /// the outbox size limit; [`DomainError::Internal`] on any other failure.
    async fn soft_delete(
        &self,
        scope: &AccessScope,
        deletion: ChatDeletion,
    ) -> Result<bool, DomainError>;

    /// Bumps `updated_at` of the non-deleted chat `chat_id` to `at` inside
    /// `tx`; `false` when no such chat is visible.
    async fn touch_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        chat_id: Uuid,
        at: OffsetDateTime,
    ) -> Result<bool, DomainError>;
}

/// `messages` reads.
#[async_trait]
pub trait MessageRepository: Send + Sync {
    /// One page of the non-deleted messages of `chat_id`.
    ///
    /// Filter and order fields are `created_at`, `id` (also accepted as a
    /// quoted UUID literal) and `role`; the default order is `created_at asc`
    /// with the `id asc` tiebreaker; the page size defaults to 20 and is
    /// clamped to 100.
    async fn list_active(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        query: &ODataQuery,
    ) -> Result<Page<MessageRecord>, DomainError>;

    /// The non-deleted message `message_id` of `chat_id`.
    async fn find_active(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> Result<Option<MessageRecord>, DomainError>;

    /// Order key of the latest non-deleted message of `chat_id` with a
    /// `request_id` (the context snapshot boundary); `None` for an empty chat.
    async fn latest_key(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<MessageKey>, DomainError>;

    /// `input_tokens + output_tokens` of the latest non-deleted assistant
    /// message of `chat_id` with non-zero usage; 0 when there is none. With
    /// `exclude_request`, messages of that turn are skipped (the retry / edit
    /// preflight, where the replaced turn is not part of the re-sent
    /// history).
    async fn prior_context_tokens(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        exclude_request: Option<Uuid>,
    ) -> Result<u64, DomainError>;

    /// The recent-messages query of the context plan: the newest `limit`
    /// non-deleted, non-compressed messages of `chat_id` with a `request_id`
    /// whose key is `<= boundary` (and `> frontier` when a summary frontier
    /// is given), returned in chronological order `(created_at, id)`.
    async fn recent_for_context(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        boundary: MessageKey,
        frontier: Option<MessageKey>,
        limit: u32,
    ) -> Result<Vec<ContextMessageRecord>, DomainError>;

    /// The chat's committed thread summary, if any.
    async fn thread_summary(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<ThreadSummaryRecord>, DomainError>;

    /// The summary frontier of `chat_id`, read in `tx`.
    async fn summary_frontier_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<MessageKey>, DomainError>;

    /// Order key of the latest non-deleted message of `chat_id` that does not
    /// carry `request_id` (the frozen thread-summary target), read in `tx`.
    async fn latest_key_excluding_request_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<MessageKey>, DomainError>;

    /// Inserts the user message of a new turn inside `tx`.
    ///
    /// # Errors
    ///
    /// `409 aborted` (`request_id_conflict`) when a live message of the chat
    /// already carries `(request_id, user)`; [`DomainError::Internal`] on any
    /// other failure.
    async fn insert_user_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        message: NewUserMessage,
    ) -> Result<(), DomainError>;

    /// Inserts the (complete or partial) assistant message of a turn inside
    /// `tx`.
    async fn insert_assistant_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        message: NewAssistantMessage,
    ) -> Result<(), DomainError>;

    // ── Turn mutations (DESIGN section 3.9) ──

    /// The non-deleted user message of the turn `request_id` of `chat_id`.
    async fn find_user_of_request(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<MessageRecord>, DomainError>;

    /// Order key of the latest non-deleted message of `chat_id` that does not
    /// carry `request_id` (the context snapshot boundary of a retry / edit
    /// turn whose user message is already stored).
    async fn latest_key_excluding_request(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<MessageKey>, DomainError>;

    /// Soft-deletes the live messages of the turn `request_id` of `chat_id`
    /// inside `tx` (`deleted_at = at`); returns the number of rows.
    async fn soft_delete_request_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
        at: OffsetDateTime,
    ) -> Result<u64, DomainError>;

    /// Deletes the thread summary of `chat_id` and clears `is_compressed` on
    /// every message of the chat inside `tx`; `false` when no summary existed.
    async fn drop_thread_summary_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<bool, DomainError>;
}

/// `attachments` / `message_attachments` reads.
#[async_trait]
pub trait AttachmentRepository: Send + Sync {
    /// The non-deleted attachments linked to `message_ids` of `chat_id`, in
    /// one query, ordered by link time.
    async fn summaries_for_messages(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        message_ids: &[Uuid],
    ) -> Result<Vec<AttachmentSummaryRecord>, DomainError>;

    /// The `ready`, non-deleted attachments of `chat_id` (tools, code
    /// interpreter files and the citation map of a turn).
    async fn ready_for_chat(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Vec<ChatAttachmentRecord>, DomainError>;

    /// The attachments `ids` of `chat_id` visible through the tenant scope,
    /// in any status, deleted or not (validation of `attachment_ids`).
    async fn find_in_chat(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        ids: &[Uuid],
    ) -> Result<Vec<ChatAttachmentRecord>, DomainError>;

    /// [`Self::find_in_chat`] inside `tx`.
    async fn find_in_chat_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        chat_id: Uuid,
        ids: &[Uuid],
    ) -> Result<Vec<ChatAttachmentRecord>, DomainError>;

    /// Links `attachment_ids` to the user message `message_id` of `chat_id`
    /// inside `tx` (`message_attachments`, `created_at = at`).
    async fn link_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        link: MessageAttachmentLinks,
    ) -> Result<(), DomainError>;

    /// The provider vector store of `chat_id`, once created.
    async fn vector_store_id(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<String>, DomainError>;

    /// The non-deleted attachments linked to the message `message_id` of
    /// `chat_id`, ordered by link time (then attachment id).
    async fn live_for_message(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> Result<Vec<ChatAttachmentRecord>, DomainError>;

    /// Copies the links of `copy.from_message_id` to non-deleted attachments
    /// onto `copy.to_message_id` inside `tx` (the old rows stay); returns the
    /// copied attachment ids in link order.
    async fn copy_live_links_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        copy: AttachmentLinkCopy,
    ) -> Result<Vec<Uuid>, DomainError>;
}

/// `message_reactions` persistence (always the caller's own rows).
#[async_trait]
pub trait ReactionRepository: Send + Sync {
    /// The reactions of `user_id` on `message_ids`, in one query.
    async fn reactions_of_user(
        &self,
        scope: &AccessScope,
        user_id: Uuid,
        message_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, Reaction>, DomainError>;

    /// Inserts or replaces the reaction of `(message_id, user_id)`.
    async fn upsert(
        &self,
        scope: &AccessScope,
        reaction: NewReaction,
    ) -> Result<ReactionRecord, DomainError>;

    /// Removes the reaction of `(message_id, user_id)`; no-op when absent.
    async fn delete(
        &self,
        scope: &AccessScope,
        message_id: Uuid,
        user_id: Uuid,
    ) -> Result<(), DomainError>;
}

/// `chat_turns` persistence: reads, the running-turn insert, the
/// finalization CAS and progress updates.
#[async_trait]
pub trait TurnRepository: Send + Sync {
    /// The non-deleted turn of `chat_id` with `request_id`.
    async fn find_active_by_request(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<TurnRecord>, DomainError>;

    /// The turn of `(chat_id, request_id)`, soft-deleted or not (idempotency
    /// key lookup).
    async fn find_by_request_any(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<TurnKeyRecord>, DomainError>;

    /// Whether `chat_id` has a non-deleted `running` turn.
    async fn running_exists(&self, scope: &AccessScope, chat_id: Uuid)
    -> Result<bool, DomainError>;

    /// Inserts a `running` user turn with its preflight columns inside `tx`.
    ///
    /// # Errors
    ///
    /// `409 aborted` with `turn_already_running` when another turn of the
    /// chat is running (`uq_chat_turns_running`), `request_id_conflict` when
    /// `(chat_id, request_id)` is taken; [`DomainError::Internal`] otherwise.
    async fn insert_running_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        turn: NewRunningTurn,
    ) -> Result<(), DomainError>;

    /// The finalization CAS inside `tx`: `UPDATE chat_turns SET state, ...
    /// WHERE id = ? AND state = 'running'`, issued as the first statement of
    /// the finalization transaction. Returns the finalized row, or `None` when
    /// the turn was no longer running (CAS loser).
    async fn finalize_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        finalization: TurnFinalization,
    ) -> Result<Option<FinalizedTurnRecord>, DomainError>;

    /// Persists the tool-call counters of a running turn and, when
    /// `progress_at` is set, refreshes `last_progress_at` (one statement,
    /// guarded by `state = 'running'`); `false` when the turn is no longer
    /// running.
    async fn record_progress(
        &self,
        scope: &AccessScope,
        turn_id: Uuid,
        counts: ToolCallCounts,
        progress_at: Option<OffsetDateTime>,
    ) -> Result<bool, DomainError>;

    // ── Turn mutations (DESIGN section 3.9) ──

    /// The turn of `(chat_id, request_id)`, soft-deleted or not.
    async fn find_mutation_target(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<MutationTargetRecord>, DomainError>;

    /// [`Self::find_mutation_target`] inside `tx`.
    async fn find_mutation_target_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<MutationTargetRecord>, DomainError>;

    /// Order key of the latest non-deleted turn of `chat_id` (greatest
    /// `(started_at, id)`).
    async fn latest_active(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<TurnKey>, DomainError>;

    /// Whether a non-deleted turn of `chat_id` follows `key` in the
    /// latest-turn order, read in `tx` (the rows are locked with `FOR
    /// UPDATE` on PostgreSQL).
    async fn newer_active_exists_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        chat_id: Uuid,
        key: TurnKey,
    ) -> Result<bool, DomainError>;

    /// Soft-deletes the turn `delete.turn_id` inside `tx` when it is live and
    /// terminal (one guarded `UPDATE`); `false` otherwise.
    async fn soft_delete_terminal_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        delete: TurnSoftDelete,
    ) -> Result<bool, DomainError>;

    /// Inserts the `running` replacement turn of a retry / edit with NULL
    /// preflight columns inside `tx`.
    ///
    /// # Errors
    ///
    /// `409 aborted` with `turn_already_running` when another turn of the
    /// chat is running (`uq_chat_turns_running`), `request_id_conflict` when
    /// `(chat_id, request_id)` is taken; [`DomainError::Internal`] otherwise.
    async fn insert_mutation_turn_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        turn: NewMutationTurn,
    ) -> Result<(), DomainError>;

    /// Writes the preflight columns of the running turn `turn_id` inside `tx`
    /// when they are still NULL; `false` when the turn is no longer running or
    /// the columns are already set.
    async fn record_preflight_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        turn_id: Uuid,
        columns: TurnPreflightColumns,
    ) -> Result<bool, DomainError>;

    /// The plain CAS of an unstarted retry / edit turn: `running → failed`
    /// with `error_code` (`completed_at = updated_at = at`), one statement;
    /// `false` when the turn was no longer running.
    async fn fail_unstarted(
        &self,
        scope: &AccessScope,
        turn_id: Uuid,
        error_code: &str,
        at: OffsetDateTime,
    ) -> Result<bool, DomainError>;
}

/// Transactional producer of the turn finalization outbox messages. Every
/// method enqueues inside the caller's transaction and returns the outbox
/// [`Wake`] to fire after the commit.
#[async_trait]
pub trait TurnOutbox: Send + Sync {
    /// Enqueues the turn usage event (usage queue, partition: tenant).
    async fn enqueue_usage_in_tx(
        &self,
        tx: &DbTx<'_>,
        event: &UsageEvent,
    ) -> Result<Wake, DomainError>;

    /// Enqueues the turn audit event (audit queue, partition: tenant).
    async fn enqueue_audit_in_tx(
        &self,
        tx: &DbTx<'_>,
        event: &AuditEvent,
    ) -> Result<Wake, DomainError>;

    /// Enqueues a thread-summary work item (thread-summary queue, partition:
    /// chat).
    async fn enqueue_thread_summary_in_tx(
        &self,
        tx: &DbTx<'_>,
        task: &ThreadSummaryTask,
    ) -> Result<Wake, DomainError>;
}

/// Every persistence port, as handed to the domain services.
#[derive(Clone)]
pub struct Repositories {
    pub chats: Arc<dyn ChatRepository>,
    pub messages: Arc<dyn MessageRepository>,
    pub attachments: Arc<dyn AttachmentRepository>,
    pub reactions: Arc<dyn ReactionRepository>,
    pub turns: Arc<dyn TurnRepository>,
    pub outbox: Arc<dyn TurnOutbox>,
}

impl std::fmt::Debug for Repositories {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Repositories").finish_non_exhaustive()
    }
}

// ── Quota usage (DESIGN section 3.7 `quota_usage`) ───────────────────────

/// The `(tenant_id, user_id)` every `quota_usage` query is confined to.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct QuotaOwner {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
}

/// One bucket row of one period: the key of `uq_quota_usage_bucket` without
/// the tenant and user.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct QuotaBucketKey {
    pub period: crate::domain::quota::QuotaPeriod,
    pub period_start: time::Date,
    pub bucket: crate::domain::quota::QuotaBucket,
}

/// The counters of a `quota_usage` row the quota service reads.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaUsageRecord {
    pub key: QuotaBucketKey,
    pub spent_credits_micro: i64,
    pub reserved_credits_micro: i64,
    pub calls: i32,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub web_search_calls: i32,
    pub code_interpreter_calls: i32,
}

/// The settlement of one turn on one bucket row (section 3.7 "Commit
/// semantics"); `calls` is always incremented by 1.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BucketSettlement {
    pub key: QuotaBucketKey,
    /// The turn's reserve, subtracted from `reserved_credits_micro` (clamped
    /// at 0).
    pub release_reserved_credits_micro: i64,
    /// Committed credits, added to `spent_credits_micro`.
    pub spent_credits_micro: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub web_search_calls: u32,
    pub code_interpreter_calls: u32,
}

/// `quota_usage` persistence, always confined to one `(tenant_id, user_id)`.
///
/// The `*_in_tx` methods run inside a transaction owned by the caller (the
/// stream service's reserve or finalization transaction); a write method is
/// called before any read of the same transaction, so on `SQLite` the
/// transaction takes the write lock with its first statement.
#[async_trait]
pub trait QuotaRepository: Send + Sync {
    /// The existing rows of `keys`, read in one transaction of their own
    /// (`SELECT ... FOR UPDATE` on PostgreSQL when `for_update`). Missing
    /// rows are absent from the result.
    async fn read_buckets(
        &self,
        scope: &AccessScope,
        owner: QuotaOwner,
        keys: &[QuotaBucketKey],
        for_update: bool,
    ) -> Result<Vec<QuotaUsageRecord>, DomainError>;

    /// The existing rows of `keys`, read in `tx`.
    async fn read_buckets_in_tx(
        &self,
        tx: &toolkit_db::secure::DbTx<'_>,
        scope: &AccessScope,
        owner: QuotaOwner,
        keys: &[QuotaBucketKey],
    ) -> Result<Vec<QuotaUsageRecord>, DomainError>;

    /// Atomically adds `credits_micro` to `reserved_credits_micro` of every
    /// row of `keys`, creating missing rows (upsert on
    /// `uq_quota_usage_bucket`).
    async fn add_reserve_in_tx(
        &self,
        tx: &toolkit_db::secure::DbTx<'_>,
        scope: &AccessScope,
        owner: QuotaOwner,
        keys: &[QuotaBucketKey],
        credits_micro: i64,
        now: OffsetDateTime,
    ) -> Result<(), DomainError>;

    /// Applies every settlement atomically per row (`col = col + x`),
    /// creating missing rows.
    async fn settle_in_tx(
        &self,
        tx: &toolkit_db::secure::DbTx<'_>,
        scope: &AccessScope,
        owner: QuotaOwner,
        settlements: &[BucketSettlement],
        now: OffsetDateTime,
    ) -> Result<(), DomainError>;
}

// ── Attachment upload, read and deletion (DESIGN sections 3.3, 3.6, 3.7) ──

/// Machine-readable `attachments.error_code` values (DESIGN section 3.7
/// `attachments.error_code`).
pub mod attachment_error {
    /// The provider Files API upload failed.
    pub const UPLOAD_FAILED: &str = "upload_failed";
    /// The chat vector store could not be created or resolved.
    pub const VECTOR_STORE_FAILED: &str = "vector_store_failed";
    /// Adding the file to the vector store or indexing it failed.
    pub const INDEXING_FAILED: &str = "indexing_failed";
}

/// A full `attachments` row as the attachment service reads it.
/// `provider_file_id` and the secondary copy are internal only and never
/// leave the gear.
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent stored columns: purpose flags"
)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentRecord {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub uploaded_by_user_id: Uuid,
    pub filename: String,
    pub content_type: Option<String>,
    pub size_bytes: Option<i64>,
    /// Storage backend label the provider file was stored with.
    pub storage_backend: String,
    pub provider_file_id: Option<String>,
    pub status: AttachmentStatus,
    pub error_code: Option<String>,
    pub kind: AttachmentKind,
    pub for_file_search: bool,
    pub for_code_interpreter: bool,
    /// Present when the row stores a thumbnail with both dimensions.
    pub thumbnail: Option<StoredThumbnail>,
    /// `attachments.cleanup_status` as stored (`pending`, `done`, `failed`).
    pub cleanup_status: Option<String>,
    pub cleanup_attempts: i32,
    pub created_at: OffsetDateTime,
    pub deleted_at: Option<OffsetDateTime>,
    /// `attachments.secondary_file_id` (set when the secondary upload
    /// succeeded).
    pub secondary_file_id: Option<String>,
    /// `attachments.secondary_provider_kind` (`anthropic` or `None`).
    pub secondary_provider_kind: Option<String>,
    /// `attachments.secondary_provider_id`: the provider entry the copy was
    /// uploaded with (`None` on rows stored without it).
    pub secondary_provider_id: Option<String>,
}

/// The `pending` row of a new upload.
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent stored columns: purpose flags"
)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAttachment {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub uploaded_by_user_id: Uuid,
    pub filename: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub storage_backend: String,
    pub kind: AttachmentKind,
    pub for_file_search: bool,
    pub for_code_interpreter: bool,
    /// `created_at = updated_at`.
    pub now: OffsetDateTime,
}

/// Per-chat limits enforced when a pending row is inserted (DESIGN "RAG
/// Quality & Scale Controls"); both count the chat's non-deleted, non-failed
/// attachments including the new row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChatUploadLimits {
    /// `rag.max_documents_per_chat` (applies to document uploads only).
    pub max_documents: u32,
    /// `rag.max_total_upload_mb_per_chat` in bytes (images included).
    pub max_total_bytes: u64,
}

/// Outcome of a secondary (Anthropic Files API) upload of an image, stored in
/// `attachments.secondary_*`. Without an attempt the row keeps
/// `secondary_status = not_attempted`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecondaryCopy {
    /// `secondary_status = uploaded` with the copy's file id.
    Uploaded {
        file_id: String,
        provider_kind: String,
        /// Provider entry the copy was uploaded with.
        provider_id: String,
    },
    /// `secondary_status = failed`, no file id.
    Failed {
        provider_kind: String,
        provider_id: String,
    },
}

/// Outcome of the attachment deletion transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentDeletion {
    /// Soft-deleted now; the attachment cleanup message was enqueued.
    Deleted,
    /// Already soft-deleted (or no longer visible); nothing changed.
    AlreadyDeleted,
}

/// `attachments` persistence of the attachment service. Every method takes
/// the PEP scope of the parent chat (already loaded through the scoped chat
/// query) and filters on `chat_id`; the queries use the tenant predicates of
/// that scope.
#[async_trait]
pub trait AttachmentStore: Send + Sync {
    /// Inserts `attachment` as `pending` and checks `limits` in one
    /// transaction whose first statement is the insert.
    ///
    /// # Errors
    ///
    /// `429 document_limit` / `storage_limit` (the insert is rolled back);
    /// [`DomainError::Internal`] on a storage failure.
    async fn insert_pending(
        &self,
        scope: &AccessScope,
        attachment: NewAttachment,
        limits: ChatUploadLimits,
    ) -> Result<AttachmentRecord, DomainError>;

    /// The attachment `id` of `chat_id`, deleted or not.
    async fn find(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        id: Uuid,
    ) -> Result<Option<AttachmentRecord>, DomainError>;

    /// `pending -> uploaded` with `provider_file_id`; `false` when the row is
    /// no longer `pending`.
    async fn mark_uploaded(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        id: Uuid,
        provider_file_id: &str,
        at: OffsetDateTime,
    ) -> Result<bool, DomainError>;

    /// `uploaded -> ready` (with the image thumbnail), guarded by `status =
    /// uploaded`, `cleanup_status IS NULL` and `deleted_at IS NULL`; `false`
    /// when the guard does not hold.
    async fn mark_ready(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        id: Uuid,
        thumbnail: Option<StoredThumbnail>,
        at: OffsetDateTime,
    ) -> Result<bool, DomainError>;

    /// `pending | uploaded -> failed` with `error_code`; `false` when the row
    /// is in neither status.
    async fn mark_failed(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        id: Uuid,
        error_code: &str,
        at: OffsetDateTime,
    ) -> Result<bool, DomainError>;

    /// Refreshes `updated_at` of a row that is still `uploaded`, not deleted
    /// and not claimed by a cleanup (`cleanup_status IS NULL`); `false`
    /// otherwise (background indexing heartbeat).
    async fn touch_uploaded(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        id: Uuid,
        at: OffsetDateTime,
    ) -> Result<bool, DomainError>;

    /// The background indexing failure, in one transaction whose first
    /// statement is the guarded update (`uploaded`, not deleted,
    /// `cleanup_status IS NULL`): `failed`, `error_code = indexing_failed`,
    /// `cleanup_status = pending` and an attachment cleanup message
    /// (`attachment_indexing_failed`); `false` when the guard does not hold.
    async fn fail_indexing_with_cleanup(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        id: Uuid,
        at: OffsetDateTime,
    ) -> Result<bool, DomainError>;

    /// Records the secondary copy outcome while the row is not deleted and
    /// not claimed by a cleanup (`cleanup_status IS NULL`); `false` when the
    /// guard does not hold (the caller then owns the delete of the copy).
    async fn mark_secondary(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        id: Uuid,
        copy: SecondaryCopy,
        at: OffsetDateTime,
    ) -> Result<bool, DomainError>;

    /// The attachment deletion, in one transaction whose first statement
    /// locks the parent chat row without changing it (the row the turn
    /// transactions lock before they link attachments to a message),
    /// followed by the guarded soft delete (`deleted_at = at`,
    /// `cleanup_status = pending` when unset), the reference check and the
    /// `attachment_deleted` cleanup message. The message carries
    /// `secondary_ref` when the soft-deleted row (as returned by the update)
    /// has a secondary copy whose upstream alias `secondary_aliases`
    /// resolves from the stored provider entry (the kind only for rows
    /// without one).
    ///
    /// # Errors
    ///
    /// `404` (chat) when the chat is no longer visible or was soft-deleted;
    /// `409 attachment_locked` when any `message_attachments` row references
    /// the attachment (the transaction rolls back);
    /// [`DomainError::Internal`] on a storage or outbox failure.
    async fn soft_delete(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        id: Uuid,
        at: OffsetDateTime,
        secondary_aliases: Option<Arc<dyn SecondaryAliasResolver>>,
    ) -> Result<AttachmentDeletion, DomainError>;
}

/// A `chat_vector_stores` row (DESIGN section 3.7 `chat_vector_stores`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorStoreRecord {
    /// Row id (the compare-and-set key of the creator).
    pub id: Uuid,
    /// `None` while the provider store is being created.
    pub vector_store_id: Option<String>,
    /// Storage backend label of the RAG provider.
    pub provider: String,
    pub created_at: OffsetDateTime,
}

/// The placeholder row of a vector store creation (`vector_store_id = NULL`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewVectorStorePlaceholder {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub provider: String,
    pub now: OffsetDateTime,
}

/// `chat_vector_stores` persistence: always filtered on the tenant of the
/// chat scope and the parent `chat_id`, never by row id or provider id alone.
#[async_trait]
pub trait VectorStoreRepository: Send + Sync {
    /// The row of `chat_id`, if any.
    async fn find(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<VectorStoreRecord>, DomainError>;

    /// Inserts the placeholder (auto-committed); `false` when the chat
    /// already has a row (unique violation on `(tenant_id, chat_id)`).
    async fn insert_placeholder(
        &self,
        scope: &AccessScope,
        placeholder: NewVectorStorePlaceholder,
    ) -> Result<bool, DomainError>;

    /// `SET vector_store_id = :id WHERE id = :row_id AND vector_store_id IS
    /// NULL`; `false` when the placeholder is gone or already set.
    async fn set_vector_store_id(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        row_id: Uuid,
        vector_store_id: &str,
    ) -> Result<bool, DomainError>;

    /// Deletes the placeholder `row_id` while its `vector_store_id` is still
    /// NULL; `false` when nothing was deleted.
    async fn delete_placeholder(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        row_id: Uuid,
    ) -> Result<bool, DomainError>;
}

// ── Provider storage (DESIGN sections 3.5 "LLM Provider", 3.6 "File Upload") ──

/// Where a file or vector-store operation goes: the storage backend label
/// (`attachments.storage_backend`, `chat_vector_stores.provider`) and the
/// tenant whose upstream alias is used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageTarget {
    pub backend: String,
    pub tenant_id: Uuid,
}

/// Failure of a provider storage call. Messages are diagnostics only: they
/// are logged or stored internally (`last_cleanup_error`), never returned to
/// clients.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StorageError {
    /// Gateway failure, provider 5xx, 408 or 429, or no S2S context: the same
    /// call may succeed later.
    #[error("transient storage failure: {0}")]
    Transient(String),
    /// The provider rejected the call (any other non-2xx status) or answered
    /// with an unusable body.
    #[error("storage call rejected: {0}")]
    Rejected(String),
}

impl StorageError {
    /// Whether a retry of the same call may succeed.
    #[must_use]
    pub const fn is_transient(&self) -> bool {
        matches!(self, Self::Transient(_))
    }
}

/// A file to store with the provider Files API (`purpose = assistants`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileUpload {
    /// Provider-side filename (`{chat_id}_{attachment_id}.{ext}`).
    pub filename: String,
    pub content_type: String,
    pub data: bytes::Bytes,
}

/// Provider Files API.
#[async_trait]
pub trait FileStorage: Send + Sync {
    /// Storage backend label of chats whose model is served by
    /// `provider_id`: the label of its `rag_provider` entry, or of the entry
    /// itself.
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`] when the provider (or its `rag_provider`) is
    /// not configured.
    fn backend_for_provider(&self, provider_id: &str) -> Result<String, DomainError>;

    /// Uploads `file`; returns the provider file id.
    async fn upload_file(
        &self,
        target: &StorageTarget,
        file: FileUpload,
    ) -> Result<String, StorageError>;

    /// Deletes the provider file `file_id`; a 2xx or `404 Not Found` is
    /// success.
    async fn delete_file(&self, target: &StorageTarget, file_id: &str) -> Result<(), StorageError>;
}

/// Indexing status of a vector store file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VectorStoreFileStatus {
    /// `in_progress`, or a response without `status`.
    InProgress,
    Completed,
    Failed,
    Cancelled,
    /// Any other value (counts as failed).
    Other(String),
}

/// Provider Vector Stores API.
#[async_trait]
pub trait VectorStores: Send + Sync {
    /// Creates the vector store of `chat_id`; returns its provider id.
    async fn create_vector_store(
        &self,
        target: &StorageTarget,
        chat_id: Uuid,
    ) -> Result<String, StorageError>;

    /// Deletes the vector store; a 2xx or `404 Not Found` is success.
    async fn delete_vector_store(
        &self,
        target: &StorageTarget,
        vector_store_id: &str,
    ) -> Result<(), StorageError>;

    /// Adds `file_id` with the single attribute `{attachment_id}`; returns
    /// the indexing status of the response.
    async fn add_file(
        &self,
        target: &StorageTarget,
        vector_store_id: &str,
        file_id: &str,
        attachment_id: Uuid,
    ) -> Result<VectorStoreFileStatus, StorageError>;

    /// The current indexing status of `file_id`.
    async fn file_status(
        &self,
        target: &StorageTarget,
        vector_store_id: &str,
        file_id: &str,
    ) -> Result<VectorStoreFileStatus, StorageError>;
}

/// `secondary_provider_kind` of an Anthropic Files API copy.
pub const SECONDARY_KIND_ANTHROPIC: &str = "anthropic";

/// Where the secondary copy of an upload goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecondaryTarget {
    /// Provider entry the copy is stored with.
    pub provider_id: String,
    /// `attachments.secondary_provider_kind` (`anthropic`).
    pub provider_kind: String,
    /// OAGW upstream alias for the tenant.
    pub upstream_alias: String,
}

/// Resolves the upstream alias of a stored secondary copy at delete time.
pub trait SecondaryAliasResolver: Send + Sync {
    /// The upstream alias for `tenant_id` of a copy of `provider_kind`
    /// uploaded with the provider entry `provider_id`
    /// (`attachments.secondary_provider_id`). Without a stored entry the
    /// alias is resolved by kind alone (ambiguous when several entries
    /// share it). `None` when it cannot be resolved (the delete is skipped).
    fn upstream_alias(
        &self,
        tenant_id: Uuid,
        provider_kind: &str,
        provider_id: Option<&str>,
    ) -> Option<String>;
}

/// Secondary copies of uploaded images (DESIGN section 3.2 `llm_provider`,
/// section 3.6 "File Upload", ADR-0005): chats whose model is served by an
/// `anthropic_messages` provider keep a copy of each image in the Anthropic
/// Files API.
#[async_trait]
pub trait SecondaryFileUploader: SecondaryFileStorage + SecondaryAliasResolver {
    /// The copy target for an upload of `tenant_id` into a chat whose model
    /// is served by `provider_id`; `None` when the provider needs no copy.
    fn secondary_target(&self, provider_id: &str, tenant_id: Uuid) -> Option<SecondaryTarget>;

    /// Uploads `file` (only the `file` part, no `purpose`); returns the file
    /// id of the copy.
    async fn upload_secondary_file(
        &self,
        target: &SecondaryTarget,
        file: FileUpload,
    ) -> Result<String, StorageError>;
}

/// Secondary provider copy of an attachment (Anthropic Files API); the
/// attachment and chat cleanups delete it best effort.
#[async_trait]
pub trait SecondaryFileStorage: Send + Sync {
    /// Deletes the secondary file; a 2xx or `404 Not Found` is success.
    async fn delete_secondary_file(
        &self,
        tenant_id: Uuid,
        file_id: &str,
        provider_kind: &str,
        upstream_alias: &str,
    ) -> Result<(), StorageError>;
}

/// Preview thumbnail generation of image uploads (DESIGN section 3.6
/// "Thumbnail generation details"). `None` skips the thumbnail.
pub trait ThumbnailRenderer: Send + Sync {
    /// The WebP thumbnail of `data`, or `None` when it is skipped or fails.
    fn render(&self, data: &[u8]) -> Option<StoredThumbnail>;
}

// ── Thread summary execution (DESIGN section 3.6 "Thread Summary Update") ──

/// The CAS-protected commit of one thread-summary work item ("Stable Range
/// and Commit Invariant").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadSummaryCommit {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    /// Id of the `thread_summaries` row when the commit inserts it.
    pub summary_id: Uuid,
    /// The frontier the work item is based on (`None`: no summary yet).
    pub base_frontier: Option<MessageKey>,
    /// The frozen target frontier, the new stored frontier.
    pub target: MessageKey,
    pub summary_text: String,
    pub token_estimate: i32,
    /// The system usage event enqueued with the commit (usage queue).
    pub usage_event: UsageEvent,
    /// `created_at` / `updated_at` of the summary row.
    pub at: OffsetDateTime,
}

/// Outcome of a [`ThreadSummaryCommit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadSummaryCommitOutcome {
    /// Summary stored, frontier advanced, range compressed, usage enqueued.
    Committed,
    /// The target frontier message is soft-deleted: nothing was written.
    FrontierDeleted,
    /// The stored frontier no longer equals the base: nothing was written.
    CasConflict,
    /// The base summary row no longer exists: nothing was written.
    BaseMissing,
}

/// Persistence of the thread-summary handler. Every method takes the tenant
/// scope of the work item's chat and filters on `chat_id`.
#[async_trait]
pub trait ThreadSummaryStore: Send + Sync {
    /// The chat's committed thread summary, if any.
    async fn current_summary(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<ThreadSummaryRecord>, DomainError>;

    /// The non-deleted, non-compressed messages of `chat_id` whose order key
    /// is in `(base, target]` (`base = None`: no lower bound), ordered by
    /// `(created_at, id)` ascending.
    async fn summary_range(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        base: Option<MessageKey>,
        target: MessageKey,
    ) -> Result<Vec<ContextMessageRecord>, DomainError>;

    /// Applies `commit` in one transaction whose first statement locks the
    /// live target frontier message (a no-op `UPDATE`, the row lock on
    /// PostgreSQL), then the CAS upsert of `thread_summaries`, the
    /// `is_compressed` mark of exactly `(base, target]` and the usage event.
    async fn commit_summary(
        &self,
        scope: &AccessScope,
        commit: ThreadSummaryCommit,
    ) -> Result<ThreadSummaryCommitOutcome, DomainError>;
}

// ── Background workers (DESIGN section 4 "Orphan Turn Watchdog", B.9) ────

/// `attachments.error_code` of an upload failed by the upload reaper (B.9.5).
pub const UPLOAD_ABANDONED: &str = "upload_abandoned";

/// A `running` turn matched by the stale-progress scan of the orphan
/// watchdog. Discovery is advisory only: the orphan finalization CAS
/// re-checks every predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrphanCandidate {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
}

/// The orphan finalization CAS of one candidate: `running -> failed` with
/// `error_code = orphan_timeout`, `completed_at = updated_at = at`, guarded
/// by `state = 'running'`, `deleted_at IS NULL` and the stale-progress
/// predicate against `cutoff` (application clock).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrphanFinalization {
    pub turn_id: Uuid,
    pub tenant_id: Uuid,
    /// `last_progress_at <= cutoff OR (last_progress_at IS NULL AND
    /// started_at <= cutoff)`.
    pub cutoff: OffsetDateTime,
    /// `completed_at = updated_at`.
    pub at: OffsetDateTime,
}

/// `chat_turns` persistence of the orphan watchdog. The scan crosses
/// tenants (system worker); the CAS and the owner read are confined to the
/// candidate's tenant.
#[async_trait]
pub trait OrphanTurnRepository: Send + Sync {
    /// At most `limit` running, non-deleted turns whose `last_progress_at`
    /// (or `started_at` when NULL) is `<= cutoff`, oldest start first.
    async fn orphan_candidates(
        &self,
        cutoff: OffsetDateTime,
        limit: u64,
    ) -> Result<Vec<OrphanCandidate>, DomainError>;

    /// The orphan finalization CAS inside `tx`, issued as the first
    /// statement of the transaction. Returns the finalized row, or `None`
    /// when the turn is no longer orphan-finalizable (already terminal,
    /// soft-deleted or progressed after the scan).
    async fn finalize_orphan_in_tx(
        &self,
        tx: &DbTx<'_>,
        finalization: OrphanFinalization,
    ) -> Result<Option<FinalizedTurnRecord>, DomainError>;

    /// Owner (`chats.user_id`) of the chat `chat_id` of `tenant_id`, deleted
    /// or not, read in `tx`.
    async fn chat_owner_in_tx(
        &self,
        tx: &DbTx<'_>,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Option<Uuid>, DomainError>;
}
