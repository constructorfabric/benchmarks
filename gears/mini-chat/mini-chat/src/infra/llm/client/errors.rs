//! Error mapping for provider calls: upstream HTTP error bodies, gateway (`CanonicalError`)
//! failures and in-stream provider errors, all reduced to sanitized [`LlmFailure`]s.

use std::time::Duration;

use bytes::Bytes;
use http::{HeaderMap, StatusCode};
use oagw_sdk::Body;
use oagw_sdk::api::ErrorSource;
use serde_json::Value;
use toolkit_canonical_errors::CanonicalError;

use super::super::LlmFailure;
use super::super::sanitize::sanitize_provider_message;
use crate::domain::error::stream_codes::{PROVIDER_ERROR, PROVIDER_TIMEOUT, RATE_LIMITED};
use mini_chat_sdk::UsageTokens;

/// Generic client-safe message for gateway / transport failures.
pub(crate) const UNAVAILABLE_MESSAGE: &str = "Provider is currently unavailable";
/// Message of a provider / gateway timeout.
pub(crate) const TIMEOUT_MESSAGE: &str = "Provider request timed out";
/// Message when the transport ended without a terminal provider event.
pub(crate) const STREAM_ENDED_MESSAGE: &str = "Provider stream ended unexpectedly";
/// Message of an unparseable provider response.
pub(crate) const INVALID_RESPONSE_MESSAGE: &str = "Provider returned an invalid response";

/// Upper bound for reading an error / JSON response body.
const BODY_READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Bodies are truncated to this size (error bodies and JSON responses are small).
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Builds a failure with a sanitized message.
pub(crate) fn failure(code: &'static str, message: &str) -> LlmFailure {
    LlmFailure {
        code,
        message: sanitize_provider_message(message),
        usage: None,
        response_id: None,
        context_length_exceeded: false,
    }
}

/// Reads a (finite) response body with a timeout; a failing or hanging body yields what was
/// read so far as empty bytes.
pub(crate) async fn read_body(body: Body) -> Bytes {
    match tokio::time::timeout(BODY_READ_TIMEOUT, body.into_bytes()).await {
        Ok(Ok(b)) if b.len() > MAX_BODY_BYTES => b.slice(..MAX_BODY_BYTES),
        Ok(Ok(b)) => b,
        Ok(Err(e)) => {
            tracing::debug!(error = %e, "failed to read provider response body");
            Bytes::new()
        }
        Err(_) => {
            tracing::debug!("timed out reading provider response body");
            Bytes::new()
        }
    }
}

/// Provider error `code` + `message` extracted from a JSON error payload.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct ProviderError {
    pub code: Option<String>,
    pub message: Option<String>,
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn from_error_object(e: &Value) -> ProviderError {
    ProviderError {
        code: str_field(e, "code").or_else(|| str_field(e, "type")),
        message: str_field(e, "message"),
    }
}

/// Extracts the provider error from a JSON value, trying in order: `response.error`,
/// top-level `error` (object or string), flat `{code, message}`, and a Problem `detail`.
pub(crate) fn extract_provider_error(v: &Value) -> Option<ProviderError> {
    if let Some(e) = v.pointer("/response/error").filter(|e| e.is_object()) {
        return Some(from_error_object(e));
    }
    match v.get("error") {
        Some(e) if e.is_object() => return Some(from_error_object(e)),
        Some(Value::String(s)) => {
            return Some(ProviderError {
                code: str_field(v, "code"),
                message: Some(s.clone()),
            });
        }
        _ => {}
    }
    if v.get("message").is_some_and(Value::is_string) {
        return Some(ProviderError {
            code: str_field(v, "code"),
            message: str_field(v, "message"),
        });
    }
    if v.get("detail").is_some_and(Value::is_string) {
        return Some(ProviderError {
            code: None,
            message: str_field(v, "detail"),
        });
    }
    None
}

/// True when the provider error denotes a context-length overflow.
pub(crate) fn is_context_length_exceeded(e: &ProviderError) -> bool {
    if e.code.as_deref() == Some("context_length_exceeded") {
        return true;
    }
    e.message.as_deref().is_some_and(|m| {
        let m = m.to_ascii_lowercase();
        m.contains("context_length_exceeded")
            || m.contains("maximum context length")
            || m.contains("prompt is too long")
            || m.contains("context window")
    })
}

/// In-stream provider failure (`response.failed`, SSE `error`): `provider_error` with the
/// sanitized provider message. `raw_fallback` is used when nothing could be parsed.
pub(crate) fn stream_failure(
    v: Option<&Value>,
    raw_fallback: Option<&str>,
    usage: Option<UsageTokens>,
    response_id: Option<String>,
) -> LlmFailure {
    let err = v.and_then(extract_provider_error).unwrap_or_default();
    let message = err
        .message
        .clone()
        .filter(|m| !m.trim().is_empty())
        .or_else(|| {
            raw_fallback
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "Provider request failed".to_owned());
    tracing::warn!(
        provider_code = err.code.as_deref().unwrap_or(""),
        "provider reported a stream failure"
    );
    LlmFailure {
        code: PROVIDER_ERROR,
        message: sanitize_provider_message(&message),
        usage,
        response_id,
        context_length_exceeded: is_context_length_exceeded(&ProviderError {
            code: err.code,
            message: Some(message),
        }),
    }
}

fn retry_after_secs(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(http::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
}

/// Maps a non-success HTTP response (upstream or gateway) to a failure.
pub(crate) fn http_failure(
    status: StatusCode,
    headers: &HeaderMap,
    source: Option<ErrorSource>,
    body: &[u8],
) -> LlmFailure {
    let json: Option<Value> = serde_json::from_slice(body).ok();
    let provider_err = json.as_ref().and_then(extract_provider_error);
    let ctx_exceeded = provider_err
        .as_ref()
        .is_some_and(is_context_length_exceeded);
    tracing::warn!(
        status = status.as_u16(),
        source = source.map_or("unknown", ErrorSource::as_str),
        provider_code = provider_err
            .as_ref()
            .and_then(|e| e.code.as_deref())
            .unwrap_or(""),
        "provider call failed"
    );

    if status == StatusCode::TOO_MANY_REQUESTS {
        let message = match retry_after_secs(headers) {
            Some(s) => format!("Provider rate limit exceeded; retry after {s} seconds"),
            None => "Provider rate limit exceeded".to_owned(),
        };
        return failure(RATE_LIMITED, &message);
    }

    // A JSON `error` object (OpenAI / Anthropic style) is the provider's own error body.
    let provider_json_error = json
        .as_ref()
        .is_some_and(|v| v.get("error").is_some_and(Value::is_object));

    if status == StatusCode::GATEWAY_TIMEOUT
        && (source == Some(ErrorSource::Gateway) || !provider_json_error)
    {
        return failure(PROVIDER_TIMEOUT, TIMEOUT_MESSAGE);
    }

    if source == Some(ErrorSource::Gateway) && !provider_json_error {
        return failure(PROVIDER_ERROR, UNAVAILABLE_MESSAGE);
    }

    let message = provider_err
        .and_then(|e| e.message)
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| format!("Provider returned HTTP {}", status.as_u16()));
    let mut f = failure(PROVIDER_ERROR, &message);
    f.context_length_exceeded = ctx_exceeded;
    f
}

/// Maps a gateway failure (`Err(CanonicalError)` from `proxy_request`).
pub(crate) fn gateway_failure(err: &CanonicalError) -> LlmFailure {
    tracing::warn!(error = %err.detail(), category = err.title(), "OAGW proxy call failed");
    match err {
        CanonicalError::DeadlineExceeded { .. } => failure(PROVIDER_TIMEOUT, TIMEOUT_MESSAGE),
        _ => failure(PROVIDER_ERROR, UNAVAILABLE_MESSAGE),
    }
}

/// Message for a storage call error: provider message, else `HTTP {status}`; sanitized.
pub(crate) fn storage_error_message(status: StatusCode, body: &[u8]) -> String {
    let msg = serde_json::from_slice::<Value>(body)
        .ok()
        .as_ref()
        .and_then(extract_provider_error)
        .and_then(|e| e.message)
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| format!("HTTP {}", status.as_u16()));
    sanitize_provider_message(&msg)
}
