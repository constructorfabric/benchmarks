#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 6 of `cpt-cf-mini-chat-feature-chat-crud`: the reaction value check
//! that precedes the PEP (`cpt-cf-mini-chat-flow-chat-crud-set-reaction`,
//! step 2).

use super::Reaction;
use crate::domain::error::DomainError;

#[test]
fn like_and_dislike_parse_to_their_values() {
    assert_eq!(Reaction::parse("like").unwrap(), Reaction::Like);
    assert_eq!(Reaction::parse("dislike").unwrap(), Reaction::Dislike);
}

#[test]
fn the_wire_value_round_trips() {
    for reaction in [Reaction::Like, Reaction::Dislike] {
        assert_eq!(Reaction::parse(reaction.as_str()).unwrap(), reaction);
    }
    assert_eq!(Reaction::Like.as_str(), "like");
    assert_eq!(Reaction::Dislike.as_str(), "dislike");
}

#[test]
fn every_other_string_is_an_invalid_reaction() {
    for raw in [
        "",
        " ",
        "love",
        "Like",
        "LIKE",
        "Dislike",
        " like",
        "like ",
        "likes",
        "thumbs_up",
        "\u{1f44d}",
        "like\n",
        "dis like",
    ] {
        match Reaction::parse(raw) {
            Err(DomainError::InvalidReaction) => {}
            other => panic!("`{raw:?}` must be an invalid reaction, got {other:?}"),
        }
    }
}
