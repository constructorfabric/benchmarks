//! Mapping of transport, gateway and HTTP failures of a provider call to [`ProviderError`].

use http::{HeaderMap, StatusCode};
use oagw_sdk::api::ErrorSource;
use serde_json::Value;
use toolkit_canonical_errors::CanonicalError;

use super::types::{ProviderError, ProviderErrorKind};
use crate::domain::error::DomainError;
use crate::domain::sanitize::sanitize_provider_message;

/// Provider error code that signals an over-long prompt.
const CONTEXT_LENGTH_CODE: &str = "context_length_exceeded";

/// Maximum length of a logged failure cause.
const MAX_LOGGED_CAUSE: usize = 512;

/// A failure cause fit for the logs: credentials (`Bearer …`, `sk-…`), URLs and provider ids
/// scrubbed ([`sanitize_provider_message`]), cut to [`MAX_LOGGED_CAUSE`] bytes.
pub(super) fn loggable(cause: &impl std::fmt::Display) -> String {
    let mut text = sanitize_provider_message(&cause.to_string());
    if text.len() > MAX_LOGGED_CAUSE {
        let mut cut = MAX_LOGGED_CAUSE;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("...");
    }
    text
}

/// `proxy_request` itself failed (pre-flight, auth, rate limit, timeout waiting for headers).
#[must_use]
pub fn from_canonical(err: &CanonicalError) -> ProviderError {
    // `Internal` keeps its cause in the context; its `detail` is a generic public text.
    let cause = match err {
        CanonicalError::Internal { ctx, .. } => format!("internal: {}", ctx.description),
        other => other.to_string(),
    };
    tracing::warn!(cause = %loggable(&cause), "provider call failed in OAGW before a response");
    match err {
        CanonicalError::DeadlineExceeded { .. } => {
            ProviderError::new(ProviderErrorKind::Timeout, "provider request timed out")
        }
        CanonicalError::ResourceExhausted { .. } => ProviderError::new(
            ProviderErrorKind::RateLimited {
                retry_after_secs: None,
            },
            "provider rate limit exceeded",
        ),
        _ => ProviderError::provider("provider request failed"),
    }
}

/// The S2S context is not available yet.
#[must_use]
pub fn from_s2s(err: &DomainError) -> ProviderError {
    ProviderError::provider(sanitize_provider_message(&err.to_string()))
}

/// A non-2xx response.
#[must_use]
pub fn from_http(
    source: Option<ErrorSource>,
    status: StatusCode,
    headers: &HeaderMap,
    body: &[u8],
) -> ProviderError {
    if source == Some(ErrorSource::Gateway) {
        tracing::warn!(status = status.as_u16(), detail = %loggable(&gateway_detail(body)),
            "OAGW returned a gateway error for a provider call");
        return if status == StatusCode::GATEWAY_TIMEOUT {
            ProviderError::new(ProviderErrorKind::Timeout, "provider request timed out")
        } else {
            ProviderError::provider(format!("provider gateway error ({})", status.as_u16()))
        };
    }
    let (code, message) = error_fields(body);
    if status == StatusCode::TOO_MANY_REQUESTS {
        return ProviderError::new(
            ProviderErrorKind::RateLimited {
                retry_after_secs: retry_after_secs(headers),
            },
            message.unwrap_or_else(|| "provider rate limit exceeded".to_owned()),
        );
    }
    let kind = if code.as_deref() == Some(CONTEXT_LENGTH_CODE) {
        ProviderErrorKind::ContextLengthExceeded
    } else {
        ProviderErrorKind::Provider
    };
    ProviderError::new(
        kind,
        message.unwrap_or_else(|| format!("provider returned HTTP {}", status.as_u16())),
    )
}

/// `detail` (or `title`) of an OAGW problem body, else the body as text.
fn gateway_detail(body: &[u8]) -> String {
    let problem = serde_json::from_slice::<Value>(body).ok();
    let field = |name| {
        problem
            .as_ref()
            .and_then(|p| p.get(name))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    field("detail")
        .or_else(|| field("title"))
        .unwrap_or_else(|| String::from_utf8_lossy(body).into_owned())
}

/// Numeric `Retry-After` (seconds); HTTP dates are not interpreted.
fn retry_after_secs(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(http::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// `error.code` and the sanitized `error.message` of a provider error body.
pub(crate) fn error_fields(body: &[u8]) -> (Option<String>, Option<String>) {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return (None, None);
    };
    let Some(error) = value.get("error") else {
        return (None, None);
    };
    let code = error.get("code").and_then(Value::as_str).map(str::to_owned);
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .filter(|m| !m.is_empty())
        .map(sanitize_provider_message);
    (code, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_error_body_is_sanitized_and_context_length_detected() {
        let body = serde_json::json!({"error": {"code": "context_length_exceeded", "message": "too long for file-abcdefghijklmnop"}});
        let err = from_http(
            Some(ErrorSource::Upstream),
            StatusCode::BAD_REQUEST,
            &HeaderMap::new(),
            body.to_string().as_bytes(),
        );
        assert_eq!(err.kind, ProviderErrorKind::ContextLengthExceeded);
        assert_eq!(err.message, "too long for [provider_id]");
        assert_eq!(err.sse_code(), "provider_error");
    }

    #[test]
    fn non_json_upstream_error_falls_back_to_status() {
        let err = from_http(
            None,
            StatusCode::BAD_GATEWAY,
            &HeaderMap::new(),
            b"<html>oops</html>",
        );
        assert_eq!(err.kind, ProviderErrorKind::Provider);
        assert_eq!(err.message, "provider returned HTTP 502");
    }

    #[test]
    fn logged_causes_are_scrubbed_and_bounded() {
        assert_eq!(
            loggable(&"denied: Bearer abc.def sk-abcdefghijklmnop"),
            "denied: [credential] [credential]"
        );
        let long = "\u{e9}".repeat(MAX_LOGGED_CAUSE);
        let cut = loggable(&long);
        assert!(cut.len() <= MAX_LOGGED_CAUSE + 3, "{}", cut.len());
        assert!(cut.ends_with("..."));
    }

    #[test]
    fn retry_after_must_be_numeric() {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::RETRY_AFTER,
            "Wed, 21 Oct 2026 07:28:00 GMT".parse().unwrap(),
        );
        let err = from_http(
            Some(ErrorSource::Upstream),
            StatusCode::TOO_MANY_REQUESTS,
            &headers,
            b"{}",
        );
        assert_eq!(
            err.kind,
            ProviderErrorKind::RateLimited {
                retry_after_secs: None
            }
        );
        assert_eq!(err.client_message(), "Provider rate limit exceeded");
    }
}
