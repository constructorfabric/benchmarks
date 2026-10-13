//! Provider-message sanitization (DESIGN §3.3 "Provider identifier
//! non-exposure invariant").

use std::sync::LazyLock;

use regex::Regex;

static URL_RE: LazyLock<Regex> = LazyLock::new(|| {
    #[allow(clippy::unwrap_used)]
    Regex::new(r#"https?://[^\s"'<>]+"#).unwrap()
});
static BEARER_RE: LazyLock<Regex> = LazyLock::new(|| {
    #[allow(clippy::unwrap_used)]
    Regex::new(r"(?i)\bBearer\s+[A-Za-z0-9\-._~+/]+=*").unwrap()
});
static SK_RE: LazyLock<Regex> = LazyLock::new(|| {
    #[allow(clippy::unwrap_used)]
    Regex::new(r"\bsk-[A-Za-z0-9_\-]{10,}").unwrap()
});
static RESP_ID_RE: LazyLock<Regex> = LazyLock::new(|| {
    #[allow(clippy::unwrap_used)]
    Regex::new(r"\b(?:resp_|chatcmpl-|cmpl-|msg_)[A-Za-z0-9]+").unwrap()
});
static FILE_ID_RE: LazyLock<Regex> = LazyLock::new(|| {
    #[allow(clippy::unwrap_used)]
    Regex::new(r"\b(?:file-|file_|assistant-|vs_)[A-Za-z0-9]{12,}").unwrap()
});

/// Replace provider ids with `[provider_id]`, URLs with `[url]` and
/// `sk-…` keys / Bearer tokens with `[credential]`.
#[must_use]
pub fn sanitize_provider_message(msg: &str) -> String {
    let s = URL_RE.replace_all(msg, "[url]");
    let s = BEARER_RE.replace_all(&s, "[credential]");
    let s = SK_RE.replace_all(&s, "[credential]");
    let s = RESP_ID_RE.replace_all(&s, "[provider_id]");
    let s = FILE_ID_RE.replace_all(&s, "[provider_id]");
    s.into_owned()
}

#[cfg(test)]
mod tests {
    use super::sanitize_provider_message as s;

    #[test]
    fn replaces_ids_urls_and_keys() {
        assert_eq!(s("bad file-abcdefghijklmnop here"), "bad [provider_id] here");
        assert_eq!(s("see https://x.example/a?b=1 now"), "see [url] now");
        assert_eq!(s("key sk-1234567890abc leaked"), "key [credential] leaked");
        assert_eq!(s("Authorization: Bearer abc.def"), "Authorization: [credential]");
        assert_eq!(s("response resp_abc123 failed"), "response [provider_id] failed");
        assert_eq!(s("vs_abcdefghijkl1 gone"), "[provider_id] gone");
    }

    #[test]
    fn keeps_ordinary_words() {
        assert_eq!(s("file-based storage and file_search tool"), "file-based storage and file_search tool");
        assert_eq!(s("plain message"), "plain message");
    }
}
