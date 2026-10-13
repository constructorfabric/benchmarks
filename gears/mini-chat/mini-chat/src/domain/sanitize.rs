//! Provider identifier / credential scrubbing for client-visible messages (DESIGN §3.3).

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;

// The patterns are compile-time constants covered by the unit tests; `.ok()` keeps
// startup panic-free (an invalid pattern would only disable that scrub step).
static PROVIDER_ID: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(
        r"\b(?:(?:resp_|chatcmpl-|cmpl-|msg_)[A-Za-z0-9]+|(?:file-|file_|assistant-|vs_)[A-Za-z0-9]{12,})",
    )
    .ok()
});
static URL: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r#"https?://[^\s"'<>)\]]+"#).ok());
static SK_KEY: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"\bsk-[A-Za-z0-9_\-]{10,}").ok());
static BEARER: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"(?i)\bbearer\s+[A-Za-z0-9._~+/\-]+=*").ok());

fn scrub<'a>(re: Option<&Regex>, input: &'a str, replacement: &str) -> Cow<'a, str> {
    re.map_or(Cow::Borrowed(input), |re| re.replace_all(input, replacement))
}

/// Replaces provider ids with `[provider_id]`, URLs with `[url]` and `sk-` keys /
/// bearer tokens with `[credential]`; everything else is left as is.
#[must_use]
pub fn sanitize_provider_message(input: &str) -> String {
    let s = scrub(URL.as_ref(), input, "[url]");
    let s = scrub(BEARER.as_ref(), &s, "[credential]");
    let s = scrub(SK_KEY.as_ref(), &s, "[credential]");
    scrub(PROVIDER_ID.as_ref(), &s, "[provider_id]").into_owned()
}

#[cfg(test)]
#[path = "sanitize_tests.rs"]
mod sanitize_tests;
