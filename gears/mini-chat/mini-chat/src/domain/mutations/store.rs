//! The persistence ports of the turn mutations and of the reactions, implemented by the
//! infrastructure layer over the database provider (`infra::storage::mutation_store`).

use std::sync::Arc;

use async_trait::async_trait;
use time::OffsetDateTime;
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::messages::{MessageRow, Reaction};
use crate::domain::mutations::carry::LinkedAttachment;
use crate::domain::mutations::reactions::StoredReaction;
use crate::domain::mutations::{CommittedMutation, MutationOp};
use crate::domain::quota::{PreflightDecision, QuotaService, ReserveReceipt};
use crate::domain::streaming::{SnapshotBoundary, StreamStore, TurnOutbox};
use crate::domain::turns::TurnRow;

/// The target turn of a mutation: the turn row and the flag the replacement turn reuses.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationTurn {
    pub row: TurnRow,
    /// `chat_turns.web_search_enabled` of the target.
    pub web_search_enabled: bool,
}

/// The reads of the mutation preflight (read-only, one scoped chat).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationReads {
    /// The latest non-deleted message of the chat whose `request_id` differs from the target's;
    /// `None` when the target is the only turn.
    pub boundary: Option<SnapshotBoundary>,
    /// The attachments linked to the target's user message, deleted ones flagged.
    pub linked: Vec<LinkedAttachment>,
    /// Ready, non-deleted document attachments of the chat.
    pub ready_documents: u32,
    /// Ready, non-deleted attachments of the chat with `for_code_interpreter`.
    pub ready_code_interpreter: u32,
}

/// The replacement turn of a retry or edit.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct Replacement {
    /// The server-generated UUID v4 `request_id`.
    pub request_id: Uuid,
    pub turn_id: Uuid,
    pub user_message_id: Uuid,
    /// The original content (retry) or the validated content (edit).
    pub content: String,
    /// The carried attachments, in order.
    pub attachment_ids: Vec<Uuid>,
    pub web_search_enabled: bool,
    /// The preflight instant of the mutation preflight decision.
    pub started_at: OffsetDateTime,
}

/// The input of the mutation commit transaction.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct CommitCommand {
    pub tenant_id: Uuid,
    /// The caller: the requester of the replacement turn and the audit actor.
    pub user_id: Uuid,
    pub chat_id: Uuid,
    pub op: MutationOp,
    pub target_request_id: Uuid,
    /// `None` for a delete.
    pub replacement: Option<Replacement>,
    /// The application clock reading of the commit.
    pub now: OffsetDateTime,
}

/// The input of the reserve transaction of a replacement turn (`ReserveTurn::Existing`).
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct ExistingReserve {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub chat_id: Uuid,
    pub request_id: Uuid,
    pub turn_id: Uuid,
    pub decision: PreflightDecision,
}

/// The codes of an unstarted replacement turn failure.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnstartedFailure {
    ContextLengthExceeded,
    TurnSetupFailed,
    QuotaExceeded,
}

impl UnstartedFailure {
    /// The stored `error_code`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ContextLengthExceeded => "context_length_exceeded",
            Self::TurnSetupFailed => "turn_setup_failed",
            Self::QuotaExceeded => "quota_exceeded",
        }
    }
}

/// The persistence of the turn mutations. The chat load and the send-setup reads come from the
/// [`StreamStore`] supertrait.
#[async_trait]
pub trait MutationStore: StreamStore {
    /// The `(chat_id, request_id)` turn of the tenant, soft-deleted rows included.
    ///
    /// # Errors
    ///
    /// `Internal` on a database failure.
    async fn find_mutation_turn(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<MutationTurn>, DomainError>;

    /// The latest turn of the chat (`deleted_at IS NULL`, greatest `(started_at, id)`, any state;
    /// the fixed-width sort key on `SQLite`).
    ///
    /// # Errors
    ///
    /// `Internal` on a database failure.
    async fn latest_turn(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Option<TurnRow>, DomainError>;

    /// The non-deleted user message of the turn `request_id`.
    ///
    /// # Errors
    ///
    /// `Internal` on a database failure.
    async fn target_user_message(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<MessageRow>, DomainError>;

    /// The reads of the mutation preflight for the target turn `request_id`.
    ///
    /// # Errors
    ///
    /// `Internal` on a database failure.
    async fn read_mutation(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        target_request_id: Uuid,
    ) -> Result<MutationReads, DomainError>;

    /// The mutation commit transaction: lock and re-check, the guarded soft-delete of the turn
    /// and its messages, the replacement message, links and `running` turn (retry and edit), the
    /// activity bump (retry and edit), the summary invalidation and the audit enqueue through
    /// `outbox`; one transaction, committed or rolled back as a whole. The outbox wake fires
    /// after the commit.
    ///
    /// # Errors
    ///
    /// `Conflict { NotLatestTurn }`, `Conflict { GenerationInProgress }`, `TurnNotTerminal`,
    /// `OutboxPayloadTooLarge` and `Internal`; nothing is left behind.
    async fn commit_mutation(
        &self,
        outbox: Arc<dyn TurnOutbox>,
        command: &CommitCommand,
    ) -> Result<CommittedMutation, DomainError>;

    /// The reserve transaction of a replacement turn: `QuotaService::write_reserve` with
    /// `ReserveTurn::Existing`; one transaction.
    ///
    /// # Errors
    ///
    /// `QuotaExceeded { Tokens }` for the re-check, `Internal` otherwise; nothing is left behind.
    async fn reserve_existing(
        &self,
        quota: &QuotaService,
        command: &ExistingReserve,
    ) -> Result<ReserveReceipt, DomainError>;

    /// The unstarted replacement turn failure: a plain CAS `running` to `failed` with the code, a
    /// sanitized detail and `completed_at`; no settlement, no usage event, no audit event.
    /// Returns whether this call won the CAS.
    ///
    /// # Errors
    ///
    /// `Internal` on a database failure.
    async fn fail_unstarted(
        &self,
        tenant_id: Uuid,
        turn_id: Uuid,
        code: UnstartedFailure,
    ) -> Result<bool, DomainError>;
}

/// The persistence of the reactions. The chat load and `load_message` (non-deleted messages of
/// the chat) come from the [`StreamStore`] supertrait.
#[async_trait]
pub trait ReactionStore: StreamStore {
    /// Upsert the reaction of `user_id` on `message_id` (`tenant_id` and `user_id` from the
    /// security context): a new row, or `reaction` and `created_at` replaced on the
    /// `(message_id, user_id)` conflict, in one statement.
    ///
    /// # Errors
    ///
    /// `Internal` on a database failure.
    async fn upsert_reaction(
        &self,
        tenant_id: Uuid,
        user_id: Uuid,
        message_id: Uuid,
        reaction: Reaction,
        now: OffsetDateTime,
    ) -> Result<StoredReaction, DomainError>;

    /// Delete the reaction of `user_id` on `message_id`; zero rows is a success.
    ///
    /// # Errors
    ///
    /// `Internal` on a database failure.
    async fn remove_reaction(
        &self,
        tenant_id: Uuid,
        user_id: Uuid,
        message_id: Uuid,
    ) -> Result<(), DomainError>;
}
