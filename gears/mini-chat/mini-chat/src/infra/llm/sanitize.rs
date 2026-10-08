//! Sanitization of provider error messages before they reach a client:
//! provider identifiers become `[provider_id]`, URLs `[url]`, and `sk-…`
//! keys or `Bearer` tokens `[credential]`.

use std::sync::LazyLock;

use regex::Regex;

static CREDENTIAL: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(r"(?i)\bbearer\s+[A-Za-z0-9._~+/=-]+|\bsk-[A-Za-z0-9_-]{10,}").ok()
});
static URL: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r#"https?://[^\s"'<>]+"#).ok());
static RESPONSE_ID: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"\b(?:resp_|chatcmpl-|cmpl-|msg_)[A-Za-z0-9]+").ok());
static FILE_ID: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"\b(?:file-|file_|assistant-|vs_)[A-Za-z0-9]{12,}").ok());

fn replace(re: &LazyLock<Option<Regex>>, input: &str, with: &str) -> String {
    match re.as_ref() {
        Some(re) => re.replace_all(input, with).into_owned(),
        None => input.to_owned(),
    }
}

/// Sanitize a provider message.
#[must_use]
pub fn sanitize(message: &str) -> String {
    let s = replace(&CREDENTIAL, message, "[credential]");
    let s = replace(&URL, &s, "[url]");
    let s = replace(&RESPONSE_ID, &s, "[provider_id]");
    replace(&FILE_ID, &s, "[provider_id]")
}

#[cfg(test)]
mod tests {
    use super::sanitize;

    #[test]
    fn provider_ids_urls_and_credentials_are_scrubbed() {
        let msg = "File file-AbCdEfGh1234567 in vs_abcdefghijkl99 failed for resp_123abc; see https://x.y/z with sk-abcdefghijklmnop and Bearer eyJ.abc.def";
        let out = sanitize(msg);
        assert!(!out.contains("file-AbCd"));
        assert!(!out.contains("vs_abcd"));
        assert!(!out.contains("resp_123"));
        assert!(!out.contains("https://"));
        assert!(!out.contains("sk-abcd"));
        assert!(!out.contains("eyJ"));
        assert!(out.contains("[provider_id]"));
        assert!(out.contains("[url]"));
        assert!(out.contains("[credential]"));
    }

    #[test]
    fn ordinary_words_are_kept() {
        assert_eq!(
            sanitize("file-based file_search failed"),
            "file-based file_search failed"
        );
        assert_eq!(sanitize("assistant-x short"), "assistant-x short");
    }
}
