//! OAGW error model.
//!
//! Every gateway-originated error is rendered as an RFC 9457
//! `application/problem+json` document carrying a GTS `type` identifier from
//! `DESIGN.md §3.3` (Error Response Format) plus the
//! `X-OAGW-Error-Source: gateway` header. Upstream responses are passed
//! through as-is with `X-OAGW-Error-Source: upstream` (see
//! `ADR/0007-error-source-distinction.md`).

use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;

/// Source of an OAGW response (`gateway` or `upstream`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorSource {
    /// Originated inside the gateway.
    Gateway,
    /// Passed through from the upstream service.
    Upstream,
}

impl ErrorSource {
    /// HTTP header value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gateway => "gateway",
            Self::Upstream => "upstream",
        }
    }

    /// Header name used to disambiguate the response origin.
    pub const HEADER: &'static str = "X-OAGW-Error-Source";
}

/// RFC 9457 problem detail payload.
///
/// The OAGW extension fields from `DESIGN.md §3.3` are rendered twice: flat at
/// the top level, as the design requires, and inside `context` — the canonical
/// error middleware (`toolkit::api::canonical_error_layer`) re-serializes every
/// `application/problem+json` body into the platform `Problem` envelope, which
/// drops fields it does not know. `context` is the one field it preserves.
#[derive(Debug, Clone, Serialize)]
pub struct Problem {
    /// GTS identifier of the error type.
    #[serde(rename = "type")]
    pub problem_type: String,
    /// Human-readable summary.
    pub title: String,
    /// HTTP status.
    pub status: u16,
    /// Occurrence-specific explanation.
    pub detail: String,
    /// URI reference for this occurrence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    /// Trace correlation identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// OAGW-specific extension fields, mirrored into `context` for the
    /// canonical error middleware.
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<serde_json::Map<String, serde_json::Value>>,
    /// Request context carried on the platform envelope.
    pub context: serde_json::Value,
}

/// Every error OAGW can produce.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{detail}")]
pub struct OagwError {
    /// Error discriminator.
    pub kind: ErrorKind,
    /// Human-readable explanation specific to this occurrence.
    pub detail: String,
    /// Request-context extensions (`upstream_id`, `host`, `path`, …).
    pub extensions: serde_json::Map<String, serde_json::Value>,
    /// `Retry-After` hint in seconds, when applicable.
    pub retry_after: Option<u64>,
}

/// Error discriminators with their canonical HTTP status + GTS type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// 400 — general route validation error.
    RouteError,
    /// 400 — request validation failed.
    ValidationError,
    /// 400 — `X-OAGW-Target-Host` required.
    MissingTargetHost,
    /// 400 — `X-OAGW-Target-Host` format invalid.
    InvalidTargetHost,
    /// 400 — `X-OAGW-Target-Host` does not match a configured endpoint.
    UnknownTargetHost,
    /// 400 — unsupported transfer encoding.
    TransferEncodingUnsupported,
    /// 401 — authentication to upstream failed.
    AuthenticationFailed,
    /// 403 — CORS origin rejected.
    CorsOriginNotAllowed,
    /// 403 — CORS method rejected.
    CorsMethodNotAllowed,
    /// 404 — no matching route found.
    RouteNotFound,
    /// 409 — plugin still referenced by an upstream or route.
    PluginInUse,
    /// 409 — resource already exists.
    AlreadyExists,
    /// 413 — request payload exceeds the limit.
    PayloadTooLarge,
    /// 429 — rate limit exceeded.
    RateLimitExceeded,
    /// 500 — referenced secret not found.
    SecretNotFound,
    /// 502 — protocol-level error.
    ProtocolError,
    /// 502 — upstream service error.
    DownstreamError,
    /// 502 — stream connection aborted.
    StreamAborted,
    /// 503 — upstream link unavailable.
    LinkUnavailable,
    /// 503 — circuit breaker open.
    CircuitBreakerOpen,
    /// 503 — plugin not found.
    PluginNotFound,
    /// 503 — upstream disabled.
    UpstreamDisabled,
    /// 504 — connection timeout.
    ConnectionTimeout,
    /// 504 — request timeout.
    RequestTimeout,
    /// 504 — idle timeout on a streaming response.
    IdleTimeout,
}

impl ErrorKind {
    /// Canonical HTTP status for this error kind.
    #[must_use]
    pub fn status(self) -> StatusCode {
        match self {
            Self::RouteError
            | Self::ValidationError
            | Self::MissingTargetHost
            | Self::InvalidTargetHost
            | Self::UnknownTargetHost
            | Self::TransferEncodingUnsupported => StatusCode::BAD_REQUEST,
            Self::AuthenticationFailed => StatusCode::UNAUTHORIZED,
            Self::CorsOriginNotAllowed | Self::CorsMethodNotAllowed => StatusCode::FORBIDDEN,
            Self::RouteNotFound => StatusCode::NOT_FOUND,
            Self::PluginInUse | Self::AlreadyExists => StatusCode::CONFLICT,
            Self::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::RateLimitExceeded => StatusCode::TOO_MANY_REQUESTS,
            Self::SecretNotFound => StatusCode::INTERNAL_SERVER_ERROR,
            Self::ProtocolError
            | Self::DownstreamError
            | Self::StreamAborted => StatusCode::BAD_GATEWAY,
            Self::LinkUnavailable
            | Self::CircuitBreakerOpen
            | Self::PluginNotFound
            | Self::UpstreamDisabled => StatusCode::SERVICE_UNAVAILABLE,
            Self::ConnectionTimeout | Self::RequestTimeout | Self::IdleTimeout => {
                StatusCode::GATEWAY_TIMEOUT
            }
        }
    }

    /// Canonical GTS instance ID for this error kind.
    #[must_use]
    pub fn gts_type(self) -> &'static str {
        match self {
            Self::RouteError => "gts.cf.core.errors.err.v1~cf.oagw.route.error.v1",
            Self::ValidationError => "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
            Self::MissingTargetHost => "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1",
            Self::InvalidTargetHost => "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1",
            Self::UnknownTargetHost => "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1",
            Self::TransferEncodingUnsupported => "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
            Self::AuthenticationFailed => "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1",
            Self::CorsOriginNotAllowed => "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1",
            Self::CorsMethodNotAllowed => "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1",
            Self::RouteNotFound => "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1",
            Self::PluginInUse => "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1",
            Self::AlreadyExists => "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
            Self::PayloadTooLarge => "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1",
            Self::RateLimitExceeded => "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1",
            Self::SecretNotFound => "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1",
            Self::ProtocolError => "gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1",
            Self::DownstreamError => "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1",
            Self::StreamAborted => "gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1",
            Self::LinkUnavailable => "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1",
            Self::CircuitBreakerOpen => "gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1",
            Self::PluginNotFound => "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1",
            Self::UpstreamDisabled => "gts.cf.core.errors.err.v1~cf.oagw.upstream.disabled.v1",
            Self::ConnectionTimeout => "gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1",
            Self::RequestTimeout => "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1",
            Self::IdleTimeout => "gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1",
        }
    }

    /// Human-readable RFC 9457 `title`.
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::RouteError => "Route Error",
            Self::ValidationError => "Validation Error",
            Self::MissingTargetHost => "Missing Target Host",
            Self::InvalidTargetHost => "Invalid Target Host",
            Self::UnknownTargetHost => "Unknown Target Host",
            Self::TransferEncodingUnsupported => "Unsupported Transfer Encoding",
            Self::AuthenticationFailed => "Authentication Failed",
            Self::CorsOriginNotAllowed => "Origin Not Allowed",
            Self::CorsMethodNotAllowed => "Method Not Allowed",
            Self::RouteNotFound => "Route Not Found",
            Self::PluginInUse => "Plugin In Use",
            Self::AlreadyExists => "Already Exists",
            Self::PayloadTooLarge => "Payload Too Large",
            Self::RateLimitExceeded => "Rate Limit Exceeded",
            Self::SecretNotFound => "Secret Not Found",
            Self::ProtocolError => "Protocol Error",
            Self::DownstreamError => "Downstream Error",
            Self::StreamAborted => "Stream Aborted",
            Self::LinkUnavailable => "Link Unavailable",
            Self::CircuitBreakerOpen => "Circuit Breaker Open",
            Self::PluginNotFound => "Plugin Not Found",
            Self::UpstreamDisabled => "Upstream Disabled",
            Self::ConnectionTimeout => "Connection Timeout",
            Self::RequestTimeout => "Request Timeout",
            Self::IdleTimeout => "Idle Timeout",
        }
    }
}

impl OagwError {
    /// Construct an error of `kind` with `detail`.
    #[must_use]
    pub fn new(kind: ErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
            extensions: serde_json::Map::new(),
            retry_after: None,
        }
    }

    /// Attach an RFC 9457 extension field.
    #[must_use]
    pub fn with_ext(mut self, key: &str, value: impl Into<serde_json::Value>) -> Self {
        self.extensions.insert(key.to_owned(), value.into());
        self
    }

    /// Attach the `Retry-After` hint (seconds).
    #[must_use]
    pub fn with_retry_after(mut self, seconds: u64) -> Self {
        self.retry_after = Some(seconds);
        self
    }

    /// Render the RFC 9457 body.
    #[must_use]
    pub fn problem(&self) -> Problem {
        let mut extensions = self.extensions.clone();
        if let Some(retry_after) = self.retry_after {
            extensions
                .entry("retry_after_seconds".to_owned())
                .or_insert_with(|| serde_json::Value::from(retry_after));
        }
        let context = serde_json::Value::Object(extensions.clone());
        Problem {
            problem_type: self.kind.gts_type().to_owned(),
            title: self.kind.title().to_owned(),
            status: self.kind.status().as_u16(),
            detail: self.detail.clone(),
            instance: None,
            trace_id: None,
            extensions: if extensions.is_empty() {
                None
            } else {
                Some(extensions)
            },
            context,
        }
    }

    /// Build the full gateway-error response, including
    /// `X-OAGW-Error-Source: gateway` and `Retry-After` when set.
    #[must_use]
    pub fn into_response_with(self, request_id: Option<&str>) -> Response {
        let status = self.kind.status();
        let problem = self.problem();
        let mut headers = HeaderMap::new();
        headers.insert(
            ErrorSource::HEADER,
            HeaderValue::from_static("gateway"),
        );
        if let Some(Ok(v)) = self.retry_after.map(|s| HeaderValue::from_str(&s.to_string())) {
            headers.insert(header::RETRY_AFTER, v);
        }
        if let Some(Ok(v)) = request_id.map(HeaderValue::from_str) {
            headers.insert("x-request-id", v);
        }
        let body = serde_json::to_string(&problem).unwrap_or_else(|_| {
            format!(
                r#"{{"type":"{}","status":{},"context":{{}}}}"#,
                ErrorKind::ValidationError.gts_type(),
                status.as_u16()
            )
        });
        let mut response = Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/problem+json");
        for (name, value) in headers.iter() {
            response = response.header(name, value);
        }
        response
            .body(axum::body::Body::from(body))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
    }
}

impl IntoResponse for OagwError {
    fn into_response(self) -> Response {
        self.into_response_with(None)
    }
}

/// Convenience alias for handler results.
pub type ApiResult<T> = Result<T, OagwError>;

/// Result alias used across the OAGW handlers.
pub type OagwResult<T> = Result<T, OagwError>;

/// Attach `X-OAGW-Error-Source: upstream` to a passthrough response.
#[must_use]
pub fn with_upstream_source(mut response: Response) -> Response {
    response.headers_mut().insert(
        ErrorSource::HEADER,
        HeaderValue::from_static("upstream"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn problem_carries_gts_type_and_status() {
        let e = OagwError::new(
            ErrorKind::RouteNotFound,
            "no route matched GET /v1/unknown",
        )
        .with_ext("path", "/v1/unknown");
        let p = e.problem();
        assert_eq!(p.status, 404);
        assert_eq!(p.problem_type, "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1");
        assert_eq!(p.title, "Route Not Found");
        assert_eq!(p.extensions.as_ref().and_then(|e| e.get("path")).cloned(),
            Some(serde_json::Value::from("/v1/unknown")));
    }

    #[test]
    fn rate_limit_sets_retry_after_header() {
        let resp = OagwError::new(ErrorKind::RateLimitExceeded, "bucket drained")
            .with_retry_after(3)
            .into_response_with(Some("req-1"));
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            resp.headers().get(ErrorSource::HEADER),
            Some(&HeaderValue::from_static("gateway"))
        );
        assert_eq!(resp.headers().get(header::RETRY_AFTER).unwrap(), "3");
    }

    #[test]
    fn upstream_source_header_is_set() {
        let resp = Response::builder()
            .status(200)
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = with_upstream_source(resp);
        assert_eq!(
            resp.headers().get(ErrorSource::HEADER),
            Some(&HeaderValue::from_static("upstream"))
        );
    }

    #[test]
    fn all_kinds_have_unique_titles() {
        let kinds = [
            ErrorKind::RouteError,
            ErrorKind::ValidationError,
            ErrorKind::MissingTargetHost,
            ErrorKind::InvalidTargetHost,
            ErrorKind::UnknownTargetHost,
            ErrorKind::TransferEncodingUnsupported,
            ErrorKind::AuthenticationFailed,
            ErrorKind::CorsOriginNotAllowed,
            ErrorKind::CorsMethodNotAllowed,
            ErrorKind::RouteNotFound,
            ErrorKind::PluginInUse,
            ErrorKind::AlreadyExists,
            ErrorKind::PayloadTooLarge,
            ErrorKind::RateLimitExceeded,
            ErrorKind::SecretNotFound,
            ErrorKind::ProtocolError,
            ErrorKind::DownstreamError,
            ErrorKind::StreamAborted,
            ErrorKind::LinkUnavailable,
            ErrorKind::CircuitBreakerOpen,
            ErrorKind::PluginNotFound,
            ErrorKind::UpstreamDisabled,
            ErrorKind::ConnectionTimeout,
            ErrorKind::RequestTimeout,
            ErrorKind::IdleTimeout,
        ];
        for k in kinds {
            assert!(k.gts_type().starts_with("gts.cf.core.errors.err.v1~cf.oagw."));
            assert!(!k.title().is_empty());
        }
    }
}
