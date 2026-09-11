//! OAGW error type.
//!
//! Every gateway-generated error is an `RFC 9457` `application/problem+json`
//! document carrying the documented GTS error identifier as `type`, the
//! `title`, `status`, `detail` and `instance` members, OAGW extension members
//! inside `context`, and the `X-OAGW-Error-Source: gateway` marker header.

use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use toolkit_canonical_errors::Problem;

/// Marker header distinguishing gateway-generated errors from upstream ones.
pub const ERROR_SOURCE_HEADER: &str = "X-OAGW-Error-Source";
/// Value used when the gateway produced the error.
pub const SOURCE_GATEWAY: &str = "gateway";
/// Value used when the response came from the upstream.
pub const SOURCE_UPSTREAM: &str = "upstream";

/// `Content-Type` used for `RFC 9457` problem documents.
pub const PROBLEM_CONTENT_TYPE: &str = "application/problem+json";

pub(crate) static ERROR_SOURCE_NAME: HeaderName = HeaderName::from_static("x-oagw-error-source");
static RETRY_AFTER_NAME: HeaderName = HeaderName::from_static("retry-after");

/// Which side produced an error response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorSource {
    /// The gateway produced it.
    Gateway,
    /// The upstream produced it.
    Upstream,
}

impl ErrorSource {
    /// Header value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gateway => SOURCE_GATEWAY,
            Self::Upstream => SOURCE_UPSTREAM,
        }
    }
}

/// An OAGW gateway error.
#[derive(Debug, Clone)]
pub struct OagwError {
    problem: Problem,
    retry_after: Option<String>,
    rate_limit: Option<RateLimitHeaders>,
    source: ErrorSource,
    status: StatusCode,
}

/// Rate limit headers attached to a `429`.
#[derive(Debug, Clone, Copy)]
struct RateLimitHeaders {
    limit: u32,
    remaining: u32,
    reset: u64,
}

impl OagwError {
    /// Builds a gateway error.
    #[must_use]
    pub fn new(
        status: StatusCode,
        error_type: &str,
        title: &str,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            problem: Problem {
                problem_type: error_type.to_owned(),
                title: title.to_owned(),
                status: status.as_u16(),
                detail: detail.into(),
                instance: None,
                trace_id: None,
                context: serde_json::Value::Object(serde_json::Map::new()),
                error_code: None,
                error_domain: None,
            },
            retry_after: None,
            rate_limit: None,
            source: ErrorSource::Gateway,
            status,
        }
    }

    /// Adds an OAGW extension member inside the problem `context`.
    #[must_use]
    pub fn with_extension(mut self, key: &str, value: serde_json::Value) -> Self {
        if let serde_json::Value::Object(map) = &mut self.problem.context {
            map.insert(key.to_owned(), value);
        }
        self
    }

    /// Adds a machine-readable error code.
    #[must_use]
    pub fn with_code(mut self, code: &str) -> Self {
        self.problem.error_code = Some(code.to_owned());
        self
    }

    /// Sets `Retry-After` in seconds.
    #[must_use]
    pub fn with_retry_after(mut self, seconds: u64) -> Self {
        self.retry_after = Some(seconds.to_string());
        self
    }

    /// Attaches the `X-RateLimit-*` headers.
    #[must_use]
    pub fn with_rate_limit_headers(mut self, limit: u32, remaining: u32, reset: u64) -> Self {
        self.rate_limit = Some(RateLimitHeaders { limit, remaining, reset });
        self
    }

    /// Overrides the error source marker.
    #[must_use]
    pub fn with_source(mut self, source: ErrorSource) -> Self {
        self.source = source;
        self
    }

    /// Sets the `detail` text.
    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.problem.detail = detail.into();
        self
    }

    /// Sets the `instance` member.
    #[must_use]
    pub fn with_instance(mut self, instance: impl Into<String>) -> Self {
        self.problem.instance = Some(instance.into());
        self
    }

    /// The problem document.
    #[must_use]
    pub fn problem(&self) -> &Problem {
        &self.problem
    }

    /// The response status.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        self.status
    }

    /// The machine-readable error code, if any.
    #[must_use]
    pub fn error_code(&self) -> Option<&str> {
        self.problem.error_code.as_deref()
    }

    /// `400 ValidationError`.
    #[must_use]
    pub fn validation(detail: impl Into<String>) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            crate::domain::ids::ERR_VALIDATION,
            "Validation Error",
            detail,
        )
        .with_code("VALIDATION_ERROR")
    }

    /// `404 RouteNotFound`.
    #[must_use]
    pub fn route_not_found(detail: impl Into<String>) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            crate::domain::ids::ERR_ROUTE_NOT_FOUND,
            "Route Not Found",
            detail,
        )
        .with_code("ROUTE_NOT_FOUND")
    }

    /// `404` resource not found.
    #[must_use]
    pub fn not_found(error_type: &str, title: &str, detail: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, error_type, title, detail).with_code("NOT_FOUND")
    }

    /// `409 Conflict`.
    #[must_use]
    pub fn conflict(error_type: &str, title: &str, detail: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, error_type, title, detail).with_code("CONFLICT")
    }

    /// `413 PayloadTooLarge`.
    #[must_use]
    pub fn payload_too_large(detail: impl Into<String>) -> Self {
        Self::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            crate::domain::ids::ERR_PAYLOAD_TOO_LARGE,
            "Payload Too Large",
            detail,
        )
        .with_code("PAYLOAD_TOO_LARGE")
    }

    /// `429 RateLimitExceeded`.
    #[must_use]
    pub fn rate_limit_exceeded(detail: impl Into<String>) -> Self {
        Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            crate::domain::ids::ERR_RATE_LIMIT,
            "Rate Limit Exceeded",
            detail,
        )
        .with_code("RATE_LIMIT_EXCEEDED")
    }

    /// `500 SecretNotFound`.
    #[must_use]
    pub fn secret_not_found(detail: impl Into<String>) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            crate::domain::ids::ERR_SECRET_NOT_FOUND,
            "Secret Not Found",
            detail,
        )
        .with_code("SECRET_NOT_FOUND")
    }

    /// `401 AuthenticationFailed`.
    #[must_use]
    pub fn auth_failed(detail: impl Into<String>) -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            crate::domain::ids::ERR_AUTH_FAILED,
            "Authentication Failed",
            detail,
        )
        .with_code("AUTHENTICATION_FAILED")
    }

    /// `502 DownstreamError`.
    #[must_use]
    pub fn downstream(detail: impl Into<String>) -> Self {
        Self::new(
            StatusCode::BAD_GATEWAY,
            crate::domain::ids::ERR_DOWNSTREAM,
            "Downstream Error",
            detail,
        )
        .with_code("DOWNSTREAM_ERROR")
    }

    /// `504 Timeout`.
    #[must_use]
    pub fn timeout(detail: impl Into<String>) -> Self {
        Self::new(
            StatusCode::GATEWAY_TIMEOUT,
            crate::domain::ids::ERR_TIMEOUT,
            "Upstream Timeout",
            detail,
        )
        .with_code("UPSTREAM_TIMEOUT")
    }

    /// `503 LinkUnavailable`.
    #[must_use]
    pub fn link_unavailable(detail: impl Into<String>) -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            crate::domain::ids::ERR_LINK_UNAVAILABLE,
            "Link Unavailable",
            detail,
        )
        .with_code("LINK_UNAVAILABLE")
    }

    /// `504 ConnectionTimeout`: the upstream connector gave up establishing a
    /// connection. Distinguished from [`Self::downstream`] because a connect
    /// timeout is retryable while a refused connection is not.
    #[must_use]
    pub fn connect_timeout(detail: impl Into<String>) -> Self {
        Self::new(
            StatusCode::GATEWAY_TIMEOUT,
            crate::domain::ids::ERR_CONNECT_TIMEOUT,
            "Connection Timeout",
            detail,
        )
        .with_code("CONNECTION_TIMEOUT")
    }

    /// `403` `CORS` origin rejection.
    #[must_use]
    pub fn cors_origin_not_allowed(detail: impl Into<String>) -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            crate::domain::ids::ERR_CORS_ORIGIN,
            "Origin Not Allowed",
            detail,
        )
        .with_code("CORS_ORIGIN_NOT_ALLOWED")
    }

    /// `403` `CORS` method rejection.
    #[must_use]
    pub fn cors_method_not_allowed(detail: impl Into<String>) -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            crate::domain::ids::ERR_CORS_METHOD,
            "Method Not Allowed",
            detail,
        )
        .with_code("CORS_METHOD_NOT_ALLOWED")
    }

    /// `400` routing error.
    #[must_use]
    pub fn routing(error_type: &str, title: &str, detail: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, error_type, title, detail)
    }

    /// `503 PluginNotFound`.
    #[must_use]
    pub fn plugin_not_found(detail: impl Into<String>) -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            crate::domain::ids::ERR_PLUGIN_NOT_FOUND,
            "Plugin Not Found",
            detail,
        )
        .with_code("PLUGIN_NOT_FOUND")
    }

    /// `500` internal gateway failure.
    #[must_use]
    pub fn internal(detail: impl Into<String>) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            crate::domain::ids::ERR_INTERNAL,
            "Internal Error",
            detail,
        )
        .with_code("INTERNAL_ERROR")
    }

    /// Renders the problem document as `JSON`.
    ///
    /// The documented extension members (`referenced_by`, `valid_hosts`, ...)
    /// are carried in the problem's `context` internally and hoisted to the top
    /// level here, which is where `RFC 9457` puts extension members.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        let mut value = serde_json::to_value(&self.problem).unwrap_or_default();
        let Some(map) = value.as_object_mut() else {
            return value;
        };
        let extensions = map.remove("context").unwrap_or(serde_json::Value::Null);
        if let serde_json::Value::Object(members) = extensions {
            for (name, member) in members {
                map.insert(name, member);
            }
        }
        value
    }

    /// Renders the problem document as `JSON` bytes.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(&self.to_json()).unwrap_or_default()
    }
}

impl std::fmt::Display for OagwError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.problem.status, self.problem.title)
    }
}

impl std::error::Error for OagwError {}

impl IntoResponse for OagwError {
    fn into_response(self) -> Response {
        let mut response = (self.status, self.to_bytes()).into_response();
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static(PROBLEM_CONTENT_TYPE),
        );
        response.headers_mut().insert(
            ERROR_SOURCE_NAME.clone(),
            HeaderValue::from_static(SOURCE_GATEWAY),
        );
        if let Some(retry_after) = &self.retry_after
            && let Ok(value) = HeaderValue::from_str(retry_after)
        {
            response.headers_mut().insert(&RETRY_AFTER_NAME, value);
        }
        if let Some(headers) = &self.rate_limit {
            for (name, value) in [
                ("x-ratelimit-limit", headers.limit.to_string()),
                ("x-ratelimit-remaining", headers.remaining.to_string()),
                ("x-ratelimit-reset", headers.reset.to_string()),
            ] {
                if let Ok(value) = HeaderValue::from_str(&value) {
                    response.headers_mut().insert(HeaderName::from_static(name), value);
                }
            }
        }
        response
    }
}
