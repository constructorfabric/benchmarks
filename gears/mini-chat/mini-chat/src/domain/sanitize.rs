//! Sanitization of provider messages before they reach a client (DESIGN "Provider identifier
//! non-exposure invariant", section 3.3).

use std::sync::LazyLock;

use regex::Regex;

/// Longest message sent to a client, in characters.
const MAX_MESSAGE_CHARS: usize = 1024;

#[allow(
    clippy::expect_used,
    reason = "the patterns are compile-time constants"
)]
static URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"https?://[^\s<>"']*[^\s<>"'.,;:!?)\]]"#).expect("a valid regex")
});
#[allow(
    clippy::expect_used,
    reason = "the patterns are compile-time constants"
)]
static CREDENTIAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i:\bBearer\s+[A-Za-z0-9._~+/=-]+)|\bsk-[A-Za-z0-9_-]{10,}")
        .expect("a valid regex")
});
#[allow(
    clippy::expect_used,
    reason = "the patterns are compile-time constants"
)]
static PROVIDER_ID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\b(?:(?:resp_|chatcmpl-|cmpl-|msg_)[A-Za-z0-9]+|(?:file-|file_|assistant-|vs_)[A-Za-z0-9]{12,})",
    )
    .expect("a valid regex")
});

/// Replaces URLs with `[url]`, `sk-` keys and `Bearer` tokens with `[credential]` and provider
/// identifiers with `[provider_id]`; everything else is left as is. The result is capped at
/// 1024 characters.
#[must_use]
pub fn sanitize_provider_message(raw: &str) -> String {
    let without_urls = URL.replace_all(raw, "[url]");
    let without_credentials = CREDENTIAL.replace_all(&without_urls, "[credential]");
    let sanitized = PROVIDER_ID.replace_all(&without_credentials, "[provider_id]");
    if sanitized.chars().count() > MAX_MESSAGE_CHARS {
        let cut: String = sanitized.chars().take(MAX_MESSAGE_CHARS).collect();
        format!("{cut}...")
    } else {
        sanitized.into_owned()
    }
}

#[cfg(test)]
#[path = "sanitize_tests.rs"]
mod tests;
