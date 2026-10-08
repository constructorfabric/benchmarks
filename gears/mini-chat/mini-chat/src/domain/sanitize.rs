//! Provider identifier non-exposure (DESIGN §3.3): sanitizes provider
//! messages before they reach clients.

use std::sync::LazyLock;

use regex::Regex;

static URL_RE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r#"https?://[^\s"'<>]+"#).ok());
static BEARER_RE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"(?i)\bBearer\s+[A-Za-z0-9._~+/=\-]+").ok());
static SK_RE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"\bsk-[A-Za-z0-9_\-]{10,}").ok());
static RESP_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(r"\b(?:resp_|chatcmpl-|cmpl-|msg_)[A-Za-z0-9]+").ok()
});
static FILE_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(r"\b(?:file-|file_|assistant-|vs_)[A-Za-z0-9]{12,}").ok()
});

fn apply(re: &LazyLock<Option<Regex>>, input: &str, with: &str) -> String {
    match re.as_ref() {
        Some(r) => r.replace_all(input, with).into_owned(),
        None => input.to_owned(),
    }
}

/// Replaces provider ids with `[provider_id]`, URLs with `[url]` and
/// `sk-…` keys / `Bearer` tokens with `[credential]`.
#[must_use]
pub fn sanitize_provider_message(message: &str) -> String {
    let s = apply(&URL_RE, message, "[url]");
    let s = apply(&BEARER_RE, &s, "[credential]");
    let s = apply(&SK_RE, &s, "[credential]");
    let s = apply(&RESP_RE, &s, "[provider_id]");
    apply(&FILE_RE, &s, "[provider_id]")
}

#[cfg(test)]
mod tests {
    use super::sanitize_provider_message;

    #[test]
    fn replaces_ids_urls_and_credentials() {
        let out = sanitize_provider_message(
            "resp_abc123 failed for file-ABCDEFGHIJKL1 in vs_abcdefghijkl12 see https://x.io/a?b=1 key sk-abcdefghij123 Bearer eyJabc.def",
        );
        assert!(!out.contains("resp_abc123"));
        assert!(!out.contains("file-ABCDEFGHIJKL1"));
        assert!(!out.contains("vs_abcdefghijkl12"));
        assert!(!out.contains("https://"));
        assert!(!out.contains("sk-abcdefghij123"));
        assert!(!out.contains("eyJabc"));
        assert!(out.contains("[provider_id]"));
        assert!(out.contains("[url]"));
        assert!(out.contains("[credential]"));
    }

    #[test]
    fn keeps_ordinary_words() {
        let s = "file_search and file-based answers, msg about vs code";
        assert_eq!(sanitize_provider_message(s), s);
    }

    #[test]
    fn short_sk_is_not_a_key() {
        assert_eq!(sanitize_provider_message("sk-short"), "sk-short");
    }
}
