//! Chat title rules.

use crate::domain::error::DomainError;

/// Maximum title length in characters, counted after trimming.
pub const MAX_TITLE_CHARS: usize = 255;

/// Trims a title and checks that it has 1 to 255 characters (characters, not bytes).
///
/// # Errors
/// `InvalidTitle` when the trimmed title is empty or longer than 255 characters.
pub fn normalize_title(raw: &str) -> Result<String, DomainError> {
    let trimmed = raw.trim();
    let length = trimmed.chars().count();
    if length == 0 {
        return Err(DomainError::InvalidTitle {
            detail: "title must not be empty or whitespace-only".to_owned(),
        });
    }
    if length > MAX_TITLE_CHARS {
        return Err(DomainError::InvalidTitle {
            detail: format!("title must have at most {MAX_TITLE_CHARS} characters"),
        });
    }
    Ok(trimmed.to_owned())
}

#[cfg(test)]
#[path = "title_tests.rs"]
mod tests;
