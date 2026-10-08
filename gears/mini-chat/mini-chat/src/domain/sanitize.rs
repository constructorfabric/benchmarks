//! Sanitization of provider-originated text before it reaches clients
//! (DESIGN §3.3 "Provider identifier non-exposure invariant").

use std::sync::LazyLock;

use regex::Regex;

static URL_RE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r#"(?i)\bhttps?://[^\s"'<>)\]]+"#).ok());
static BEARER_RE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"(?i)\bbearer\s+[A-Za-z0-9._~+/=-]+").ok());
static SK_RE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"\bsk-[A-Za-z0-9_-]{10,}").ok());
static RESP_RE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"\b(?:resp_|chatcmpl-|cmpl-|msg_)[A-Za-z0-9]+").ok());
static FILE_RE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"\b(?:file-|file_|assistant-|vs_)[A-Za-z0-9]{12,}").ok());

fn replace(re: &LazyLock<Option<Regex>>, input: &str, with: &str) -> String {
    match re.as_ref() {
        Some(r) => r.replace_all(input, with).into_owned(),
        None => input.to_owned(),
    }
}

/// Replaces provider identifiers with `[provider_id]`, URLs with `[url]` and
/// `sk-…` keys / `Bearer` tokens with `[credential]`; everything else is kept.
#[must_use]
pub fn sanitize_provider_message(input: &str) -> String {
    let s = replace(&URL_RE, input, "[url]");
    let s = replace(&BEARER_RE, &s, "[credential]");
    let s = replace(&SK_RE, &s, "[credential]");
    let s = replace(&RESP_RE, &s, "[provider_id]");
    replace(&FILE_RE, &s, "[provider_id]")
}

#[cfg(test)]
#[path = "sanitize_tests.rs"]
mod sanitize_tests;
