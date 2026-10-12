//! The repository port of the turn mutations.

use async_trait::async_trait;
use time::OffsetDateTime;
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::model::{DeleteCommand, OriginalMessage, ReplaceCommand};
use crate::domain::error::DomainError;
use crate::domain::stream::MutationReserveCommand;

/// Persistence of turn mutations. Every method applies the caller's scope.
#[async_trait]
pub trait MutationRepository: Send + Sync {
    /// The non-deleted turn of the chat with the greatest `(started_at, id)`.
    async fn latest_turn_id(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<Uuid>, DomainError>;

    /// The user message of the turn with its live attachments, soft-deleted messages included;
    /// `None` when the turn has no user message.
    async fn original_message(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<OriginalMessage>, DomainError>;

    /// The mutation commit of a retry or an edit, one transaction: re-validation of the target,
    /// soft delete of the turn and its messages, the new user message with the copied
    /// attachment links, the new `running` turn (preflight columns NULL), the summary
    /// invalidation, the chat bump and the mutation audit event.
    ///
    /// # Errors
    /// `NotLatestTurn`, `InvalidTurnState`, `TurnNotFound` or `AuthzDenied` when the target no
    /// longer qualifies, `GenerationInProgress` when the insert loses the race on the
    /// one-running-turn index; any failure rolls everything back.
    async fn replace(
        &self,
        scope: &AccessScope,
        command: ReplaceCommand,
    ) -> Result<(), DomainError>;

    /// The mutation commit of a delete, one transaction: re-validation, soft delete of the turn
    /// and its messages, the summary invalidation and the mutation audit event.
    ///
    /// # Errors
    /// As for [`Self::replace`] except `GenerationInProgress`.
    async fn delete(&self, scope: &AccessScope, command: DeleteCommand) -> Result<(), DomainError>;

    /// The reserve of a retry/edit turn: books the quota reserve with the limit re-check and
    /// fills the preflight columns of the turn, in one transaction.
    ///
    /// # Errors
    /// `QuotaExceeded` when the re-check fails; any other failure is an internal error.
    async fn reserve(
        &self,
        scope: &AccessScope,
        command: MutationReserveCommand,
    ) -> Result<(), DomainError>;

    /// Moves an unstarted retry/edit turn to `failed` with a plain CAS (no settlement, no outbox
    /// event). `false` when the turn is no longer `running` or already holds a reserve (an
    /// ambiguous reserve commit): that turn stays `running` for the normal finalization or the
    /// orphan watchdog, which settle a filled reserve.
    async fn fail_unstarted(
        &self,
        scope: &AccessScope,
        turn_id: Uuid,
        error_code: &str,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError>;
}
