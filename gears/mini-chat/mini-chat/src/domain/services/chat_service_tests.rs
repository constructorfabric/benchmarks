use super::validate_title;
use crate::domain::error::DomainError;

#[test]
fn trims_surrounding_whitespace() {
    assert_eq!(validate_title("  hi \t\n").unwrap(), "hi");
    assert_eq!(validate_title("a  b").unwrap(), "a  b");
}

#[test]
fn rejects_empty_and_whitespace_only() {
    for raw in ["", " ", "\t\n  "] {
        assert_eq!(
            validate_title(raw),
            Err(DomainError::InvalidTitle),
            "{raw:?}"
        );
    }
}

#[test]
fn length_is_counted_in_chars_after_trim() {
    assert_eq!(
        validate_title(&"\u{e9}".repeat(255)).unwrap(),
        "\u{e9}".repeat(255)
    );
    assert_eq!(
        validate_title(&"\u{e9}".repeat(256)),
        Err(DomainError::InvalidTitle)
    );
    assert_eq!(validate_title(&"a".repeat(255)).unwrap().len(), 255);
    assert_eq!(
        validate_title(&"a".repeat(256)),
        Err(DomainError::InvalidTitle)
    );
    // Surrounding whitespace does not count towards the limit.
    let padded = format!("  {}  ", "a".repeat(255));
    assert_eq!(validate_title(&padded).unwrap(), "a".repeat(255));
}
