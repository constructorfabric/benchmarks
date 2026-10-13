//! Provider message sanitization (DESIGN section 3.3 "Provider identifier
//! non-exposure invariant").
//!
//! Provider error text may reach clients (SSE `event: error` `message`). Before
//! that, every recognized provider identifier is replaced with
//! `[provider_id]`, every URL with `[url]` and every `sk-...` key or `Bearer`
//! token with `[credential]`; everything else is left as is:
//!
//! - response / completion ids: `resp_`, `chatcmpl-`, `cmpl-`, `msg_` followed
//!   by letters and digits;
//! - file / vector store ids: `file-`, `file_`, `assistant-`, `vs_` followed by
//!   at least 12 letters and digits (the floor keeps `file-based` or
//!   `file_search` intact);
//! - `sk-` keys with at least 10 letters or digits after the prefix (`-` and
//!   `_` may appear inside, e.g. `sk-proj-...`).
//!
//! The patterns are compiled once. If a pattern could not be compiled the
//! function fails closed and returns a generic text instead of the input.

use std::sync::LazyLock;

use regex::{Captures, Regex};

/// Replacement for a provider-issued identifier.
pub const PROVIDER_ID_PLACEHOLDER: &str = "[provider_id]";
/// Replacement for a URL.
pub const URL_PLACEHOLDER: &str = "[url]";
/// Replacement for an API key or bearer token.
pub const CREDENTIAL_PLACEHOLDER: &str = "[credential]";

/// Returned when the patterns are unavailable (fail closed).
const FALLBACK_MESSAGE: &str = "provider error";

/// Minimum number of letters or digits after `sk-` for a key.
const MIN_KEY_ALNUM: usize = 10;

struct Patterns {
    url: Regex,
    bearer: Regex,
    secret_key: Regex,
    response_id: Regex,
    file_id: Regex,
}

static PATTERNS: LazyLock<Option<Patterns>> = LazyLock::new(compile_patterns);

fn compile_patterns() -> Option<Patterns> {
    Some(Patterns {
        // The last character must not be closing punctuation of the sentence.
        url: Regex::new(r#"(?i)\bhttps?://[^\s"'<>()\[\]{}]*[^\s"'<>()\[\]{}.,;:!?]"#).ok()?,
        bearer: Regex::new(r"(?i)\bbearer\s+[A-Za-z0-9._~+/=-]+").ok()?,
        secret_key: Regex::new(r"\bsk-[A-Za-z0-9_-]+").ok()?,
        response_id: Regex::new(r"\b(?:chatcmpl-|cmpl-|resp_|msg_)[A-Za-z0-9]+").ok()?,
        file_id: Regex::new(r"\b(?:assistant-|file-|file_|vs_)[A-Za-z0-9]{12,}").ok()?,
    })
}

/// Sanitize a provider message before it can reach a client.
///
/// URLs are replaced first (they may embed ids or keys), then credentials,
/// then provider ids.
#[must_use]
pub fn sanitize_provider_message(message: &str) -> String {
    let Some(patterns) = PATTERNS.as_ref() else {
        return FALLBACK_MESSAGE.to_owned();
    };
    let text = patterns.url.replace_all(message, URL_PLACEHOLDER);
    let text = patterns.bearer.replace_all(&text, CREDENTIAL_PLACEHOLDER);
    let text = patterns
        .secret_key
        .replace_all(&text, |caps: &Captures<'_>| replace_secret_key(&caps[0]));
    let text = patterns
        .response_id
        .replace_all(&text, PROVIDER_ID_PLACEHOLDER);
    patterns
        .file_id
        .replace_all(&text, PROVIDER_ID_PLACEHOLDER)
        .into_owned()
}

/// `sk-` followed by at least [`MIN_KEY_ALNUM`] letters or digits is a key;
/// shorter matches are ordinary text.
fn replace_secret_key(matched: &str) -> String {
    let alnum = matched
        .trim_start_matches("sk-")
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .count();
    if alnum >= MIN_KEY_ALNUM {
        CREDENTIAL_PLACEHOLDER.to_owned()
    } else {
        matched.to_owned()
    }
}

#[cfg(test)]
#[path = "sanitize_tests.rs"]
mod tests;
