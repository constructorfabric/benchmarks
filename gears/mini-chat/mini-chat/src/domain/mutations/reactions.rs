//! The pure rules of the reactions (`cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-request-validation`,
//! `...-reaction-target` and the state machine `cpt-cf-mini-chat-state-turn-mutations-and-reactions-reaction`).

use time::OffsetDateTime;
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::messages::Reaction;

/// A stored reaction as `PUT` answers it: `message_id`, `reaction` and `created_at`.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredReaction {
    pub message_id: Uuid,
    pub reaction: Reaction,
    pub created_at: OffsetDateTime,
}

/// The transitions of the reaction state machine: `PUT` with a value or `DELETE`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReactionAction {
    Set(Reaction),
    Remove,
}

/// The reaction value of a `PUT` body: exactly `like` or `dislike`.
///
/// # Errors
///
/// `InvalidReaction` for any other string.
pub fn parse_reaction(value: &str) -> Result<Reaction, DomainError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-request-validation:p1:inst-rv-value
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-request-validation:p1:inst-rv-return-value
    match value {
        "like" => Ok(Reaction::Like),
        "dislike" => Ok(Reaction::Dislike),
        _ => Err(DomainError::InvalidReaction),
    }
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-request-validation:p1:inst-rv-return-value
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-request-validation:p1:inst-rv-value
}

/// The reaction target rule: only an `assistant` message accepts a reaction.
///
/// # Errors
///
/// `ReactionTargetNotAssistant` for a `user` or `system` message.
pub fn check_reaction_target(role: &str) -> Result<(), DomainError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-target:p1:inst-rt-not-assistant
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-target:p1:inst-rt-return-400
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-target:p1:inst-rt-any-assistant
    // every non-deleted assistant message is a target, a partial message of a cancelled turn and
    // an empty completed answer included; `PUT` and `DELETE` alike
    if role == "assistant" {
        Ok(())
    } else {
        Err(DomainError::ReactionTargetNotAssistant)
    }
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-target:p1:inst-rt-any-assistant
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-target:p1:inst-rt-return-400
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-target:p1:inst-rt-not-assistant
}

/// The state after `action`: `Set` replaces or keeps the value, `Remove` clears it.
#[must_use]
pub fn next_reaction(_current: Option<Reaction>, action: ReactionAction) -> Option<Reaction> {
    match action {
        // @cpt-begin:cpt-cf-mini-chat-state-turn-mutations-and-reactions-reaction:p1:inst-rx-none-like
        // @cpt-begin:cpt-cf-mini-chat-state-turn-mutations-and-reactions-reaction:p1:inst-rx-none-dislike
        // @cpt-begin:cpt-cf-mini-chat-state-turn-mutations-and-reactions-reaction:p1:inst-rx-like-dislike
        // @cpt-begin:cpt-cf-mini-chat-state-turn-mutations-and-reactions-reaction:p1:inst-rx-dislike-like
        // @cpt-begin:cpt-cf-mini-chat-state-turn-mutations-and-reactions-reaction:p1:inst-rx-same
        // a `PUT` stores its value whatever the current one is: the row is replaced or kept
        ReactionAction::Set(value) => Some(value),
        // @cpt-end:cpt-cf-mini-chat-state-turn-mutations-and-reactions-reaction:p1:inst-rx-same
        // @cpt-end:cpt-cf-mini-chat-state-turn-mutations-and-reactions-reaction:p1:inst-rx-dislike-like
        // @cpt-end:cpt-cf-mini-chat-state-turn-mutations-and-reactions-reaction:p1:inst-rx-like-dislike
        // @cpt-end:cpt-cf-mini-chat-state-turn-mutations-and-reactions-reaction:p1:inst-rx-none-dislike
        // @cpt-end:cpt-cf-mini-chat-state-turn-mutations-and-reactions-reaction:p1:inst-rx-none-like
        // @cpt-begin:cpt-cf-mini-chat-state-turn-mutations-and-reactions-reaction:p1:inst-rx-remove
        // @cpt-begin:cpt-cf-mini-chat-state-turn-mutations-and-reactions-reaction:p1:inst-rx-remove-none
        // a `DELETE` clears the value, with or without a stored reaction
        ReactionAction::Remove => None,
        // @cpt-end:cpt-cf-mini-chat-state-turn-mutations-and-reactions-reaction:p1:inst-rx-remove-none
        // @cpt-end:cpt-cf-mini-chat-state-turn-mutations-and-reactions-reaction:p1:inst-rx-remove
    }
}
