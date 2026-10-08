//! Provider identifier / credential sanitization of client-visible messages
//! (DESIGN §3.3 "Provider identifier non-exposure invariant").

use std::sync::LazyLock;

use regex::Regex;

static URL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)\b(?:https?|wss?)://[^\s"'<>)\]]+"#).unwrap_or_else(|_| never()));
static SK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bsk-[A-Za-z0-9_\-]{10,}").unwrap_or_else(|_| never()));
static BEARER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(bearer)\s+[A-Za-z0-9._~+/=\-]+").unwrap_or_else(|_| never()));
static RESP_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:resp_|chatcmpl-|cmpl-|msg_)[A-Za-z0-9]+").unwrap_or_else(|_| never())
});
static FILE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:file-|file_|assistant-|vs_)[A-Za-z0-9]{12,}").unwrap_or_else(|_| never())
});

#[allow(clippy::panic, reason = "static regex literals are known-valid")]
fn never() -> Regex {
    panic!("invalid built-in sanitizer regex")
}

/// Replace provider ids with `[provider_id]`, URLs with `[url]`, and
/// `sk-` keys / bearer tokens with `[credential]`.
#[must_use]
pub fn sanitize_provider_message(msg: &str) -> String {
    let s = URL_RE.replace_all(msg, "[url]");
    let s = SK_RE.replace_all(&s, "[credential]");
    let s = BEARER_RE.replace_all(&s, "$1 [credential]");
    let s = RESP_RE.replace_all(&s, "[provider_id]");
    let s = FILE_RE.replace_all(&s, "[provider_id]");
    s.into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_ids_urls_and_credentials() {
        let m = sanitize_provider_message(
            "File file-AbCdEf123456789 not found in vs_abcdefghijklmnop for resp_123abc; see https://api.openai.com/x?y=1 key sk-ABCDEFGHIJKL Bearer eyJabc.def",
        );
        assert!(!m.contains("file-AbCdEf"), "{m}");
        assert!(!m.contains("vs_abcdefghijklmnop"), "{m}");
        assert!(!m.contains("resp_123abc"), "{m}");
        assert!(!m.contains("api.openai.com"), "{m}");
        assert!(!m.contains("sk-ABCDEF"), "{m}");
        assert!(!m.contains("eyJabc"), "{m}");
        assert!(m.contains("[provider_id]") && m.contains("[url]") && m.contains("[credential]"));
    }

    #[test]
    fn keeps_ordinary_words() {
        let m = sanitize_provider_message("file-based file_search is fine; msg is short; sk-short");
        assert_eq!(m, "file-based file_search is fine; msg is short; sk-short");
    }

    #[test]
    fn short_file_ids_are_kept() {
        assert_eq!(sanitize_provider_message("file-abc"), "file-abc");
        assert_eq!(sanitize_provider_message("chatcmpl-x1"), "[provider_id]");
        assert_eq!(sanitize_provider_message("msg_01ABC"), "[provider_id]");
    }
}
