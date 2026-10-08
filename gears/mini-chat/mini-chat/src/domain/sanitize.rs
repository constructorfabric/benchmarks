//! Provider message sanitization (DESIGN §3.3 "Provider identifier non-exposure invariant").
//!
//! Replaces provider ids with `[provider_id]`, URLs with `[url]`, `sk-` keys and `Bearer` tokens with
//! `[credential]`; everything else in the message is left as is.

use std::sync::LazyLock;

use regex::{Captures, Regex};

/// `http(s)://...` up to whitespace or a closing delimiter.
static URL_RE: LazyLock<Regex> = LazyLock::new(|| compile(r#"(?i)\bhttps?://[^\s"'<>`)\]}]+"#));

/// `Bearer <token>` (case-insensitive scheme).
static BEARER_RE: LazyLock<Regex> =
    LazyLock::new(|| compile(r"(?i)\b(bearer)\s+[A-Za-z0-9._~+/=-]+"));

/// `sk-` keys (the length floor of 10 letters/digits is checked in code).
static SK_RE: LazyLock<Regex> = LazyLock::new(|| compile(r"\bsk-[A-Za-z0-9_-]+"));

/// Response / completion ids: prefix followed by at least one letter or digit.
static RESPONSE_ID_RE: LazyLock<Regex> =
    LazyLock::new(|| compile(r"\b(?:resp_|chatcmpl-|cmpl-|msg_)[A-Za-z0-9]+"));

/// File / assistant / vector store ids: prefix followed by at least 12 letters or digits.
static FILE_ID_RE: LazyLock<Regex> =
    LazyLock::new(|| compile(r"\b(?:file-|file_|assistant-|vs_)[A-Za-z0-9]{12,}"));

/// Compiles a constant pattern (covered by the unit tests, cannot fail at runtime).
#[allow(clippy::expect_used, reason = "constant, tested regex patterns")]
fn compile(pattern: &str) -> Regex {
    Regex::new(pattern).expect("valid constant regex")
}

const MIN_SK_ALNUM: usize = 10;

/// Replaces provider ids with `[provider_id]`, URLs with `[url]`, `sk-` keys and `Bearer` tokens
/// with `[credential]`; leaves everything else unchanged.
#[must_use]
pub fn sanitize_provider_message(message: &str) -> String {
    // Trailing sentence punctuation is kept outside the URL.
    let s = URL_RE.replace_all(message, |c: &Captures<'_>| {
        let m = &c[0];
        let url = m.trim_end_matches(['.', ',', ';', ':', '!', '?']);
        format!("[url]{}", &m[url.len()..])
    });
    let s = BEARER_RE.replace_all(&s, |c: &Captures<'_>| format!("{} [credential]", &c[1]));
    let s = SK_RE.replace_all(&s, |c: &Captures<'_>| {
        let m = &c[0];
        if m[3..].chars().filter(char::is_ascii_alphanumeric).count() >= MIN_SK_ALNUM {
            "[credential]".to_owned()
        } else {
            m.to_owned()
        }
    });
    let s = RESPONSE_ID_RE.replace_all(&s, "[provider_id]");
    let s = FILE_ID_RE.replace_all(&s, "[provider_id]");
    s.into_owned()
}

#[cfg(test)]
#[path = "sanitize_tests.rs"]
mod tests;
