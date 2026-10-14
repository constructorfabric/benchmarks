//! Edit request validation: the emptiness check of `content` (empty and whitespace-only are
//! rejected, only the check trims), the content kept exactly as sent (never trimmed, rewritten
//! or truncated) and the generic `EmptyContent` rejection.
//!
//! Slice S1-all of `cpt-cf-mini-chat-feature-turn-mutations-and-reactions`: algorithm
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-edit-request-validation` (`inst-ev-empty`,
//! `inst-ev-return-empty`, `inst-ev-content-kept`, `inst-ev-return`); definition of done
//! `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-edit`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::{ValidatedEdit, validate_edit_content};
use crate::domain::error::DomainError;

fn rejected(content: &str) -> bool {
    matches!(
        validate_edit_content(content.to_owned()),
        Err(DomainError::EmptyContent)
    )
}

#[test]
fn an_empty_content_is_rejected_as_empty_content() {
    // inst-ev-empty, inst-ev-return-empty
    assert!(rejected(""));
}

#[test]
fn a_whitespace_only_content_is_rejected_as_empty_content() {
    // inst-ev-empty: spaces, tabs, line breaks and Unicode spaces all trim to nothing
    for content in [
        " ",
        "     ",
        "\t",
        "\n",
        "\r\n",
        " \t\n ",
        "\u{00a0}",
        "\u{2003}\u{2003}",
    ] {
        assert!(rejected(content), "{content:?} must be rejected");
    }
}

#[test]
fn a_content_with_a_visible_character_is_accepted() {
    for content in [
        "a",
        "0",
        ".",
        "ok",
        "multi\nline",
        "\u{442}\u{435}\u{441}\u{442}",
        "\u{65e5}\u{672c}\u{8a9e}",
    ] {
        let edit = validate_edit_content(content.to_owned())
            .unwrap_or_else(|error| panic!("{content:?} must be accepted: {error:?}"));
        assert_eq!(
            edit,
            ValidatedEdit {
                content: content.to_owned()
            }
        );
    }
}

#[test]
fn the_content_is_kept_exactly_as_sent() {
    // inst-ev-content-kept: only the emptiness check trims
    for content in [
        "  leading spaces",
        "trailing spaces  ",
        "\n\nsurrounded by line breaks\n",
        "  both  ",
        "inner   spacing\tkept",
        "trailing CRLF\r\n",
    ] {
        let edit = validate_edit_content(content.to_owned()).unwrap();
        assert_eq!(edit.content, content, "the content was rewritten");
    }
}

#[test]
fn a_long_content_is_not_truncated() {
    let content = "x".repeat(200_000);
    let edit = validate_edit_content(content.clone()).unwrap();
    assert_eq!(edit.content.len(), content.len());
    assert_eq!(edit.content, content);
}

#[test]
fn the_rejection_carries_no_content() {
    // the rejection is the plain `EmptyContent` variant: nothing of the input travels in it
    let error = validate_edit_content("   ".to_owned()).unwrap_err();
    assert!(matches!(error, DomainError::EmptyContent), "{error:?}");
    assert_eq!(error.to_string(), "empty content");
}
