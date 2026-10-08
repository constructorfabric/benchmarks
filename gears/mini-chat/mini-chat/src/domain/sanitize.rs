//! Sanitization of provider-originated text before it reaches clients:
//! provider identifiers, URLs and credentials are masked.

use std::sync::LazyLock;

use regex::Regex;

static URL_RE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"https?://[^\s\x22'<>)]+").ok());
static SK_RE: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"\bsk-[A-Za-z0-9]{10,}").ok());
static BEARER_RE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"(?i)\bbearer\s+[A-Za-z0-9._~+/=\-]+").ok());
static RESP_RE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"\b(?:resp_|chatcmpl-|cmpl-|msg_)[A-Za-z0-9]+").ok());
static FILE_RE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"\b(?:file-|file_|assistant-|vs_)[A-Za-z0-9]{12,}").ok());

fn apply(re: &LazyLock<Option<Regex>>, input: &str, repl: &str) -> String {
    match re.as_ref() {
        Some(r) => r.replace_all(input, repl).into_owned(),
        None => input.to_owned(),
    }
}

/// Replace provider identifiers with `[provider_id]`, URLs with `[url]` and
/// `sk-…` keys / `Bearer` tokens with `[credential]`. Everything else is
/// left as is.
#[must_use]
pub fn sanitize_provider_message(input: &str) -> String {
    let s = apply(&URL_RE, input, "[url]");
    let s = apply(&BEARER_RE, &s, "[credential]");
    let s = apply(&SK_RE, &s, "[credential]");
    let s = apply(&RESP_RE, &s, "[provider_id]");
    apply(&FILE_RE, &s, "[provider_id]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_provider_ids() {
        let s = sanitize_provider_message(
            "response resp_abc123 failed for file-ABCDEFGHIJKL1234 in vs_abcdefghijkl99",
        );
        assert_eq!(
            s,
            "response [provider_id] failed for [provider_id] in [provider_id]"
        );
        assert_eq!(
            sanitize_provider_message("chatcmpl-XYZ and msg_01abc"),
            "[provider_id] and [provider_id]"
        );
    }

    #[test]
    fn keeps_ordinary_words() {
        let s = sanitize_provider_message("file-based file_search tool failed");
        assert_eq!(s, "file-based file_search tool failed");
    }

    #[test]
    fn masks_urls_and_credentials() {
        let s = sanitize_provider_message(
            "see https://api.example.com/v1/x?y=1 key sk-abcdefghij1234 auth Bearer eyJ.abc.def",
        );
        assert_eq!(s, "see [url] key [credential] auth [credential]");
        assert_eq!(sanitize_provider_message("sk-short"), "sk-short");
    }
}
