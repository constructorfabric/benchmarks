//! Provider identifier sanitization of client-visible error messages
//! (DESIGN §3.3 "Provider identifier non-exposure invariant").
//!
//! Each recognized provider id becomes `[provider_id]`, each URL `[url]`, and
//! each `sk-…` key or `Bearer` token `[credential]`; the rest is kept.

use std::sync::LazyLock;

use regex::Regex;

#[allow(clippy::expect_used)] // static pattern, covered by the unit tests
static URL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"https?://[^\s"'<>)\]]+"#).expect("valid static regex"));
#[allow(clippy::expect_used)] // static pattern, covered by the unit tests
static BEARER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bBearer\s+[A-Za-z0-9._~+/=-]+").expect("valid static regex")
});
#[allow(clippy::expect_used)] // static pattern, covered by the unit tests
static SK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bsk-[A-Za-z0-9_-]{10,}").expect("valid static regex"));
#[allow(clippy::expect_used)] // static pattern, covered by the unit tests
static RESP_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:resp_|chatcmpl-|cmpl-|msg_)[A-Za-z0-9]+").expect("valid static regex")
});
#[allow(clippy::expect_used)] // static pattern, covered by the unit tests
static FILE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:file-|file_|assistant-|vs_)[A-Za-z0-9]{12,}").expect("valid static regex")
});

/// Sanitize a provider-originated message before it reaches a client.
#[must_use]
pub fn sanitize_provider_message(msg: &str) -> String {
    let s = URL_RE.replace_all(msg, "[url]");
    let s = BEARER_RE.replace_all(&s, "[credential]");
    let s = SK_RE.replace_all(&s, "[credential]");
    let s = RESP_RE.replace_all(&s, "[provider_id]");
    let s = FILE_RE.replace_all(&s, "[provider_id]");
    s.into_owned()
}

#[cfg(test)]
mod tests {
    use super::sanitize_provider_message as s;

    #[test]
    fn replaces_provider_ids() {
        assert_eq!(
            s("File file-abc123def456ghi not found in vs_0123456789abcdef"),
            "File [provider_id] not found in [provider_id]"
        );
        assert_eq!(
            s("response resp_abc123 failed"),
            "response [provider_id] failed"
        );
        assert_eq!(
            s("assistant-ABCDEFGHIJKL1 missing"),
            "[provider_id] missing"
        );
        assert_eq!(
            s("anthropic file_011CNha8iCJcU1wXNR6q4V8w gone"),
            "anthropic [provider_id] gone"
        );
    }

    #[test]
    fn keeps_ordinary_words() {
        assert_eq!(
            s("file-based storage and file_search tool"),
            "file-based storage and file_search tool"
        );
    }

    #[test]
    fn replaces_urls_and_credentials() {
        assert_eq!(
            s("see https://api.openai.com/v1/x?y=1 now"),
            "see [url] now"
        );
        assert_eq!(
            s("Authorization: Bearer abc.def.ghi"),
            "Authorization: [credential]"
        );
        assert_eq!(
            s("Incorrect API key provided: sk-proj-abcdefghij123"),
            "Incorrect API key provided: [credential]"
        );
    }
}
