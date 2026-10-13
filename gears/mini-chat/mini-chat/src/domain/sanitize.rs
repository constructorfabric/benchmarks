//! Provider error message sanitization (provider identifier non-exposure).

use std::sync::LazyLock;

use regex::Regex;

#[allow(clippy::expect_used)]
static URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(?:https?|wss?)://[^\s\x22'<>]+").expect("valid regex"));
#[allow(clippy::expect_used)]
static BEARER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bBearer\s+[A-Za-z0-9._~+/=-]+").expect("valid regex"));
#[allow(clippy::expect_used)]
static SK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bsk-[A-Za-z0-9_-]{10,}").expect("valid regex"));
#[allow(clippy::expect_used)]
static RESP_IDS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(?:resp_|chatcmpl-|cmpl-|msg_)[A-Za-z0-9]+").expect("valid regex"));
#[allow(clippy::expect_used)]
static FILE_IDS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(?:file-|file_|assistant-|vs_)[A-Za-z0-9]{12,}").expect("valid regex"));

/// Replaces provider ids with `[provider_id]`, URLs with `[url]` and keys or
/// bearer tokens with `[credential]`.
#[must_use]
pub fn sanitize(message: &str) -> String {
    let s = URL.replace_all(message, "[url]");
    let s = BEARER.replace_all(&s, "[credential]");
    let s = SK.replace_all(&s, "[credential]");
    let s = RESP_IDS.replace_all(&s, "[provider_id]");
    let s = FILE_IDS.replace_all(&s, "[provider_id]");
    s.into_owned()
}

#[cfg(test)]
mod tests {
    use super::sanitize;

    #[test]
    fn replaces_ids_urls_and_credentials() {
        assert_eq!(
            sanitize("bad file file-abcdefghijkl1234 at https://x.y/z"),
            "bad file [provider_id] at [url]"
        );
        assert_eq!(sanitize("response resp_abc123 failed"), "response [provider_id] failed");
        assert_eq!(sanitize("key sk-ABCDEFGHIJ123 used"), "key [credential] used");
        assert_eq!(sanitize("Authorization: Bearer abc.def"), "Authorization: [credential]");
        assert_eq!(sanitize("vector store vs_abcdefghijklmn missing"), "vector store [provider_id] missing");
    }

    #[test]
    fn keeps_ordinary_words() {
        assert_eq!(sanitize("file-based storage and file_search tool"), "file-based storage and file_search tool");
        assert_eq!(sanitize("message from assistant-x"), "message from assistant-x");
    }
}
