//! Idempotency and the parallel-turn guard
//! (`cpt-cf-mini-chat-algo-streaming-turns-resolve-idempotency`).

use toolkit_security::AccessScope;
use uuid::Uuid;

use super::ports::TurnRepository;
use super::types::{TurnRecord, TurnState};
use crate::domain::error::DomainError;

/// The outcome of the idempotency check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdempotencyOutcome {
    /// A completed, non-deleted turn with this request id: serve the replay.
    Replay(TurnRecord),
    /// The request id belongs to a running, failed, cancelled or soft-deleted
    /// turn: 409 `request_id_conflict`.
    RequestIdConflict,
    /// Another turn of the chat is running: 409 `turn_already_running`.
    TurnAlreadyRunning,
    /// Nothing in the way; the database stays the final arbiter.
    Proceed,
}

// @cpt-dod:cpt-cf-mini-chat-dod-streaming-turns-idempotency:p1
/// Resolves `(chat_id, request_id)` before the parallel-turn guard: replay is
/// checked first, so a completed turn replays while another turn runs.
///
/// # Errors
///
/// The errors of the repository.
pub async fn resolve_idempotency(
    repo: &dyn TurnRepository,
    scope: &AccessScope,
    chat_id: Uuid,
    request_id: Uuid,
) -> Result<IdempotencyOutcome, DomainError> {
    if let Some(known) = key_outcome(repo, scope, chat_id, request_id).await? {
        return Ok(known);
    }
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-resolve-idempotency:p1:inst-if-running
    // @cpt-begin:cpt-cf-mini-chat-flow-background-workers-abandoned-turn-recovery:p1:inst-send-after-recovery
    if repo.find_running(scope, chat_id).await?.is_some() {
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-resolve-idempotency:p1:inst-return-running
        return Ok(IdempotencyOutcome::TurnAlreadyRunning);
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-resolve-idempotency:p1:inst-return-running
    }
    // @cpt-end:cpt-cf-mini-chat-flow-background-workers-abandoned-turn-recovery:p1:inst-send-after-recovery
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-resolve-idempotency:p1:inst-if-running
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-resolve-idempotency:p1:inst-return-proceed
    Ok(IdempotencyOutcome::Proceed)
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-resolve-idempotency:p1:inst-return-proceed
}

/// The outcome for an existing `(chat_id, request_id)` row, if any.
async fn key_outcome(
    repo: &dyn TurnRepository,
    scope: &AccessScope,
    chat_id: Uuid,
    request_id: Uuid,
) -> Result<Option<IdempotencyOutcome>, DomainError> {
    Ok(repo
        .find_by_request_id(scope, chat_id, request_id)
        .await?
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-resolve-idempotency:p1:inst-if-key-exists
        .map(|turn| {
            // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-resolve-idempotency:p1:inst-if-completed
            if turn.state == TurnState::Completed && !turn.deleted {
                // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-resolve-idempotency:p1:inst-return-replay
                IdempotencyOutcome::Replay(turn)
                // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-resolve-idempotency:p1:inst-return-replay
            } else {
                // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-resolve-idempotency:p1:inst-return-key-conflict
                IdempotencyOutcome::RequestIdConflict
                // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-resolve-idempotency:p1:inst-return-key-conflict
            }
            // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-resolve-idempotency:p1:inst-if-completed
        }))
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-resolve-idempotency:p1:inst-if-key-exists
}

/// Resolves a unique violation of the reserve transaction after the rollback:
/// re-reads `(chat_id, request_id)` (soft-deleted rows included) and applies
/// the idempotency outcome, then looks for a running turn; the violation with
/// neither is an internal error.
///
/// # Errors
///
/// [`DomainError::Internal`] when no conflicting row is found, and the errors
/// of the repository.
pub async fn resolve_unique_violation(
    repo: &dyn TurnRepository,
    scope: &AccessScope,
    chat_id: Uuid,
    request_id: Uuid,
) -> Result<IdempotencyOutcome, DomainError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-create-turn:p1:inst-reread-key
    if let Some(known) = key_outcome(repo, scope, chat_id, request_id).await? {
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-create-turn:p1:inst-return-key-race
        return Ok(known);
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-create-turn:p1:inst-return-key-race
    }
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-create-turn:p1:inst-reread-key
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-create-turn:p1:inst-if-running-race
    if repo.find_running(scope, chat_id).await?.is_some() {
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-create-turn:p1:inst-return-running-race
        return Ok(IdempotencyOutcome::TurnAlreadyRunning);
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-create-turn:p1:inst-return-running-race
    }
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-create-turn:p1:inst-if-running-race
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-create-turn:p1:inst-return-unique-internal
    tracing::error!(
        %chat_id,
        %request_id,
        "a unique violation without a conflicting turn row"
    );
    Err(DomainError::Internal(
        "unique violation without a conflicting row".to_owned(),
    ))
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-create-turn:p1:inst-return-unique-internal
}

#[cfg(test)]
#[path = "idempotency_tests.rs"]
mod tests;
