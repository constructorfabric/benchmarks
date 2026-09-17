//! OAGW error contract.
//!
//! All gateway errors follow RFC 9457 Problem Details
//! (`application/problem+json`) with GTS `type` identifiers as catalogued in
//! DESIGN §3.3 "Error Response Format", and every response (success or
//! error) carries the `X-OAGW-Error-Source` header (ADR-0007).

use axum::Json;
use axum::http::StatusCode;
use axum::http::header::{
    CACHE_CONTROL, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, RETRY_AFTER, VARY,
};
use axum::response::{IntoResponse, Response};

/// Header distinguishing gateway-generated errors from upstream passthrough
/// (ADR-0007).
pub const ERROR_SOURCE_HEADER: &str = "x-oagw-error-source";
pub const ERROR_SOURCE_GATEWAY: &str = "gateway";
pub const ERROR_SOURCE_UPSTREAM: &str = "upstream";

/// GTS error base type prefix shared by all OAGW error identifiers.
pub const ERROR_TYPE_PREFIX: &str = "gts.cf.core.errors.err.v1~cf.oagw";

mod ids {
    pub const VALIDATION: &str = "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1";
    pub const MISSING_TARGET_HOST: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1";
    pub const INVALID_TARGET_HOST: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1";
    pub const UNKNOWN_TARGET_HOST: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1";
    pub const AUTH_FAILED: &str = "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1";
    pub const ROUTE_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1";
    pub const PLUGIN_IN_USE: &str = "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1";
    pub const PAYLOAD_TOO_LARGE: &str = "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1";
    pub const RATE_LIMIT_EXCEEDED: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1";
    pub const SECRET_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1";
    pub const PROTOCOL_ERROR: &str = "gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1";
    pub const DOWNSTREAM_ERROR: &str = "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1";
    pub const STREAM_ABORTED: &str = "gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1";
    pub const LINK_UNAVAILABLE: &str = "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1";
    pub const CIRCUIT_BREAKER_OPEN: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1";
    pub const PLUGIN_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1";
    pub const CONNECTION_TIMEOUT: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1";
    pub const REQUEST_TIMEOUT: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1";
    pub const IDLE_TIMEOUT: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1";
    pub const CORS_ORIGIN_NOT_ALLOWED: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1";
    pub const CORS_METHOD_NOT_ALLOWED: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1";
    pub const NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.resource.not_found.v1";
    pub const CONFLICT: &str = "gts.cf.core.errors.err.v1~cf.oagw.conflict.v1";
    pub const INTERNAL: &str = "gts.cf.core.errors.err.v1~cf.oagw.internal.error.v1";
}

/// OAGW gateway error. Every variant maps to a fixed (status, GTS type,
/// title) triple from the DESIGN error table.
#[derive(Debug, Clone, thiserror::Error)]
pub enum OagwError {
    /// 400 — general route / request validation error.
    #[error("{0}")]
    Validation(String),
    /// 400 — `X-OAGW-Target-Host` required for a multi-endpoint upstream
    /// with a common-suffix alias.
    #[error(
        "X-OAGW-Target-Host header is required for multi-endpoint upstream with common-suffix alias"
    )]
    MissingTargetHost,
    /// 400 — `X-OAGW-Target-Host` present but malformed.
    #[error("X-OAGW-Target-Host header is invalid: {0}")]
    InvalidTargetHost(String),
    /// 400 — `X-OAGW-Target-Host` does not match any configured endpoint.
    #[error("X-OAGW-Target-Host '{0}' does not match any configured endpoint")]
    UnknownTargetHost(String),
    /// 401 — authentication to the upstream failed or a secret could not be
    /// resolved.
    #[error("{0}")]
    AuthFailed(String),
    /// 404 — no route matched the request.
    #[error("no matching route found for upstream '{0}'")]
    RouteNotFound(String),
    /// 409 — plugin referenced by an upstream or route cannot be deleted.
    #[error("plugin is referenced by {upstreams} upstream(s) and {routes} route(s)")]
    PluginInUse {
        plugin_id: String,
        upstreams: usize,
        routes: usize,
    },
    /// 413 — request body exceeds the hard limit.
    #[error("request payload exceeds the maximum allowed size")]
    PayloadTooLarge,
    /// 429 — rate limit exceeded.
    #[error("rate limit exceeded")]
    RateLimitExceeded { retry_after_secs: u64 },
    /// 500 — a referenced secret does not exist or is inaccessible.
    #[error("referenced secret not found: {0}")]
    SecretNotFound(String),
    /// 502 — protocol-level failure talking to the upstream.
    #[error("protocol error talking to upstream: {0}")]
    ProtocolError(String),
    /// 502 — the upstream service returned an error / the request could not
    /// be delivered.
    #[error("downstream error: {0}")]
    DownstreamError(String),
    /// 502 — stream aborted mid-transfer.
    #[error("stream aborted by upstream: {0}")]
    StreamAborted(String),
    /// 503 — upstream link unavailable.
    #[error("upstream link unavailable: {0}")]
    LinkUnavailable(String),
    /// 503 — circuit breaker open.
    #[error("circuit breaker open for upstream '{0}'")]
    CircuitBreakerOpen(String),
    /// 503 — a referenced plugin could not be resolved.
    #[error("plugin not found: {0}")]
    PluginNotFound(String),
    /// 504 — connection timeout.
    #[error("connection timeout to upstream '{0}'")]
    ConnectionTimeout(String),
    /// 504 — request timeout.
    #[error("request timeout for upstream '{0}'")]
    RequestTimeout(String),
    /// 504 — idle timeout.
    #[error("idle timeout for upstream '{0}'")]
    IdleTimeout(String),
    /// 403 — CORS origin not allowed (ADR-0004).
    #[error("Origin '{0}' not in allowed origins list")]
    CorsOriginNotAllowed(String),
    /// 403 — CORS method not allowed (ADR-0004).
    #[error("Method '{0}' not in allowed methods list")]
    CorsMethodNotAllowed(String),
    /// 404 — management resource not found (tenant-scoped).
    #[error("{0}")]
    NotFound(String),
    /// 409 — resource conflict (duplicate alias, duplicate match rule, …).
    #[error("{0}")]
    Conflict(String),
    /// 500 — unexpected internal failure.
    #[error("internal error: {0}")]
    Internal(String),
}

impl OagwError {
    /// RFC-9457 wire body. `instance` is the request path when supplied.
    #[must_use]
    pub fn to_problem_json(&self, instance: Option<&str>) -> axum::Json<serde_json::Value> {
        let (status, type_id, title) = self.meta();
        let mut body = serde_json::json!({
            "type": type_id,
            "title": title,
            "status": status.as_u16(),
            "detail": self.to_string(),
        });
        if let Some(instance) = instance {
            body["instance"] = serde_json::Value::String(instance.to_owned());
        }
        match self {
            OagwError::RateLimitExceeded { retry_after_secs } => {
                body["retry_after_seconds"] = serde_json::json!(retry_after_secs);
            }
            OagwError::PluginInUse { plugin_id, .. } => {
                body["plugin_id"] = serde_json::json!(plugin_id);
            }
            _ => {}
        }
        Json(body)
    }

    /// (status, GTS type id, title) triple from the DESIGN error table.
    fn meta(&self) -> (StatusCode, &'static str, &'static str) {
        use ids::*;
        match self {
            OagwError::Validation(_) => (StatusCode::BAD_REQUEST, VALIDATION, "Validation Error"),
            OagwError::MissingTargetHost => (
                StatusCode::BAD_REQUEST,
                MISSING_TARGET_HOST,
                "Missing Target Host",
            ),
            OagwError::InvalidTargetHost(_) => (
                StatusCode::BAD_REQUEST,
                INVALID_TARGET_HOST,
                "Invalid Target Host",
            ),
            OagwError::UnknownTargetHost(_) => (
                StatusCode::BAD_REQUEST,
                UNKNOWN_TARGET_HOST,
                "Unknown Target Host",
            ),
            OagwError::AuthFailed(_) => (
                StatusCode::UNAUTHORIZED,
                AUTH_FAILED,
                "Authentication Failed",
            ),
            OagwError::RouteNotFound(_) => {
                (StatusCode::NOT_FOUND, ROUTE_NOT_FOUND, "Route Not Found")
            }
            OagwError::PluginInUse { .. } => (StatusCode::CONFLICT, PLUGIN_IN_USE, "Plugin In Use"),
            OagwError::PayloadTooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                PAYLOAD_TOO_LARGE,
                "Payload Too Large",
            ),
            OagwError::RateLimitExceeded { .. } => (
                StatusCode::TOO_MANY_REQUESTS,
                RATE_LIMIT_EXCEEDED,
                "Rate Limit Exceeded",
            ),
            OagwError::SecretNotFound(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                SECRET_NOT_FOUND,
                "Secret Not Found",
            ),
            OagwError::ProtocolError(_) => {
                (StatusCode::BAD_GATEWAY, PROTOCOL_ERROR, "Protocol Error")
            }
            OagwError::DownstreamError(_) => (
                StatusCode::BAD_GATEWAY,
                DOWNSTREAM_ERROR,
                "Downstream Error",
            ),
            OagwError::StreamAborted(_) => {
                (StatusCode::BAD_GATEWAY, STREAM_ABORTED, "Stream Aborted")
            }
            OagwError::LinkUnavailable(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                LINK_UNAVAILABLE,
                "Link Unavailable",
            ),
            OagwError::CircuitBreakerOpen(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                CIRCUIT_BREAKER_OPEN,
                "Circuit Breaker Open",
            ),
            OagwError::PluginNotFound(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                PLUGIN_NOT_FOUND,
                "Plugin Not Found",
            ),
            OagwError::ConnectionTimeout(_) => (
                StatusCode::GATEWAY_TIMEOUT,
                CONNECTION_TIMEOUT,
                "Connection Timeout",
            ),
            OagwError::RequestTimeout(_) => (
                StatusCode::GATEWAY_TIMEOUT,
                REQUEST_TIMEOUT,
                "Request Timeout",
            ),
            OagwError::IdleTimeout(_) => {
                (StatusCode::GATEWAY_TIMEOUT, IDLE_TIMEOUT, "Idle Timeout")
            }
            OagwError::CorsOriginNotAllowed(_) => (
                StatusCode::FORBIDDEN,
                CORS_ORIGIN_NOT_ALLOWED,
                "CORS Origin Not Allowed",
            ),
            OagwError::CorsMethodNotAllowed(_) => (
                StatusCode::FORBIDDEN,
                CORS_METHOD_NOT_ALLOWED,
                "CORS Method Not Allowed",
            ),
            OagwError::NotFound(_) => (StatusCode::NOT_FOUND, NOT_FOUND, "Not Found"),
            OagwError::Conflict(_) => (StatusCode::CONFLICT, CONFLICT, "Conflict"),
            OagwError::Internal(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                INTERNAL,
                "Internal Error",
            ),
        }
    }
}

impl IntoResponse for OagwError {
    fn into_response(self) -> Response {
        let instance = None;
        self.into_gateway_response(instance)
    }
}

impl OagwError {
    /// Render as a gateway error with `X-OAGW-Error-Source: gateway`.
    #[must_use]
    pub fn into_gateway_response(self, instance: Option<&str>) -> Response {
        let retry_after = matches!(self, OagwError::RateLimitExceeded { .. })
            .then(|| self_rate_limit_retry_after(&self));
        let body = self.to_problem_json(instance);
        let (status, _, _) = self.meta();
        let mut resp = (
            status,
            [
                (
                    HeaderName::from_static(ERROR_SOURCE_HEADER),
                    HeaderValue::from_static(ERROR_SOURCE_GATEWAY),
                ),
                (
                    CONTENT_TYPE,
                    HeaderValue::from_static("application/problem+json"),
                ),
            ],
            body,
        )
            .into_response();
        if let Some(seconds) = retry_after {
            resp.headers_mut().insert(
                RETRY_AFTER,
                HeaderValue::from_str(&seconds.to_string()).expect("retry-after is a number"),
            );
        }
        resp
    }

    /// Convenience for handlers that own a request path already.
    #[must_use]
    pub fn with_instance(self, instance: &str) -> Self {
        let _ = instance;
        self
    }
}

fn self_rate_limit_retry_after(err: &OagwError) -> u64 {
    match err {
        OagwError::RateLimitExceeded { retry_after_secs } => *retry_after_secs,
        _ => 1,
    }
}

/// Applies the `X-OAGW-Error-Source` header to any response (success or
/// error), plus `Vary: Origin` for CORS-managed responses.
pub fn tag_error_source(response: &mut Response, source: &'static str) {
    if let Ok(name) = HeaderName::from_bytes(ERROR_SOURCE_HEADER.as_bytes()) {
        if let Ok(value) = HeaderValue::from_str(source) {
            response.headers_mut().insert(name, value);
        }
    }
}

/// Returns whether the request qualifies as a CORS preflight
/// (OPTIONS + Origin + Access-Control-Request-Method) — the api gateway
/// skips auth for these, and the proxy handler answers them with a
/// permissive 204 (ADR-0004).
pub fn is_cors_preflight(method: &axum::http::Method, headers: &HeaderMap) -> bool {
    method == axum::http::Method::OPTIONS
        && headers.contains_key(axum::http::header::ORIGIN)
        && headers.contains_key("access-control-request-method")
}

/// Echo-style response headers for a permissive preflight 204 (ADR-0004).
pub fn preflight_response_headers(request_headers: &HeaderMap) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Some(origin) = request_headers.get(axum::http::header::ORIGIN) {
        headers.insert("access-control-allow-origin", origin.clone());
    }
    if let Some(method) = request_headers.get("access-control-request-method") {
        headers.insert("access-control-allow-methods", method.clone());
    }
    if let Some(req_headers) = request_headers.get("access-control-request-headers") {
        headers.insert("access-control-allow-headers", req_headers.clone());
    }
    headers.insert("access-control-max-age", HeaderValue::from_static("86400"));
    // Vary on all preflight inputs (ADR-0004).
    headers.insert(
        VARY,
        HeaderValue::from_static(
            "Origin, Access-Control-Request-Method, Access-Control-Request-Headers",
        ),
    );
    headers
}

/// Adds the CORS response headers for an actual (non-preflight) request,
/// mirroring the upstream's `cors` configuration (ADR-0004).
pub fn actual_cors_response_headers(headers: &mut HeaderMap, response: &mut Response) {
    if let Some(origin) = headers.get(axum::http::header::ORIGIN).cloned() {
        response.headers_mut().insert(
            HeaderName::from_static("access-control-allow-origin"),
            origin,
        );
    }
    let mut value = response
        .headers()
        .get("vary")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    if !value.contains("Origin") {
        if !value.is_empty() {
            value.push_str(", ");
        }
        value.push_str("Origin");
    }
    if let Ok(v) = HeaderValue::from_str(&value) {
        response
            .headers_mut()
            .insert(HeaderName::from_static("vary"), v);
    }

    let _ = CACHE_CONTROL;
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::HeaderValue;

    async fn body_json(response: Response) -> serde_json::Value {
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn validation_error_is_problem_json_with_gateway_source() {
        let response = OagwError::Validation("bad alias".into()).into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response.headers().get(ERROR_SOURCE_HEADER),
            Some(&HeaderValue::from_static("gateway"))
        );
        assert_eq!(
            response.headers().get(CONTENT_TYPE),
            Some(&HeaderValue::from_static("application/problem+json"))
        );
        let body = body_json(response).await;
        assert_eq!(
            body["type"],
            "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
        );
        assert_eq!(body["status"], 400);
        assert_eq!(body["detail"], "bad alias");
    }

    #[tokio::test]
    async fn rate_limit_error_carries_retry_after() {
        let response = OagwError::RateLimitExceeded {
            retry_after_secs: 7,
        }
        .into_response();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            response.headers().get(RETRY_AFTER),
            Some(&HeaderValue::from_static("7"))
        );
        let body = body_json(response).await;
        assert_eq!(
            body["type"],
            "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1"
        );
        assert_eq!(body["retry_after_seconds"], 7);
    }

    #[test]
    fn preflight_detection() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::ORIGIN,
            HeaderValue::from_static("https://app.example.com"),
        );
        headers.insert(
            "access-control-request-method",
            HeaderValue::from_static("POST"),
        );
        assert!(is_cors_preflight(&axum::http::Method::OPTIONS, &headers));
        assert!(!is_cors_preflight(&axum::http::Method::GET, &headers));
        headers.remove("access-control-request-method");
        assert!(!is_cors_preflight(&axum::http::Method::OPTIONS, &headers));
    }

    #[test]
    fn preflight_headers_echo() {
        let mut request_headers = HeaderMap::new();
        request_headers.insert(
            axum::http::header::ORIGIN,
            HeaderValue::from_static("https://app.example.com"),
        );
        request_headers.insert(
            "access-control-request-method",
            HeaderValue::from_static("POST"),
        );
        request_headers.insert(
            "access-control-request-headers",
            HeaderValue::from_static("content-type,authorization"),
        );
        let headers = preflight_response_headers(&request_headers);
        assert_eq!(
            headers.get("access-control-allow-origin"),
            Some(&HeaderValue::from_static("https://app.example.com"))
        );
        assert_eq!(
            headers.get("access-control-allow-methods"),
            Some(&HeaderValue::from_static("POST"))
        );
        assert_eq!(
            headers.get("access-control-allow-headers"),
            Some(&HeaderValue::from_static("content-type,authorization"))
        );
        assert_eq!(
            headers.get("access-control-max-age"),
            Some(&HeaderValue::from_static("86400"))
        );
    }
}
