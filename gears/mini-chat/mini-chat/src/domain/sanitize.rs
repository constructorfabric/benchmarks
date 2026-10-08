//! Provider identifier sanitization (DESIGN "Provider identifier
//! non-exposure invariant"): replaces provider ids with `[provider_id]`, URLs
//! with `[url]` and API keys / bearer tokens with `[credential]`.

use std::sync::LazyLock;

use regex::Regex;

#[allow(clippy::expect_used)]
static URL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"https?://[^\s'\x22<>]+").expect("valid regex"));
#[allow(clippy::expect_used)]
static BEARER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)bearer\s+[A-Za-z0-9._~+/=-]+").expect("valid regex"));
#[allow(clippy::expect_used)]
static SK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bsk-[A-Za-z0-9_-]{10,}").expect("valid regex"));
#[allow(clippy::expect_used)]
static RESP_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:resp_|chatcmpl-|cmpl-|msg_)[A-Za-z0-9]+").expect("valid regex")
});
#[allow(clippy::expect_used)]
static FILE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:file-|file_|assistant-|vs_)[A-Za-z0-9]{12,}").expect("valid regex")
});

/// Sanitizes a provider message before it reaches a client.
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
