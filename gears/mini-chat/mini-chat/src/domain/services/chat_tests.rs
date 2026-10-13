#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::validate_title;
use crate::domain::error::DomainError;

#[test]
fn title_is_trimmed_and_counted_in_chars() {
    assert_eq!(validate_title("  a b \t\n").unwrap(), "a b");
    let emoji = "\u{1f600}";
    assert_eq!(
        validate_title(&emoji.repeat(255)).unwrap(),
        emoji.repeat(255)
    );
    for bad in [
        String::new(),
        " \u{3000}\t".to_owned(),
        "x".repeat(256),
        emoji.repeat(256),
    ] {
        assert!(
            matches!(validate_title(&bad), Err(DomainError::InvalidTitle)),
            "{bad:?}"
        );
    }
    // Surrounding whitespace does not count toward the limit.
    assert!(validate_title(&format!("  {}  ", "x".repeat(255))).is_ok());
}
