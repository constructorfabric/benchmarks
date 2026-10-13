//! Error message sanitization
//! (`cpt-cf-mini-chat-algo-streaming-turns-sanitize-error`).

use std::sync::LazyLock;

use regex::{Captures, Regex};

/// Cap of `chat_turns.error_detail`, applied after sanitizing.
pub const ERROR_DETAIL_MAX_CHARS: usize = 1024;

const URL: &str = "[url]";
const CREDENTIAL: &str = "[credential]";
const PROVIDER_ID: &str = "[provider_id]";

/// Compiles one of the literal sanitizer patterns below. A literal that does
/// not compile is a programming error that the sanitizer tests catch, so the
/// panic cannot occur at run time.
#[allow(clippy::expect_used)]
fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("a valid sanitizer pattern")
}

static URL_RE: LazyLock<Regex> = LazyLock::new(|| regex(r"https?://\S+"));
static BEARER_RE: LazyLock<Regex> = LazyLock::new(|| regex(r"(?i)\bbearer\s+\S+"));
static AUTHORIZATION_RE: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r#"(?i)(\bauthorization\s*[:=]\s*)(?:(?:basic|digest|token|bearer|apikey|negotiate)\s+)?[^\s"',;]+"#,
    )
});
static API_KEY_RE: LazyLock<Regex> =
    LazyLock::new(|| regex(r#"(?i)(\bapi-key\s*[:=]\s*)[^\s"',;]+"#));
static SECRET_KEY_RE: LazyLock<Regex> = LazyLock::new(|| regex(r"\bsk-[A-Za-z0-9_*.-]{4,}"));
static REQUEST_ID_RE: LazyLock<Regex> =
    LazyLock::new(|| regex(r#"(?i)(\bx-request-id\s*[:=]?\s*["']?)[^\s"',;]+"#));
static UUID_RE: LazyLock<Regex> = LazyLock::new(|| {
    regex(r"(?i)\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b")
});
static RESPONSE_ID_RE: LazyLock<Regex> =
    LazyLock::new(|| regex(r"\b(?:resp_|chatcmpl-|cmpl-|msg_)[A-Za-z0-9]+"));
static PREFIXED_ID_RE: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"\b(?:resp|chatcmpl|cmpl|msg|req|org|proj|call|fc|rs|ws|ci|toolu|file|vs|assistant|thread|run|step)[-_]([A-Za-z0-9_-]{6,})",
    )
});
static FILE_ID_RE: LazyLock<Regex> =
    LazyLock::new(|| regex(r"\b(?:file|vs|assistant)[-_][A-Za-z0-9]{12,}"));

/// Replaces provider identifiers, credentials and URLs in `text` with
/// placeholders (`[url]`, `[credential]`, `[provider_id]`).
#[must_use]
// @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-sanitize-error:p1:inst-keep-rest
pub fn sanitize_error(text: &str) -> String {
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-sanitize-error:p1:inst-replace-urls
    let text = URL_RE.replace_all(text, URL);
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-sanitize-error:p1:inst-replace-urls
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-sanitize-error:p1:inst-replace-credentials
    let text = BEARER_RE.replace_all(&text, CREDENTIAL);
    let text = AUTHORIZATION_RE.replace_all(&text, |caps: &Captures<'_>| {
        format!("{}{CREDENTIAL}", &caps[1])
    });
    let text = API_KEY_RE.replace_all(&text, |caps: &Captures<'_>| {
        format!("{}{CREDENTIAL}", &caps[1])
    });
    let text = SECRET_KEY_RE.replace_all(&text, CREDENTIAL);
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-sanitize-error:p1:inst-replace-credentials
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-sanitize-error:p1:inst-replace-request-id-header
    let text = REQUEST_ID_RE.replace_all(&text, |caps: &Captures<'_>| {
        format!("{}{PROVIDER_ID}", &caps[1])
    });
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-sanitize-error:p1:inst-replace-request-id-header
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-sanitize-error:p1:inst-replace-uuids
    let text = UUID_RE.replace_all(&text, PROVIDER_ID);
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-sanitize-error:p1:inst-replace-uuids
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-sanitize-error:p1:inst-replace-response-ids
    let text = RESPONSE_ID_RE.replace_all(&text, PROVIDER_ID);
    let text = PREFIXED_ID_RE.replace_all(&text, |caps: &Captures<'_>| {
        if caps[1].chars().any(|c| c.is_ascii_digit()) {
            PROVIDER_ID.to_owned()
        } else {
            caps[0].to_owned()
        }
    });
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-sanitize-error:p1:inst-replace-response-ids
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-sanitize-error:p1:inst-replace-file-ids
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-sanitize-error:p1:inst-return-sanitized
    FILE_ID_RE.replace_all(&text, PROVIDER_ID).into_owned()
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-sanitize-error:p1:inst-return-sanitized
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-sanitize-error:p1:inst-replace-file-ids
}
// @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-sanitize-error:p1:inst-keep-rest

/// `error_detail` of a failed turn: `{code}: {message}` sanitized first and
/// then cut to [`ERROR_DETAIL_MAX_CHARS`] characters (`{message}` alone
/// when there is no provider code).
#[must_use]
// @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-sanitize-error:p1:inst-sanitize-before-truncate
pub fn error_detail(code: Option<&str>, message: &str) -> String {
    let joined = match code.filter(|code| !code.is_empty()) {
        Some(code) => format!("{code}: {message}"),
        None => message.to_owned(),
    };
    sanitize_error(&joined)
        .chars()
        .take(ERROR_DETAIL_MAX_CHARS)
        .collect()
}
// @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-sanitize-error:p1:inst-sanitize-before-truncate

#[cfg(test)]
#[path = "sanitize_tests.rs"]
mod tests;
