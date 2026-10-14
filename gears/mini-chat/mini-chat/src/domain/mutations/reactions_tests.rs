//! The pure rules of the reactions: the reaction value validation (`like`, `dislike`, anything
//! else), the reaction target rule (assistant messages only) and the reaction state machine.
//!
//! Slice S1-all of `cpt-cf-mini-chat-feature-turn-mutations-and-reactions`: algorithms
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-request-validation`
//! (`inst-rv-value`, `inst-rv-return-value`, `inst-rv-return`) and
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-target` (`inst-rt-not-assistant`,
//! `inst-rt-return-400`, `inst-rt-any-assistant`); state
//! `cpt-cf-mini-chat-state-turn-mutations-and-reactions-reaction` (`inst-rx-none-like`,
//! `inst-rx-none-dislike`, `inst-rx-like-dislike`, `inst-rx-dislike-like`, `inst-rx-same`,
//! `inst-rx-remove`, `inst-rx-remove-none`); definition of done
//! `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-reactions`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::{ReactionAction, check_reaction_target, next_reaction, parse_reaction};
use crate::domain::error::DomainError;
use crate::domain::messages::Reaction;

// ── the value ────────────────────────────────────────────────────────────────

#[test]
fn like_and_dislike_are_the_only_values() {
    // inst-rv-return
    assert_eq!(parse_reaction("like").unwrap(), Reaction::Like);
    assert_eq!(parse_reaction("dislike").unwrap(), Reaction::Dislike);
}

#[test]
fn any_other_value_is_an_invalid_reaction() {
    // inst-rv-value, inst-rv-return-value: exactly `like` or `dislike`
    for value in [
        "",
        "Like",
        "LIKE",
        "Dislike",
        " like",
        "like ",
        "like\n",
        "love",
        "thumbs_up",
        "1",
        "null",
        "true",
        "likes",
        "dis like",
    ] {
        assert!(
            matches!(parse_reaction(value), Err(DomainError::InvalidReaction)),
            "{value:?} must be an invalid reaction"
        );
    }
}

#[test]
fn the_value_round_trips_through_its_wire_form() {
    for reaction in [Reaction::Like, Reaction::Dislike] {
        assert_eq!(parse_reaction(reaction.as_str()).unwrap(), reaction);
    }
}

// ── the target ───────────────────────────────────────────────────────────────

#[test]
fn an_assistant_message_accepts_a_reaction() {
    // inst-rt-any-assistant
    check_reaction_target("assistant").unwrap();
}

#[test]
fn a_user_or_system_message_is_not_a_reaction_target() {
    // inst-rt-not-assistant, inst-rt-return-400: for PUT and DELETE alike
    for role in ["user", "system"] {
        assert!(
            matches!(
                check_reaction_target(role),
                Err(DomainError::ReactionTargetNotAssistant)
            ),
            "{role}"
        );
    }
}

#[test]
fn an_unknown_role_is_not_a_reaction_target() {
    for role in ["", "tool", "Assistant", "assistant "] {
        assert!(
            matches!(
                check_reaction_target(role),
                Err(DomainError::ReactionTargetNotAssistant)
            ),
            "{role:?}"
        );
    }
}

// ── the state machine ────────────────────────────────────────────────────────

#[test]
fn none_goes_to_like_or_dislike_on_put() {
    // inst-rx-none-like, inst-rx-none-dislike
    assert_eq!(
        next_reaction(None, ReactionAction::Set(Reaction::Like)),
        Some(Reaction::Like)
    );
    assert_eq!(
        next_reaction(None, ReactionAction::Set(Reaction::Dislike)),
        Some(Reaction::Dislike)
    );
}

#[test]
fn like_and_dislike_replace_each_other_on_put() {
    // inst-rx-like-dislike, inst-rx-dislike-like: the row is replaced
    assert_eq!(
        next_reaction(Some(Reaction::Like), ReactionAction::Set(Reaction::Dislike)),
        Some(Reaction::Dislike)
    );
    assert_eq!(
        next_reaction(Some(Reaction::Dislike), ReactionAction::Set(Reaction::Like)),
        Some(Reaction::Like)
    );
}

#[test]
fn a_repeated_put_keeps_the_same_state() {
    // inst-rx-same: idempotent
    for reaction in [Reaction::Like, Reaction::Dislike] {
        assert_eq!(
            next_reaction(Some(reaction), ReactionAction::Set(reaction)),
            Some(reaction)
        );
    }
}

#[test]
fn delete_clears_the_reaction_whatever_it_was() {
    // inst-rx-remove, inst-rx-remove-none: a delete without a reaction is still a success
    assert_eq!(
        next_reaction(Some(Reaction::Like), ReactionAction::Remove),
        None
    );
    assert_eq!(
        next_reaction(Some(Reaction::Dislike), ReactionAction::Remove),
        None
    );
    assert_eq!(next_reaction(None, ReactionAction::Remove), None);
}

#[test]
fn every_transition_of_the_state_machine_is_covered() {
    let states = [None, Some(Reaction::Like), Some(Reaction::Dislike)];
    let actions = [
        ReactionAction::Set(Reaction::Like),
        ReactionAction::Set(Reaction::Dislike),
        ReactionAction::Remove,
    ];
    for state in states {
        for action in actions {
            let next = next_reaction(state, action);
            let expected = match action {
                ReactionAction::Set(value) => Some(value),
                ReactionAction::Remove => None,
            };
            assert_eq!(next, expected, "{state:?} --{action:?}-->");
        }
    }
}
