//! The read-only mutation preview
//! (`cpt-cf-mini-chat-algo-turn-mutations-preview`).

use toolkit_security::AccessScope;
use uuid::Uuid;

use super::ports::MutationRepository;
use super::types::MutationTarget;
use crate::domain::error::{DomainError, ResourceKind};
use crate::domain::service::stream::types::TurnState;

/// Loads the target turn and its live user message and decides whether it can
/// be mutated: it must exist, belong to the caller, be live, be terminal and
/// be the latest live turn of the chat.
///
/// # Errors
///
/// `NotFound { Turn }`, `AuthzDenied` (a foreign requester), `NotLatestTurn`
/// (soft-deleted target, or a newer live turn), `TurnNotTerminal` (a running
/// target, checked before the latest-turn check), `Internal` (a turn without
/// its user message), `Database`.
// @cpt-dod:cpt-cf-mini-chat-dod-turn-mutations-eligibility:p1
pub async fn preview(
    repo: &dyn MutationRepository,
    scope: &AccessScope,
    chat_id: Uuid,
    caller: Uuid,
    request_id: Uuid,
) -> Result<MutationTarget, DomainError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-preview:p1:inst-preview-read-only
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-preview:p1:inst-if-target-missing
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-preview:p1:inst-return-target-404
    let target =
        repo.find_target(scope, chat_id, request_id)
            .await?
            .ok_or(DomainError::NotFound {
                resource: ResourceKind::Turn,
            })?;
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-preview:p1:inst-return-target-404
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-preview:p1:inst-if-target-missing
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-preview:p1:inst-if-foreign-requester
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-preview:p1:inst-return-foreign-403
    if target.turn.requester_user_id != Some(caller) {
        return Err(DomainError::AuthzDenied);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-preview:p1:inst-return-foreign-403
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-preview:p1:inst-if-foreign-requester
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-preview:p1:inst-if-target-deleted
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-preview:p1:inst-return-deleted-not-latest
    // @cpt-begin:cpt-cf-mini-chat-state-turn-mutations-turn-visibility:p1:inst-soft-deleted-final
    if target.turn.deleted {
        return Err(DomainError::NotLatestTurn);
    }
    // @cpt-end:cpt-cf-mini-chat-state-turn-mutations-turn-visibility:p1:inst-soft-deleted-final
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-preview:p1:inst-return-deleted-not-latest
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-preview:p1:inst-if-target-deleted
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-preview:p1:inst-if-target-running
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-preview:p1:inst-return-not-terminal
    if target.turn.state == TurnState::Running {
        return Err(DomainError::TurnNotTerminal);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-preview:p1:inst-return-not-terminal
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-preview:p1:inst-if-target-running
    let latest = repo.latest_turn(scope, chat_id).await?;
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-preview:p1:inst-if-not-latest
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-preview:p1:inst-return-not-latest
    if latest.is_none_or(|latest| latest.id != target.turn.id) {
        return Err(DomainError::NotLatestTurn);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-preview:p1:inst-return-not-latest
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-preview:p1:inst-if-not-latest
    let user_message = repo.user_message(scope, chat_id, request_id).await?;
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-preview:p1:inst-if-user-message-missing
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-preview:p1:inst-return-user-message-500
    let Some(user_message) = user_message else {
        tracing::error!(
            chat_id = %chat_id,
            turn_id = %target.turn.id,
            request_id = %request_id,
            "a turn has no live user message"
        );
        return Err(DomainError::Internal(
            "a turn has no live user message".to_owned(),
        ));
    };
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-preview:p1:inst-return-user-message-500
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-preview:p1:inst-if-user-message-missing
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-preview:p1:inst-return-preview
    Ok(MutationTarget {
        turn: target.turn,
        web_search_enabled: target.web_search_enabled,
        user_message,
    })
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-preview:p1:inst-return-preview
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-preview:p1:inst-preview-read-only
}

#[cfg(test)]
#[path = "preview_tests.rs"]
mod tests;
