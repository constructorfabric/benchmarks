//! Thin client over the in-process OAGW proxy (`ServiceGatewayClientV1`).
//!
//! Upstreams are provisioned by the gear under its S2S identity, so every
//! provider request is proxied with the S2S security context (obtained at
//! gear start). Calls made before the context is available wait briefly.

#![allow(clippy::result_large_err)]

use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwapOption;
use bytes::Bytes;
use oagw_sdk::api::{ErrorSource, ServiceGatewayClientV1};
use oagw_sdk::body::Body;
use tokio::sync::watch;
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;

use super::types::{ProviderErrorKind, ProviderFailure};

/// How long a provider call waits for the S2S context at startup.
const S2S_WAIT: Duration = Duration::from_secs(15);

/// Holder of the S2S security context used for proxy calls.
pub struct S2sContext {
    current: ArcSwapOption<SecurityContext>,
    ready_tx: watch::Sender<bool>,
}

impl Default for S2sContext {
    fn default() -> Self {
        let (ready_tx, _) = watch::channel(false);
        Self {
            current: ArcSwapOption::empty(),
            ready_tx,
        }
    }
}

impl S2sContext {
    /// `true` once the context was obtained.
    #[must_use]
    pub fn is_set(&self) -> bool {
        self.current.load().is_some()
    }

    pub fn set(&self, ctx: SecurityContext) {
        self.current.store(Some(Arc::new(ctx)));
        self.ready_tx.send_replace(true);
    }

    /// The S2S context, waiting up to 15 s for gear start to obtain it.
    ///
    /// # Errors
    /// The context is not available.
    pub async fn get(&self) -> Result<SecurityContext, String> {
        if let Some(ctx) = self.current.load_full() {
            return Ok((*ctx).clone());
        }
        let mut rx = self.ready_tx.subscribe();
        if tokio::time::timeout(S2S_WAIT, rx.wait_for(|ready| *ready))
            .await
            .is_err()
        {
            tracing::debug!("mini-chat: waiting for the S2S context timed out");
        }
        self.current
            .load_full()
            .map(|c| (*c).clone())
            .ok_or_else(|| "service-to-service context not available yet".to_owned())
    }
}

/// A buffered (non-streaming) proxy response.
#[derive(Debug, Clone)]
pub struct BufferedResponse {
    pub status: u16,
    pub headers: http::HeaderMap,
    pub body: Bytes,
}

impl BufferedResponse {
    #[must_use]
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// Parse the body as JSON.
    ///
    /// # Errors
    /// Invalid JSON.
    pub fn json(&self) -> Result<serde_json::Value, String> {
        serde_json::from_slice(&self.body).map_err(|e| format!("invalid JSON response: {e}"))
    }
}

/// Proxy client.
#[derive(Clone)]
pub struct ProxyClient {
    gateway: Arc<dyn ServiceGatewayClientV1>,
    s2s: Arc<S2sContext>,
}

impl ProxyClient {
    #[must_use]
    pub fn new(gateway: Arc<dyn ServiceGatewayClientV1>, s2s: Arc<S2sContext>) -> Self {
        Self { gateway, s2s }
    }

    #[must_use]
    pub fn gateway(&self) -> &Arc<dyn ServiceGatewayClientV1> {
        &self.gateway
    }

    /// Send a request through OAGW and return the raw response.
    ///
    /// # Errors
    /// Gateway failure (classified as timeout / provider error).
    pub async fn send(
        &self,
        request: http::Request<Body>,
    ) -> Result<http::Response<Body>, ProviderFailure> {
        let ctx = self.s2s.get().await.map_err(ProviderFailure::provider)?;
        self.gateway
            .proxy_request(ctx, request)
            .await
            .map_err(|e| classify_gateway_error(&e))
    }

    /// Send a JSON request and buffer the response.
    ///
    /// # Errors
    /// Gateway failure or unreadable body.
    pub async fn send_json(
        &self,
        method: http::Method,
        uri: &str,
        body: Option<&serde_json::Value>,
        extra_headers: &[(&str, &str)],
    ) -> Result<BufferedResponse, ProviderFailure> {
        let mut builder = http::Request::builder().method(method).uri(uri);
        for (k, v) in extra_headers {
            builder = builder.header(*k, *v);
        }
        let body = match body {
            Some(json) => {
                builder = builder.header(http::header::CONTENT_TYPE, "application/json");
                Body::Bytes(Bytes::from(json.to_string()))
            }
            None => Body::Empty,
        };
        let request = builder
            .body(body)
            .map_err(|e| ProviderFailure::provider(format!("invalid request: {e}")))?;
        let response = self.send(request).await?;
        buffer(response).await
    }
}

/// Buffer a proxy response.
///
/// # Errors
/// Unreadable body.
pub async fn buffer(response: http::Response<Body>) -> Result<BufferedResponse, ProviderFailure> {
    let (parts, body) = response.into_parts();
    let gateway_source = parts.extensions.get::<ErrorSource>().copied();
    let bytes = body
        .into_bytes()
        .await
        .map_err(|e| ProviderFailure::provider(format!("failed to read provider response: {e}")))?;
    let mut headers = parts.headers;
    if gateway_source == Some(ErrorSource::Gateway) {
        headers.insert(
            "x-mini-chat-error-source",
            http::HeaderValue::from_static("gateway"),
        );
    }
    Ok(BufferedResponse {
        status: parts.status.as_u16(),
        headers,
        body: bytes,
    })
}

/// Classify a gateway-level `CanonicalError`.
#[must_use]
pub fn classify_gateway_error(err: &CanonicalError) -> ProviderFailure {
    match err {
        CanonicalError::DeadlineExceeded { .. } => {
            ProviderFailure::timeout("provider request timed out")
        }
        other => ProviderFailure::provider(format!("gateway error: {}", other.detail())),
    }
}

fn header_str(headers: &http::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

/// Extract `(code, message)` from a provider or gateway error body.
#[must_use]
pub fn parse_error_body(body: &[u8]) -> (Option<String>, String) {
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(body) else {
        let text = String::from_utf8_lossy(body).trim().to_owned();
        return (
            None,
            if text.is_empty() {
                "provider error".to_owned()
            } else {
                text
            },
        );
    };
    let err_obj = json
        .get("error")
        .filter(|e| e.is_object())
        .or_else(|| json.get("response").and_then(|r| r.get("error")))
        .unwrap_or(&json);
    let code = err_obj
        .get("code")
        .and_then(|c| {
            c.as_str()
                .map(str::to_owned)
                .or_else(|| Some(c.to_string()))
        })
        .filter(|c| c != "null")
        .or_else(|| {
            err_obj
                .get("type")
                .and_then(|t| t.as_str())
                .map(str::to_owned)
        });
    let message = err_obj
        .get("message")
        .and_then(|m| m.as_str())
        .or_else(|| json.get("detail").and_then(|d| d.as_str()))
        .or_else(|| json.get("error").and_then(|e| e.as_str()))
        .map_or_else(|| json.to_string(), str::to_owned);
    (code, message)
}

/// Classify a non-success HTTP response of a provider request.
#[must_use]
pub fn classify_http_failure(resp: &BufferedResponse) -> ProviderFailure {
    let (code, message) = parse_error_body(&resp.body);
    let from_gateway = header_str(&resp.headers, "x-mini-chat-error-source").is_some();
    let is_problem =
        header_str(&resp.headers, "content-type").is_some_and(|ct| ct.contains("problem+json"));
    let problem_deadline = is_problem
        && serde_json::from_slice::<serde_json::Value>(&resp.body)
            .ok()
            .and_then(|j| j.get("type").and_then(|t| t.as_str()).map(str::to_owned))
            .is_some_and(|t| t.contains("deadline_exceeded"));
    let kind = if resp.status == 429 {
        ProviderErrorKind::RateLimited {
            retry_after_secs: header_str(&resp.headers, "retry-after")
                .and_then(|v| v.trim().parse::<u64>().ok()),
        }
    } else if resp.status == 504 && (from_gateway || problem_deadline) {
        ProviderErrorKind::Timeout
    } else {
        ProviderErrorKind::Provider
    };
    ProviderFailure {
        kind,
        message,
        provider_code: code,
        usage: None,
        response_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_body_parsing_handles_common_shapes() {
        let (code, msg) =
            parse_error_body(br#"{"error":{"code":"rate_limit","message":"slow down"}}"#);
        assert_eq!(code.as_deref(), Some("rate_limit"));
        assert_eq!(msg, "slow down");
        let (_, msg) = parse_error_body(br#"{"message":"flat"}"#);
        assert_eq!(msg, "flat");
        let (_, msg) = parse_error_body(b"plain text failure");
        assert_eq!(msg, "plain text failure");
    }

    #[test]
    fn http_failures_are_classified() {
        let mut headers = http::HeaderMap::new();
        headers.insert("retry-after", http::HeaderValue::from_static("7"));
        let resp = BufferedResponse {
            status: 429,
            headers,
            body: Bytes::from_static(br#"{"error":{"message":"too many"}}"#),
        };
        assert_eq!(
            classify_http_failure(&resp).kind,
            ProviderErrorKind::RateLimited {
                retry_after_secs: Some(7)
            }
        );
        let resp = BufferedResponse {
            status: 504,
            headers: http::HeaderMap::new(),
            body: Bytes::from_static(br#"{"error":{"message":"upstream"}}"#),
        };
        assert_eq!(
            classify_http_failure(&resp).kind,
            ProviderErrorKind::Provider
        );
        let mut headers = http::HeaderMap::new();
        headers.insert(
            "x-mini-chat-error-source",
            http::HeaderValue::from_static("gateway"),
        );
        let resp = BufferedResponse {
            status: 504,
            headers,
            body: Bytes::new(),
        };
        assert_eq!(
            classify_http_failure(&resp).kind,
            ProviderErrorKind::Timeout
        );
    }
}
