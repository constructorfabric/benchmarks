#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::{MAX_TITLE_CHARS, normalize_title};
use crate::domain::error::DomainError;

#[test]
fn titles_are_trimmed() {
    assert_eq!(
        normalize_title("  Quarterly report \t").unwrap(),
        "Quarterly report"
    );
}

#[test]
fn empty_and_blank_titles_are_refused() {
    for raw in ["", "   ", "\t\n \r"] {
        assert!(matches!(
            normalize_title(raw),
            Err(DomainError::InvalidTitle { .. })
        ));
    }
}

#[test]
fn the_limit_counts_characters_after_trimming() {
    let at_limit = "\u{e9}".repeat(MAX_TITLE_CHARS);
    assert_eq!(normalize_title(&format!(" {at_limit} ")).unwrap(), at_limit);

    let over_limit = "a".repeat(MAX_TITLE_CHARS + 1);
    assert!(matches!(
        normalize_title(&over_limit),
        Err(DomainError::InvalidTitle { .. })
    ));
}
