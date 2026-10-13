//! Scrubbing of provider-issued identifiers, URLs and credentials from text that may reach a
//! client (DESIGN "Provider identifier non-exposure invariant").

use std::sync::LazyLock;

use regex::Regex;

#[allow(clippy::expect_used)] // the patterns are compile-time constants covered by the tests
fn compile(pattern: &str) -> Regex {
    Regex::new(pattern).expect("static sanitizer pattern is valid")
}

static URL: LazyLock<Regex> = LazyLock::new(|| compile(r"https?://\S+"));
static BEARER: LazyLock<Regex> = LazyLock::new(|| compile(r"Bearer \S+"));
static SECRET_KEY: LazyLock<Regex> = LazyLock::new(|| compile(r"\bsk-[A-Za-z0-9]{10,}"));
/// Response / completion ids: any non-empty alphanumeric tail.
static RESPONSE_ID: LazyLock<Regex> =
    LazyLock::new(|| compile(r"\b(?:resp_|chatcmpl-|cmpl-|msg_)[A-Za-z0-9]+"));
/// File / vector store ids: the 12-character floor keeps `file-based` and `file_search` intact.
static FILE_ID: LazyLock<Regex> =
    LazyLock::new(|| compile(r"\b(?:file-|file_|assistant-|vs_)[A-Za-z0-9]{12,}"));

/// Replaces provider ids with `[provider_id]`, URLs with `[url]` and `sk-…` keys / `Bearer`
/// tokens with `[credential]`; the rest of the message is left as is.
#[must_use]
pub fn sanitize_provider_message(message: &str) -> String {
    // URLs first so ids and keys embedded in a URL are swallowed with it.
    let out = URL.replace_all(message, "[url]");
    let out = BEARER.replace_all(&out, "[credential]");
    let out = SECRET_KEY.replace_all(&out, "[credential]");
    let out = RESPONSE_ID.replace_all(&out, "[provider_id]");
    FILE_ID.replace_all(&out, "[provider_id]").into_owned()
}

#[cfg(test)]
mod tests {
    use super::sanitize_provider_message;

    #[test]
    fn scrubs_ids_urls_credentials() {
        assert_eq!(
            sanitize_provider_message(
                "file-AbCdEf123456789 failed at https://x.y/z with sk-abcdefghijk1 and vs_abcdefghijkl12"
            ),
            "[provider_id] failed at [url] with [credential] and [provider_id]"
        );
        assert_eq!(
            sanitize_provider_message("file-based and file_search"),
            "file-based and file_search"
        );
        assert_eq!(
            sanitize_provider_message("resp_abc123 done"),
            "[provider_id] done"
        );
    }

    #[test]
    fn scrubs_every_provider_id_family_and_bearer_tokens() {
        assert_eq!(
            sanitize_provider_message(
                "chatcmpl-9Zx cmpl-77aa msg_01AbC assistant-abcdefghijkl file_abcdefghijkl"
            ),
            "[provider_id] [provider_id] [provider_id] [provider_id] [provider_id]"
        );
        assert_eq!(
            sanitize_provider_message("auth failed: Bearer eyJhbGciOi.abc.def retry"),
            "auth failed: [credential] retry"
        );
        assert_eq!(
            sanitize_provider_message("http://a.b/c?x=resp_abc and err_msg_text"),
            "[url] and err_msg_text"
        );
    }

    #[test]
    fn short_file_like_words_survive() {
        assert_eq!(
            sanitize_provider_message("vs_short assistant-x file_abcdefghijk"),
            "vs_short assistant-x file_abcdefghijk"
        );
    }
}
