//! Scrubbing of provider identifiers, URLs and credentials from messages
//! returned to clients (DESIGN "Provider identifier non-exposure invariant").

use std::sync::LazyLock;

use regex::Regex;

#[allow(clippy::expect_used)] // constant pattern, compiled once; covered by unit tests
static PROVIDER_IDS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\b(?:(?:resp_|chatcmpl-|cmpl-|msg_)[A-Za-z0-9]+|(?:file-|file_|assistant-|vs_)[A-Za-z0-9]{12,})",
    )
    .expect("provider id regex")
});
#[allow(clippy::expect_used)] // constant pattern, compiled once; covered by unit tests
static URLS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"https?://[^\s'\x22<>]+").expect("url regex"));
#[allow(clippy::expect_used)] // constant pattern, compiled once; covered by unit tests
static SK_KEYS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bsk-[A-Za-z0-9_\-]{10,}").expect("sk regex"));
#[allow(clippy::expect_used)] // constant pattern, compiled once; covered by unit tests
static BEARER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bBearer\s+[A-Za-z0-9._\-~+/]+=*").expect("bearer regex"));

/// Replace provider ids with `[provider_id]`, URLs with `[url]`, keys and
/// bearer tokens with `[credential]`.
#[must_use]
pub fn sanitize_provider_message(msg: &str) -> String {
    let s = URLS.replace_all(msg, "[url]");
    let s = BEARER.replace_all(&s, "[credential]");
    let s = SK_KEYS.replace_all(&s, "[credential]");
    PROVIDER_IDS.replace_all(&s, "[provider_id]").into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrubs_ids_urls_and_keys() {
        let m = sanitize_provider_message(
            "File file-abc123DEF456ghi not found in vs_0123456789abcdef; see https://api.openai.com/x?y=1 key sk-1234567890abcdef Bearer abc.def resp_42",
        );
        assert!(!m.contains("file-abc"));
        assert!(!m.contains("vs_0123"));
        assert!(!m.contains("openai.com"));
        assert!(!m.contains("sk-123"));
        assert!(!m.contains("abc.def"));
        assert!(!m.contains("resp_42"));
        assert!(m.contains("[provider_id]"));
        assert!(m.contains("[url]"));
        assert!(m.contains("[credential]"));
    }

    #[test]
    fn keeps_ordinary_words() {
        assert_eq!(sanitize_provider_message("file-based file_search works"), "file-based file_search works");
        assert_eq!(sanitize_provider_message("Rate limit exceeded"), "Rate limit exceeded");
    }
}
