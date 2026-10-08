//! Provider-identifier / credential sanitization of client-visible messages (DESIGN §3.3).

use std::sync::LazyLock;

use regex::Regex;

static URL_RE: LazyLock<Regex> = LazyLock::new(|| {
    #[allow(clippy::unwrap_used)]
    Regex::new(r"https?://[^\s\x22'<>()\[\]{}]+").unwrap()
});
static BEARER_RE: LazyLock<Regex> = LazyLock::new(|| {
    #[allow(clippy::unwrap_used)]
    Regex::new(r"(?i)\bBearer\s+[A-Za-z0-9._~+/=-]+").unwrap()
});
static SK_RE: LazyLock<Regex> = LazyLock::new(|| {
    #[allow(clippy::unwrap_used)]
    Regex::new(r"\bsk-[A-Za-z0-9_-]{10,}").unwrap()
});
static RESP_RE: LazyLock<Regex> = LazyLock::new(|| {
    #[allow(clippy::unwrap_used)]
    Regex::new(r"\b(?:resp_|chatcmpl-|cmpl-|msg_)[A-Za-z0-9]+").unwrap()
});
static FILE_RE: LazyLock<Regex> = LazyLock::new(|| {
    #[allow(clippy::unwrap_used)]
    Regex::new(r"\b(?:file-|file_|assistant-|vs_)[A-Za-z0-9]{12,}").unwrap()
});

/// Replaces URLs with `[url]`, `sk-` keys and `Bearer` tokens with `[credential]`, and
/// provider ids with `[provider_id]`. Everything else is kept.
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
#[path = "sanitize_tests.rs"]
mod sanitize_tests;
