//! The pure rules of the mutation preview and of the lock re-check: the check order on the target
//! turn and the latest-turn selection (`cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preview`,
//! `...-latest-turn`, `...-mutation-lock-recheck`).

use uuid::Uuid;

use crate::domain::error::{ConflictReason, DomainError, ResourceKind};
use crate::domain::turns::{TurnRow, TurnState};

/// The checks on the turn found for the path `request_id`, in the normative order: missing (404
/// of the turn type), requester other than the caller or NULL (403 `AUTHZ_DENIED`), soft-deleted
/// (409 `NOT_LATEST_TURN`), `running` (400 `turn_state`).
///
/// # Errors
///
/// `NotFound { Turn }`, `AuthzDenied`, `Conflict { NotLatestTurn }` or `TurnNotTerminal`.
pub fn check_target(caller: Uuid, target: Option<&TurnRow>) -> Result<(), DomainError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preview:p1:inst-mp-missing
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preview:p1:inst-mp-return-404
    let Some(row) = target else {
        return Err(DomainError::NotFound {
            resource: ResourceKind::Turn,
            id: None,
        });
    };
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preview:p1:inst-mp-return-404
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preview:p1:inst-mp-missing
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preview:p1:inst-mp-not-owner
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preview:p1:inst-mp-return-403
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-error-contract:p1:inst-ec-requester
    if row.requester_user_id != Some(caller) {
        return Err(DomainError::AuthzDenied);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-error-contract:p1:inst-ec-requester
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preview:p1:inst-mp-return-403
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preview:p1:inst-mp-not-owner
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preview:p1:inst-mp-deleted
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preview:p1:inst-mp-return-deleted
    // @cpt-begin:cpt-cf-mini-chat-state-turn-mutations-and-reactions-turn-visibility:p1:inst-tv-final
    if row.deleted_at.is_some() {
        return Err(DomainError::Conflict {
            reason: ConflictReason::NotLatestTurn,
        });
    }
    // @cpt-end:cpt-cf-mini-chat-state-turn-mutations-and-reactions-turn-visibility:p1:inst-tv-final
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preview:p1:inst-mp-return-deleted
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preview:p1:inst-mp-deleted
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preview:p1:inst-mp-running
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preview:p1:inst-mp-return-running
    if row.state == TurnState::Running {
        return Err(DomainError::TurnNotTerminal);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preview:p1:inst-mp-return-running
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preview:p1:inst-mp-running
    Ok(())
}

/// The latest turn among `turns`: the non-deleted turn with the greatest `(started_at, id)`, any
/// state counted; `None` without a non-deleted turn.
#[must_use]
pub fn select_latest(turns: &[TurnRow]) -> Option<&TurnRow> {
    turns
        .iter()
        .filter(|turn| turn.deleted_at.is_none())
        .max_by_key(|turn| (turn.started_at, turn.id))
}

/// The latest-turn check: `Ok` when `latest` is the target.
///
/// # Errors
///
/// `Conflict { NotLatestTurn }` when another turn (or none) is the latest.
pub fn check_latest(target: &TurnRow, latest: Option<&TurnRow>) -> Result<(), DomainError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preview:p1:inst-mp-not-latest
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preview:p1:inst-mp-return-not-latest
    match latest {
        Some(latest) if latest.id == target.id => Ok(()),
        _ => Err(DomainError::Conflict {
            reason: ConflictReason::NotLatestTurn,
        }),
    }
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preview:p1:inst-mp-return-not-latest
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preview:p1:inst-mp-not-latest
}
