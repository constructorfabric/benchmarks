//! Provider error classification
//! (`cpt-cf-mini-chat-algo-streaming-turns-classify-provider-error`): maps a
//! `proxy_request` failure, a non-streaming response or a body read failure
//! to a streaming error code and a client-safe message.

use serde_json::Value;
use toolkit_canonical_errors::CanonicalError;

use crate::domain::service::stream::sanitize::{error_detail, sanitize_error};
use crate::domain::service::stream::types::{ProviderFailure, StreamErrorCode};

/// Which side produced an error response (`ErrorSource` of the OAGW SDK).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The gateway itself.
    Gateway,
    /// The provider.
    Upstream,
}

/// A response that is not a 2xx `text/event-stream`, as the classifier sees
/// it.
#[derive(Debug, Clone, Copy)]
pub struct HttpResponseInfo<'a> {
    /// HTTP status.
    pub status: u16,
    /// `Retry-After` header text.
    pub retry_after: Option<&'a str>,
    /// `Content-Type` header text.
    pub content_type: Option<&'a str>,
    /// The `ErrorSource` extension, when present.
    pub source: Option<Source>,
    /// The (bounded) response body.
    pub body: &'a [u8],
}

/// A `proxy_request` call that returned an error: `provider_timeout` for a
/// timeout or deadline, `provider_error` with the generic text otherwise.
#[must_use]
// @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-classify-provider-error:p1:inst-if-gateway-timeout
pub fn classify_call_error(error: &CanonicalError) -> ProviderFailure {
    if matches!(error, CanonicalError::DeadlineExceeded { .. }) {
        ProviderFailure::generic(StreamErrorCode::ProviderTimeout)
    } else {
        ProviderFailure::generic(StreamErrorCode::ProviderError)
    }
}
// @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-classify-provider-error:p1:inst-if-gateway-timeout

// @cpt-dod:cpt-cf-mini-chat-dod-streaming-turns-error-mapping:p1
/// A response that is not a normal event stream; `None` for a 2xx
/// `text/event-stream`.
#[must_use]
pub fn classify_response(response: &HttpResponseInfo<'_>) -> Option<ProviderFailure> {
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-classify-provider-error:p1:inst-if-invalid-stream
    let success = (200..300).contains(&response.status);
    if success {
        let is_stream = response.content_type.is_some_and(|value| {
            value
                .trim()
                .to_ascii_lowercase()
                .starts_with("text/event-stream")
        });
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-classify-provider-error:p1:inst-return-invalid-stream
        return (!is_stream).then(|| ProviderFailure::generic(StreamErrorCode::ProviderError));
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-classify-provider-error:p1:inst-return-invalid-stream
    }
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-classify-provider-error:p1:inst-if-invalid-stream
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-classify-provider-error:p1:inst-return-gateway
    if response.status == 504 && response.source == Some(Source::Gateway) {
        return Some(ProviderFailure::generic(StreamErrorCode::ProviderTimeout));
    }
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-classify-provider-error:p1:inst-return-gateway
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-classify-provider-error:p1:inst-if-429
    if response.status == 429 {
        let generic = StreamErrorCode::RateLimited.generic_message();
        let message = match response
            .retry_after
            .and_then(|value| value.trim().parse::<u64>().ok())
        {
            Some(seconds) => format!("{generic} Retry after {seconds} seconds."),
            None => generic.to_owned(),
        };
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-classify-provider-error:p1:inst-return-429
        return Some(ProviderFailure {
            code: StreamErrorCode::RateLimited,
            message,
            detail: None,
            usage: None,
        });
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-classify-provider-error:p1:inst-return-429
    }
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-classify-provider-error:p1:inst-if-429
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-classify-provider-error:p1:inst-if-http-error
    Some(provider_body_failure(response.body))
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-classify-provider-error:p1:inst-if-http-error
}

/// The provider's own error body: its sanitized message, or the generic text.
// @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-classify-provider-error:p1:inst-return-http-error
fn provider_body_failure(body: &[u8]) -> ProviderFailure {
    let parsed: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let text = |value: &Value| value.as_str().map(str::to_owned);
    let message = text(&parsed["error"]["message"]).or_else(|| text(&parsed["message"]));
    let code = text(&parsed["error"]["code"])
        .or_else(|| text(&parsed["error"]["type"]))
        .or_else(|| text(&parsed["code"]));
    let Some(message) = message else {
        return ProviderFailure::generic(StreamErrorCode::ProviderError);
    };
    let sanitized = sanitize_error(&message);
    if sanitized.trim().is_empty() {
        return ProviderFailure::generic(StreamErrorCode::ProviderError);
    }
    ProviderFailure {
        code: StreamErrorCode::ProviderError,
        message: sanitized,
        detail: Some(error_detail(code.as_deref(), &message)),
        usage: None,
    }
}
// @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-classify-provider-error:p1:inst-return-http-error

/// A body read error: `provider_timeout` for a timeout (an `io::ErrorKind::TimedOut`,
/// a deadline or timeout canonical error), `provider_error` otherwise.
#[must_use]
pub fn classify_body_error(error: &(dyn std::error::Error + 'static)) -> ProviderFailure {
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(error) = current {
        let timed_out = error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::TimedOut)
            || error
                .downcast_ref::<CanonicalError>()
                .is_some_and(|canonical| {
                    matches!(canonical, CanonicalError::DeadlineExceeded { .. })
                })
            || error
                .downcast_ref::<tokio::time::error::Elapsed>()
                .is_some();
        if timed_out {
            return ProviderFailure::generic(StreamErrorCode::ProviderTimeout);
        }
        current = error.source();
    }
    ProviderFailure::generic(StreamErrorCode::ProviderError)
}

#[cfg(test)]
#[path = "classify_tests.rs"]
mod tests;
