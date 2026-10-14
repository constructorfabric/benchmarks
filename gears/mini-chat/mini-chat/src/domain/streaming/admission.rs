//! Turn admission checks (`cpt-cf-mini-chat-algo-message-streaming-turn-admission-checks`):
//! idempotency first, then the parallel-turn guard.

use toolkit_macros::domain_model;

use crate::domain::error::{ConflictReason, DomainError};
use crate::domain::turns::{TurnRow, TurnState};

/// What the admission checks decided.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmissionDecision {
    /// A completed, non-deleted turn of the supplied `request_id`: serve the replay and run no
    /// further check.
    Replay(Box<TurnRow>),
    /// A new turn may be started (a loser of the insert race is resolved by the reserve
    /// transaction).
    Proceed,
}

/// The admission decision of one request.
///
/// `supplied` is whether the client supplied the `request_id` (a server-generated one cannot
/// collide, so the key lookup result `existing` is ignored then); `existing` is the
/// `(chat_id, request_id)` row including soft-deleted ones; `running_exists` is whether the chat
/// has any `running` non-deleted turn, whatever its `request_id`.
///
/// # Errors
///
/// `Conflict { RequestIdConflict }` for a soft-deleted row (any state) and for a `running`,
/// `failed` or `cancelled` row; `Conflict { TurnAlreadyRunning }` when no row matched the key and
/// another turn is running. The replay of a completed turn is decided before the running check.
pub fn decide_admission(
    supplied: bool,
    existing: Option<&TurnRow>,
    running_exists: bool,
) -> Result<AdmissionDecision, DomainError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-turn-admission-checks:p1:inst-adm-supplied
    // a server-generated key cannot collide, so the key lookup result matters only when the
    // client supplied the `request_id`
    if supplied && let Some(row) = existing {
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-turn-admission-checks:p1:inst-adm-match
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-turn-admission-checks:p1:inst-adm-case-replay
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-turn-admission-checks:p1:inst-adm-case-deleted
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-turn-admission-checks:p1:inst-adm-case-running
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-turn-admission-checks:p1:inst-adm-case-terminal
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-turn-admission-checks:p1:inst-adm-order
        // the replay of a completed, non-deleted turn is decided first and runs no further
        // check, so it is served while another turn of the chat is running; every other reuse
        // of the key (a soft-deleted row of any state, a running, failed or cancelled turn) is
        // a `request_id_conflict`
        return match (row.state, row.deleted_at) {
            (TurnState::Completed, None) => Ok(AdmissionDecision::Replay(Box::new(row.clone()))),
            _ => Err(DomainError::Conflict {
                reason: ConflictReason::RequestIdConflict,
            }),
        };
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-turn-admission-checks:p1:inst-adm-order
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-turn-admission-checks:p1:inst-adm-case-terminal
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-turn-admission-checks:p1:inst-adm-case-running
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-turn-admission-checks:p1:inst-adm-case-deleted
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-turn-admission-checks:p1:inst-adm-case-replay
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-turn-admission-checks:p1:inst-adm-match
    }
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-turn-admission-checks:p1:inst-adm-supplied
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-turn-admission-checks:p1:inst-adm-case-none
    // no row for the key: continue with the parallel-turn guard
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-turn-admission-checks:p1:inst-adm-case-none
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-turn-admission-checks:p1:inst-adm-running-found
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-turn-admission-checks:p1:inst-adm-return-running
    if running_exists {
        return Err(DomainError::Conflict {
            reason: ConflictReason::TurnAlreadyRunning,
        });
    }
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-turn-admission-checks:p1:inst-adm-return-running
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-turn-admission-checks:p1:inst-adm-running-found
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-turn-admission-checks:p1:inst-adm-return-proceed
    Ok(AdmissionDecision::Proceed)
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-turn-admission-checks:p1:inst-adm-return-proceed
}
