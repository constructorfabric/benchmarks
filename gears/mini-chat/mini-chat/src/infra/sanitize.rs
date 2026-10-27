//! Provider identifier sanitization (DESIGN.md "Provider identifier
//! non-exposure invariant").
//!
//! Error messages returned to clients MUST NOT contain provider-issued
//! identifiers. Sanitization replaces each recognized provider ID with
//! `[provider_id]`, each URL with `[url]`, and each `sk-…` key or `Bearer`
//! token with `[credential]`.

use regex::Regex;
use std::sync::LazyLock;

#[allow(clippy::expect_used, reason = "compile-time-validated regexes")]
static PATTERNS: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    vec![
        // Credentials first (they may contain IDs).
        (
            Regex::new(r"sk-[A-Za-z0-9]{10,}").expect("valid regex"),
            "[credential]",
        ),
        (
            Regex::new(r"(?i)Bearer\s+\S+").expect("valid regex"),
            "[credential]",
        ),
        // URLs.
        (
            Regex::new(r"https?://\S+").expect("valid regex"),
            "[url]",
        ),
        // Response / completion ids.
        (
            Regex::new(r"(?:resp_|chatcmpl-|cmpl-|msg_)[A-Za-z0-9]+").expect("valid regex"),
            "[provider_id]",
        ),
        // File / vector store ids (length floor keeps ordinary words such as
        // `file-based` or `file_search` intact).
        (
            Regex::new(r"(?:file-|file_|assistant-|vs_)[A-Za-z0-9]{12,}").expect("valid regex"),
            "[provider_id]",
        ),
    ]
});

/// Sanitize a provider error message before returning it to clients.
#[must_use]
pub fn sanitize_provider_message(message: &str) -> String {
    let mut out = message.to_owned();
    for (re, replacement) in PATTERNS.iter() {
        out = re.replace_all(&out, *replacement).into_owned();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_provider_ids() {
        assert_eq!(
            sanitize_provider_message("vector store vs_abc123def456 not found"),
            "vector store [provider_id] not found"
        );
        assert_eq!(
            sanitize_provider_message("file file-abcdefghijklmnop missing"),
            "file [provider_id] missing"
        );
        assert_eq!(
            sanitize_provider_message("resp_abc123 failed"),
            "[provider_id] failed"
        );
    }

    #[test]
    fn keeps_ordinary_words() {
        assert_eq!(
            sanitize_provider_message("file-based file_search is file-tuned"),
            "file-based file_search is file-tuned"
        );
    }

    #[test]
    fn replaces_credentials_and_urls() {
        assert_eq!(
            sanitize_provider_message("key sk-abcdefghijklmnop leaked"),
            "key [credential] leaked"
        );
        assert_eq!(
            sanitize_provider_message("see https://example.com/x?y=1 for details"),
            "see [url] for details"
        );
        assert_eq!(
            sanitize_provider_message("Bearer abc.def.ghi"),
            "[credential]"
        );
    }
}
