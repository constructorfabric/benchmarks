//! Provider text sanitization (DESIGN 3.3 "Provider identifier non-exposure
//! invariant").
//!
//! Every provider-authored text that can reach a client (SSE `error.message`,
//! anything stored for the API) goes through [`sanitize_provider_message`]:
//! recognized provider identifiers become `[provider_id]`, URLs `[url]`, keys
//! and bearer tokens `[credential]`; everything else is left as is.

use std::sync::LazyLock;

use regex::Regex;

/// Longest provider text (in characters) forwarded to a client.
pub const MAX_CLIENT_MESSAGE_CHARS: usize = 512;

/// `(pattern, replacement)` pairs, applied in order: URLs first (so an
/// identifier inside a URL disappears with it), then credentials, then ids.
const RULES: [(&str, &str); 5] = [
    (r"https?://\S+", "[url]"),
    (r"(?i)\bBearer\s+[A-Za-z0-9._~+/=-]+", "[credential]"),
    (r"\bsk-[A-Za-z0-9][A-Za-z0-9_-]{9,}", "[credential]"),
    (
        r"\b(?:chatcmpl-|cmpl-|resp_|msg_)[A-Za-z0-9]+",
        "[provider_id]",
    ),
    (
        r"\b(?:file-|file_|assistant-|vs_)[A-Za-z0-9]{12,}",
        "[provider_id]",
    ),
];

static COMPILED: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    RULES
        .iter()
        .filter_map(|(pattern, replacement)| match Regex::new(pattern) {
            Ok(regex) => Some((regex, *replacement)),
            Err(error) => {
                // Unreachable for the constant patterns above (a unit test
                // compiles each of them); the rule is skipped instead of panicking.
                tracing::error!(%error, pattern, "provider sanitizer rule does not compile");
                None
            }
        })
        .collect()
});

/// Replaces provider identifiers, URLs and credentials in `message`.
///
/// Idempotent: sanitizing a sanitized text changes nothing.
#[must_use]
pub fn sanitize_provider_message(message: &str) -> String {
    let mut text = message.to_owned();
    for (regex, replacement) in COMPILED.iter() {
        if let std::borrow::Cow::Owned(replaced) = regex.replace_all(&text, *replacement) {
            text = replaced;
        }
    }
    text
}

/// Sanitizes and bounds a provider text for a client.
#[must_use]
pub fn client_message(message: &str) -> String {
    let sanitized = sanitize_provider_message(message);
    if sanitized.chars().count() <= MAX_CLIENT_MESSAGE_CHARS {
        return sanitized;
    }
    sanitized.chars().take(MAX_CLIENT_MESSAGE_CHARS).collect()
}

#[cfg(test)]
#[path = "sanitize_tests.rs"]
mod tests;
