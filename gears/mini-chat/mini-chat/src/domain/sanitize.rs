//! Provider message sanitizer (DESIGN section 3.3, "Provider identifier
//! non-exposure invariant").
//!
//! Provider error text reaches clients (SSE `error` messages), so recognized
//! provider-scoped identifiers, URLs and credentials are replaced:
//!
//! - `resp_`, `chatcmpl-`, `cmpl-`, `msg_` followed by letters and digits
//!   become `[provider_id]`;
//! - `file-`, `file_`, `assistant-`, `vs_` followed by at least 12 letters and
//!   digits become `[provider_id]` (the floor keeps `file-based` and
//!   `file_search` intact);
//! - `http://` and `https://` URLs become `[url]`;
//! - `sk-` keys (at least 10 letters or digits) become `[credential]`, and
//!   `Bearer <token>` becomes `Bearer [credential]` (the word `Bearer` stays);
//! - everything else is left as is.
//!
//! A pattern only starts at a word boundary, so `profile-abcdefghijklm` is not
//! mistaken for a `file-` identifier. URLs are replaced first (an identifier
//! inside a URL disappears with it).

const PROVIDER_ID: &str = "[provider_id]";
const URL: &str = "[url]";
const CREDENTIAL: &str = "[credential]";
const BEARER_CREDENTIAL: &str = "Bearer [credential]";

/// Prefixes followed by at least one letter or digit.
const SHORT_ID_PREFIXES: &[&str] = &["resp_", "chatcmpl-", "cmpl-", "msg_"];
/// Prefixes followed by at least [`LONG_ID_MIN`] letters or digits.
const LONG_ID_PREFIXES: &[&str] = &["file-", "file_", "assistant-", "vs_"];
const LONG_ID_MIN: usize = 12;
const KEY_MIN: usize = 10;

/// Replace provider identifiers, URLs and credentials in `msg`.
#[must_use]
pub fn sanitize_provider_message(msg: &str) -> String {
    let mut out = String::with_capacity(msg.len());
    let mut prev_is_word = false;
    let mut i = 0;
    while i < msg.len() {
        let rest = &msg[i..];
        if !prev_is_word && let Some((consumed, replacement)) = match_at(rest) {
            out.push_str(replacement);
            i += consumed;
            prev_is_word = false;
            continue;
        }
        let Some(ch) = rest.chars().next() else {
            break;
        };
        out.push(ch);
        prev_is_word = ch.is_ascii_alphanumeric() || ch == '_';
        i += ch.len_utf8();
    }
    out
}

/// Try every pattern at the start of `rest`: `(bytes consumed, replacement)`.
fn match_at(rest: &str) -> Option<(usize, &'static str)> {
    match_url(rest)
        .or_else(|| match_bearer(rest))
        .or_else(|| match_key(rest))
        .or_else(|| match_provider_id(rest))
}

fn starts_with_ignore_case(s: &str, prefix: &str) -> bool {
    s.as_bytes()
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix.as_bytes()))
}

/// Length in bytes of the leading run of ASCII letters and digits.
fn alnum_run(s: &str) -> usize {
    s.bytes().take_while(u8::is_ascii_alphanumeric).count()
}

fn match_url(rest: &str) -> Option<(usize, &'static str)> {
    let scheme = ["https://", "http://"]
        .into_iter()
        .find(|scheme| starts_with_ignore_case(rest, scheme))?;
    let body = &rest[scheme.len()..];
    let end = body
        .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>'))
        .unwrap_or(body.len());
    // Trailing sentence punctuation is not part of the URL.
    let url_body = body[..end].trim_end_matches(['.', ',', ';', ':', '!', ')', ']', '}']);
    if url_body.is_empty() {
        return None;
    }
    Some((scheme.len() + url_body.len(), URL))
}

fn match_bearer(rest: &str) -> Option<(usize, &'static str)> {
    const WORD: &str = "bearer";
    if !starts_with_ignore_case(rest, WORD) {
        return None;
    }
    let after_word = &rest[WORD.len()..];
    let spaces = after_word.bytes().take_while(|b| *b == b' ').count();
    if spaces == 0 {
        return None;
    }
    let token = &after_word[spaces..];
    let token_len = token
        .bytes()
        .take_while(|b| b.is_ascii_alphanumeric() || b"-._~+/=".contains(b))
        .count();
    if token_len == 0 {
        return None;
    }
    Some((WORD.len() + spaces + token_len, BEARER_CREDENTIAL))
}

fn match_key(rest: &str) -> Option<(usize, &'static str)> {
    let body = rest.strip_prefix("sk-")?;
    // Keys such as `sk-proj-...` carry `-` and `_` between the alphanumerics.
    let run = body
        .bytes()
        .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        .count();
    let alnum = body
        .bytes()
        .take(run)
        .filter(u8::is_ascii_alphanumeric)
        .count();
    (alnum >= KEY_MIN).then_some((3 + run, CREDENTIAL))
}

fn match_provider_id(rest: &str) -> Option<(usize, &'static str)> {
    for (prefixes, min) in [(SHORT_ID_PREFIXES, 1), (LONG_ID_PREFIXES, LONG_ID_MIN)] {
        for prefix in prefixes {
            if let Some(body) = rest.strip_prefix(prefix) {
                let run = alnum_run(body);
                if run >= min {
                    return Some((prefix.len() + run, PROVIDER_ID));
                }
            }
        }
    }
    None
}

#[cfg(test)]
#[path = "sanitize_tests.rs"]
mod sanitize_tests;
