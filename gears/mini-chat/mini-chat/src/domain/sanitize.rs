//! Provider identifier non-exposure (DESIGN §3.3) and the provider `user` field (§4).

use std::sync::LazyLock;

use regex::Regex;
use uuid::Uuid;

fn re(pattern: &str) -> Regex {
    // Patterns are compile-time constants; a failure is a programming error
    // caught by the unit tests.
    #[allow(clippy::expect_used)]
    Regex::new(pattern).expect("valid sanitizer regex")
}

static URL: LazyLock<Regex> = LazyLock::new(|| re(r"https?://\S+"));
static BEARER: LazyLock<Regex> = LazyLock::new(|| re(r"Bearer \S+"));
static SK_KEY: LazyLock<Regex> = LazyLock::new(|| re(r"sk-[A-Za-z0-9]{10,}"));
static RESPONSE_ID: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?:resp_|chatcmpl-|cmpl-|msg_)[A-Za-z0-9]+"));
static FILE_ID: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?:file-|file_|assistant-|vs_)[A-Za-z0-9]{12,}"));

/// Replace provider ids with `[provider_id]`, URLs with `[url]` and
/// credentials with `[credential]`; everything else is left as is.
#[must_use]
pub fn sanitize_provider_message(msg: &str) -> String {
    let s = URL.replace_all(msg, "[url]");
    let s = BEARER.replace_all(&s, "[credential]");
    let s = SK_KEY.replace_all(&s, "[credential]");
    let s = RESPONSE_ID.replace_all(&s, "[provider_id]");
    FILE_ID.replace_all(&s, "[provider_id]").into_owned()
}

/// Provider `user` field: both UUIDs in simple hex form, tenant first
/// (64 characters); `"{tenant}:{user}"` when either is not a UUID.
#[must_use]
pub fn user_field(tenant: &str, user: &str) -> String {
    match (Uuid::parse_str(tenant), Uuid::parse_str(user)) {
        (Ok(t), Ok(u)) => format!("{}{}", t.simple(), u.simple()),
        _ => format!("{tenant}:{user}"),
    }
}

#[cfg(test)]
#[path = "sanitize_tests.rs"]
mod sanitize_tests;
