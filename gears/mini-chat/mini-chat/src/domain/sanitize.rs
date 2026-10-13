//! Scrubbing of provider identifiers, URLs and credentials from client-visible messages
//! (DESIGN §3.3 "Provider identifier non-exposure invariant").

use std::sync::LazyLock;

use regex::Regex;

static URL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:https?|wss?)://[^\s'\x22<>]+").unwrap_or_else(|_| unreachable!())
});
static BEARER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bBearer\s+[A-Za-z0-9._~+/=-]+").unwrap_or_else(|_| unreachable!())
});
static SK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bsk-[A-Za-z0-9_-]{10,}").unwrap_or_else(|_| unreachable!()));
static RESP_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:resp_|chatcmpl-|cmpl-|msg_)[A-Za-z0-9]+").unwrap_or_else(|_| unreachable!())
});
static FILE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:file-|file_|assistant-|vs_)[A-Za-z0-9]{12,}")
        .unwrap_or_else(|_| unreachable!())
});

/// Replace provider ids with `[provider_id]`, URLs with `[url]`, keys/bearer tokens with `[credential]`.
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
mod tests;
