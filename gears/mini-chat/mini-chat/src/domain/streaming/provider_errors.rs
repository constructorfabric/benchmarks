//! Provider error mapping and sanitization (slice S2-provider-path of
//! `cpt-cf-mini-chat-feature-message-streaming`, algorithm
//! `cpt-cf-mini-chat-algo-message-streaming-error-sanitization`): the streaming error code, the
//! sanitized client message and the stored `error_detail` of every provider failure.

use mini_chat_sdk::UsageTokens;
use toolkit_macros::domain_model;

use crate::domain::error::StreamErrorCode;
use crate::domain::streaming::provider_events::ProviderFailureEvent;

/// The longest client message: the sanitized provider text is cut here.
const MAX_MESSAGE_CHARS: usize = 400;

/// The longest provider text kept for the logs.
const MAX_INTERNAL_CHARS: usize = 2048;

/// The shortest unrecognized id-like token: letters, digits and `_`, mixing letters and digits.
const ID_LIKE_MIN: usize = 20;

/// The id prefixes followed by at least one letter or digit.
const SHORT_ID_PREFIXES: [&str; 8] = [
    "resp_",
    "chatcmpl-",
    "cmpl-",
    "msg_",
    "req_",
    "org-",
    "proj_",
    "toolu_",
];

/// The id prefixes followed by at least [`LONG_ID_MIN`] letters or digits.
const LONG_ID_PREFIXES: [&str; 7] = [
    "call_",
    "run_",
    "thread_",
    "file-",
    "file_",
    "assistant-",
    "vs_",
];

/// The length floor of the [`LONG_ID_PREFIXES`] families.
const LONG_ID_MIN: usize = 12;

/// The shortest run of hexadecimal characters taken for a request-id style token.
const HEX_RUN_MIN: usize = 16;

/// The shortest key body after `sk-`.
const KEY_BODY_MIN: usize = 10;

/// A provider failure before it is mapped.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderFailure {
    /// A non-successful HTTP response (the body is bounded by the reader).
    Http {
        status: u16,
        /// The raw `Retry-After` header value.
        retry_after: Option<String>,
        body: String,
        /// `true` when the response carries `ErrorSource::Gateway` (the gateway's own Problem).
        from_gateway: bool,
    },
    /// A gateway timeout (`Err(CanonicalError::DeadlineExceeded)`).
    GatewayTimeout,
    /// Any other gateway or transport error (an unavailable provider, an invalid response).
    Transport(String),
    /// The provider stream broke or ended without a terminal event.
    StreamBroken,
    /// No byte arrived within the provider stream idle timeout.
    IdleTimeout,
    /// `response.failed`, an `error` event or the Anthropic `error` event.
    Event(ProviderFailureEvent),
}

/// The mapped failure.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappedFailure {
    pub code: StreamErrorCode,
    /// Sanitized client text, also stored as `chat_turns.error_detail`: the sanitized provider
    /// message, or the generic message of the code.
    pub message: String,
    /// The provider's own code and message, for logs only (never sanitized, never sent).
    pub internal: String,
    /// The usage of a `response.failed` that carries it.
    pub usage: Option<UsageTokens>,
}

/// The generic client message of a streaming error code (no provider text).
#[must_use]
pub fn generic_message(code: StreamErrorCode) -> &'static str {
    match code {
        StreamErrorCode::ProviderError => "The provider request failed. Please try again later.",
        StreamErrorCode::ProviderTimeout => {
            "The provider did not respond in time. Please try again."
        }
        StreamErrorCode::RateLimited => {
            "The provider is limiting requests right now. Please try again later."
        }
        StreamErrorCode::WebSearchCallsExceeded => {
            "The response used more web searches than one message may use."
        }
        StreamErrorCode::CodeInterpreterCallsExceeded => {
            "The response used more code interpreter calls than one message may use."
        }
        StreamErrorCode::AgenticIterationsExceeded => {
            "The response needed more retrieval steps than one message may use."
        }
        StreamErrorCode::UnexpectedToolUse => {
            "The provider asked for a tool that is not available."
        }
        StreamErrorCode::MessagePersistenceFailed => {
            "The response could not be saved. Please resend your message."
        }
        StreamErrorCode::FinalizationFailed => {
            "The response could not be completed. Please check the turn status."
        }
        StreamErrorCode::StreamInterrupted => {
            "The stream was interrupted. Please check the turn status."
        }
    }
}

/// A character that can continue an alphanumeric token.
fn is_alnum(c: char) -> bool {
    c.is_ascii_alphanumeric()
}

/// A token starts here when the previous character is not a letter, digit, `_` or `-`.
fn strict_start(chars: &[char], index: usize) -> bool {
    index == 0
        || !(is_alnum(chars[index - 1]) || chars[index - 1] == '_' || chars[index - 1] == '-')
}

/// A token starts here when the previous character is not a letter or a digit.
fn loose_start(chars: &[char], index: usize) -> bool {
    index == 0 || !is_alnum(chars[index - 1])
}

fn starts_with_at(chars: &[char], index: usize, needle: &str) -> bool {
    let mut at = index;
    for expected in needle.chars() {
        match chars.get(at) {
            Some(c) if *c == expected => at += 1,
            _ => return false,
        }
    }
    true
}

fn starts_with_ci(chars: &[char], index: usize, needle: &str) -> bool {
    let mut at = index;
    for expected in needle.chars() {
        match chars.get(at) {
            Some(c) if c.eq_ignore_ascii_case(&expected) => at += 1,
            _ => return false,
        }
    }
    true
}

/// The end (exclusive) of the run of characters of `index..` that satisfy `keep`.
fn run_end(chars: &[char], index: usize, keep: impl Fn(char) -> bool) -> usize {
    let mut end = index;
    while end < chars.len() && keep(chars[end]) {
        end += 1;
    }
    end
}

/// A URL: `http://` or `https://` up to the next whitespace, without the trailing punctuation.
fn url_end(chars: &[char], index: usize) -> Option<usize> {
    let scheme = if starts_with_ci(chars, index, "https://") {
        "https://".len()
    } else if starts_with_ci(chars, index, "http://") {
        "http://".len()
    } else {
        return None;
    };
    let mut end = run_end(chars, index + scheme, |c| !c.is_whitespace());
    while end > index + scheme && ".,;:!?)]}'\"".contains(chars[end - 1]) {
        end -= 1;
    }
    (end > index + scheme).then_some(end)
}

/// An `sk-` key: at least [`KEY_BODY_MIN`] letters, digits, `_`, `-` and `*` after the prefix.
fn key_end(chars: &[char], index: usize) -> Option<usize> {
    if !starts_with_at(chars, index, "sk-") {
        return None;
    }
    let end = run_end(chars, index + 3, |c| {
        is_alnum(c) || c == '_' || c == '-' || c == '*'
    });
    (end - (index + 3) >= KEY_BODY_MIN).then_some(end)
}

/// A `Bearer` token: the word and the token that follows it (a repeated word is part of the
/// match, so `bearer Bearer abc` leaves nothing of the token behind).
fn bearer_end(chars: &[char], index: usize) -> Option<usize> {
    const WORD: &str = "bearer";
    if !starts_with_ci(chars, index, WORD) {
        return None;
    }
    let token_char = |c: char| is_alnum(c) || "._~+/=-".contains(c);
    let mut at = index + WORD.len();
    loop {
        let after_space = run_end(chars, at, char::is_whitespace);
        if after_space == at {
            return None;
        }
        let end = run_end(chars, after_space, token_char);
        if end == after_space {
            return None;
        }
        let token: String = chars[after_space..end].iter().collect();
        if token.eq_ignore_ascii_case(WORD) {
            at = end;
            continue;
        }
        return Some(end);
    }
}

/// A provider id by its prefix family and length floor.
fn prefixed_id_end(chars: &[char], index: usize) -> Option<usize> {
    let families: [(&[&str], usize); 2] =
        [(&SHORT_ID_PREFIXES, 1), (&LONG_ID_PREFIXES, LONG_ID_MIN)];
    for (prefixes, floor) in families {
        for prefix in prefixes {
            if starts_with_at(chars, index, prefix) {
                let start = index + prefix.chars().count();
                let end = run_end(chars, start, is_alnum);
                if end - start >= floor {
                    return Some(end);
                }
            }
        }
    }
    None
}

/// A UUID in hyphenated form.
fn uuid_end(chars: &[char], index: usize) -> Option<usize> {
    const GROUPS: [usize; 5] = [8, 4, 4, 4, 12];
    let mut at = index;
    for (position, len) in GROUPS.iter().enumerate() {
        if position > 0 {
            if chars.get(at) != Some(&'-') {
                return None;
            }
            at += 1;
        }
        let end = run_end(chars, at, |c| c.is_ascii_hexdigit());
        if end - at != *len {
            return None;
        }
        at = end;
    }
    chars.get(at).is_none_or(|c| !is_alnum(*c)).then_some(at)
}

/// A request-id style token: an alphanumeric token of at least [`HEX_RUN_MIN`] hexadecimal
/// characters only.
fn hex_run_end(chars: &[char], index: usize) -> Option<usize> {
    let end = run_end(chars, index, is_alnum);
    let all_hex = chars[index..end].iter().all(char::is_ascii_hexdigit);
    (all_hex && end - index >= HEX_RUN_MIN).then_some(end)
}

/// Sanitize client-visible text by replacement only: each recognized provider id becomes
/// `[provider_id]`, each URL `[url]`, each `sk-` key and `Bearer` token `[credential]`;
/// everything else (`file-based`, `file_search`, `thread_summary`) is left unchanged.
#[must_use]
pub fn sanitize_provider_text(text: &str) -> String {
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-error-sanitization:p1:inst-err-sanitize
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < chars.len() {
        let replacement = if loose_start(&chars, index) {
            if let Some(end) = url_end(&chars, index) {
                Some((end, "[url]"))
            } else if let Some(end) = key_end(&chars, index) {
                Some((end, "[credential]"))
            } else if let Some(end) = bearer_end(&chars, index) {
                Some((end, "[credential]"))
            } else if let Some(end) = uuid_end(&chars, index) {
                Some((end, "[provider_id]"))
            } else if strict_start(&chars, index) {
                prefixed_id_end(&chars, index)
                    .or_else(|| hex_run_end(&chars, index))
                    .map(|end| (end, "[provider_id]"))
            } else {
                hex_run_end(&chars, index).map(|end| (end, "[provider_id]"))
            }
        } else {
            None
        };
        if let Some((end, label)) = replacement {
            out.push_str(label);
            index = end;
        } else {
            out.push(chars[index]);
            index += 1;
        }
    }
    out
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-error-sanitization:p1:inst-err-sanitize
}

/// Whether `text` still holds an unrecognized id-like token: a run of at least 20 letters,
/// digits and `_` that mixes letters and digits. Such a text is never sent to a client.
#[must_use]
pub fn has_unrecognized_id(text: &str) -> bool {
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-error-sanitization:p1:inst-err-generic-fallback
    let chars: Vec<char> = text.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        let end = run_end(&chars, index, |c| is_alnum(c) || c == '_');
        if end == index {
            index += 1;
            continue;
        }
        let run = &chars[index..end];
        if run.len() >= ID_LIKE_MIN
            && run.iter().any(char::is_ascii_alphabetic)
            && run.iter().any(char::is_ascii_digit)
        {
            return true;
        }
        index = end;
    }
    false
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-error-sanitization:p1:inst-err-generic-fallback
}

/// The message a provider put into an error body: `error.message`, a top-level `message` or a
/// string `error`; `None` for a body that is not such a JSON document.
fn body_message(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let text = value
        .pointer("/error/message")
        .or_else(|| value.get("message"))
        .or_else(|| value.get("error"))
        .and_then(serde_json::Value::as_str)?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// `text` cut to at most `max` characters.
fn bounded(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// The client message of a provider text: sanitized, the generic message of `code` when nothing
/// usable remains or an unrecognized id-like token does.
fn client_message(code: StreamErrorCode, provider_text: Option<&str>) -> String {
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-error-sanitization:p1:inst-err-internal-keep
    let sanitized = provider_text.map(sanitize_provider_text);
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-error-sanitization:p1:inst-err-internal-keep
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-error-sanitization:p1:inst-err-generic-message
    match sanitized {
        Some(text) if !text.trim().is_empty() && !has_unrecognized_id(&text) => {
            bounded(&text, MAX_MESSAGE_CHARS)
        }
        _ => generic_message(code).to_owned(),
    }
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-error-sanitization:p1:inst-err-generic-message
}

/// The seconds of a numeric `Retry-After` header; an HTTP date is not a delay.
fn retry_after_seconds(header: Option<&str>) -> Option<u64> {
    header.and_then(|value| value.trim().parse::<u64>().ok())
}

/// Map a provider failure: HTTP 429 gives `rate_limited` (the seconds of a numeric
/// `Retry-After` in the message), a gateway timeout or the gateway's own 504 gives
/// `provider_timeout`, an idle timeout gives `provider_timeout`, everything else gives
/// `provider_error`; a provider 401 or 403, an empty message and an unsafe message give the
/// generic message.
#[must_use]
pub fn map_provider_failure(failure: &ProviderFailure) -> MappedFailure {
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-error-sanitization:p1:inst-err-match
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-error-sanitization:p1:inst-err-return
    match failure {
        ProviderFailure::Http {
            status,
            retry_after,
            body,
            from_gateway,
        } => {
            let internal = format!(
                "http status {status}: {}",
                bounded(body, MAX_INTERNAL_CHARS)
            );
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-error-sanitization:p1:inst-err-timeout
            if *from_gateway && *status == 504 {
                return MappedFailure {
                    code: StreamErrorCode::ProviderTimeout,
                    message: generic_message(StreamErrorCode::ProviderTimeout).to_owned(),
                    internal,
                    usage: None,
                };
            }
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-error-sanitization:p1:inst-err-timeout
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-error-sanitization:p1:inst-err-429
            if *status == 429 && !*from_gateway {
                // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-error-sanitization:p1:inst-err-no-quota-mapping
                // a provider 429 is `rate_limited`: never a quota 429 and never a quota scope
                // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-error-sanitization:p1:inst-err-no-quota-mapping
                // the provider text is sanitized, the delay of a numeric `Retry-After` follows
                let message =
                    client_message(StreamErrorCode::RateLimited, body_message(body).as_deref());
                let message = match retry_after_seconds(retry_after.as_deref()) {
                    Some(seconds) => format!("{message} Retry after {seconds} seconds."),
                    None => message,
                };
                return MappedFailure {
                    code: StreamErrorCode::RateLimited,
                    message,
                    internal,
                    usage: None,
                };
            }
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-error-sanitization:p1:inst-err-429
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-error-sanitization:p1:inst-err-provider-error
            // an authentication or authorization failure and the gateway's own Problem never
            // show provider or gateway text
            let text = if matches!(status, 401 | 403) || *from_gateway {
                None
            } else {
                body_message(body)
            };
            MappedFailure {
                code: StreamErrorCode::ProviderError,
                message: client_message(StreamErrorCode::ProviderError, text.as_deref()),
                internal,
                usage: None,
            }
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-error-sanitization:p1:inst-err-provider-error
        }
        ProviderFailure::GatewayTimeout => timeout_failure("the gateway timed out".to_owned()),
        ProviderFailure::IdleTimeout => {
            timeout_failure("no provider byte arrived within the idle timeout".to_owned())
        }
        ProviderFailure::Transport(detail) => MappedFailure {
            code: StreamErrorCode::ProviderError,
            message: generic_message(StreamErrorCode::ProviderError).to_owned(),
            internal: bounded(detail, MAX_INTERNAL_CHARS),
            usage: None,
        },
        ProviderFailure::StreamBroken => MappedFailure {
            code: StreamErrorCode::ProviderError,
            message: generic_message(StreamErrorCode::ProviderError).to_owned(),
            internal: "the provider stream broke or ended without a terminal event".to_owned(),
            usage: None,
        },
        ProviderFailure::Event(event) => MappedFailure {
            code: StreamErrorCode::ProviderError,
            message: client_message(StreamErrorCode::ProviderError, event.message.as_deref()),
            internal: bounded(
                &format!(
                    "{}: {}",
                    event.code.as_deref().unwrap_or("unknown"),
                    event.message.as_deref().unwrap_or("")
                ),
                MAX_INTERNAL_CHARS,
            ),
            usage: event.usage,
        },
    }
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-error-sanitization:p1:inst-err-return
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-error-sanitization:p1:inst-err-match
}

fn timeout_failure(internal: String) -> MappedFailure {
    MappedFailure {
        code: StreamErrorCode::ProviderTimeout,
        message: generic_message(StreamErrorCode::ProviderTimeout).to_owned(),
        internal,
        usage: None,
    }
}
