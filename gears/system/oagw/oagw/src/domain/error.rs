//! OAGW error type — RFC 9457 Problem Details with GTS type
//! identifiers and the `X-OAGW-Error-Source: gateway` header.
//!
//! Every gateway-originated error (control plane *and* data plane)
//! renders through [`OagwError`]; the `type` field carries the GTS
//! error Instance Identifier from the DESIGN error table
//! (`gts.cf.core.errors.err.v1~cf.oagw.*.v1`). Upstream-originated
//! errors are NOT wrapped in a problem document — the body is passed
//! through untouched with `X-OAGW-Error-Source: upstream` (see
//! `docs/ADR/0007-error-source-distinction.md`).

use axum::http::header;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value};

/// `X-OAGW-Error-Source` header, present on every proxied response.
pub const ERROR_SOURCE_HEADER: &str = "x-oagw-error-source";
/// `X-OAGW-Error-Source` value for errors raised by the gateway itself.
pub const ERROR_SOURCE_GATEWAY: &str = "gateway";
/// `X-OAGW-Error-Source` value for responses received from upstream
/// (including upstream error responses, passed through as-is).
pub const ERROR_SOURCE_UPSTREAM: &str = "upstream";

/// A single gateway error. `problem_type` is the GTS error Instance
/// Identifier (no `gts://` prefix), matching the DESIGN error table.
#[derive(Debug, Clone)]
pub struct OagwError {
    /// GTS error identifier (`gts.cf.core.errors.err.v1~cf.oagw.*.v1`).
    pub problem_type: &'static str,
    /// Human-readable, stable summary.
    pub title: &'static str,
    /// HTTP status code.
    pub status: StatusCode,
    /// Occurrence-specific explanation.
    pub detail: String,
    /// Retry guidance (`retry_after_seconds` extension field + HTTP
    /// `Retry-After` header). Only set for retriable errors.
    pub retry_after_seconds: Option<u64>,
    /// Optional RFC 9457 `instance` URI (request context).
    pub instance: Option<String>,
    /// Optional OAGW extension fields (`upstream_id`, `host`, `path`,
    /// …). Serialized flat on the problem document.
    pub context: Map<String, Value>,
}

impl OagwError {
    fn new(problem_type: &'static str, title: &'static str, status: StatusCode) -> Self {
        Self {
            problem_type,
            title,
            status,
            detail: title.to_owned(),
            retry_after_seconds: None,
            instance: None,
            context: Map::new(),
        }
    }

    /// Builder: set the occurrence detail.
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }

    /// Builder: attach an RFC 9457 `instance` URI.
    pub fn with_instance(mut self, instance: impl Into<String>) -> Self {
        self.instance = Some(instance.into());
        self
    }

    /// Builder: attach an OAGW extension field.
    pub fn with_ctx(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.context.insert(key.into(), value.into());
        self
    }

    /// 400 — general route / request validation failure.
    pub fn validation(detail: impl Into<String>) -> Self {
        Self::new(
            crate::gts::ERR_VALIDATION,
            "Route validation failed",
            StatusCode::BAD_REQUEST,
        )
        .with_detail(detail)
    }

    /// 400 — `X-OAGW-Target-Host` required for a common-suffix pool.
    pub fn missing_target_host(detail: impl Into<String>) -> Self {
        Self::new(
            crate::gts::ERR_MISSING_TARGET_HOST,
            "Target host missing",
            StatusCode::BAD_REQUEST,
        )
        .with_detail(detail)
    }

    /// 403 — token lacks the scope required for the operation.
    pub fn permission_denied(detail: impl Into<String>) -> Self {
        Self::new(
            crate::gts::ERR_PERMISSION_DENIED,
            "Permission denied",
            StatusCode::FORBIDDEN,
        )
        .with_detail(detail)
    }

    /// 404 — management resource not found in the calling tenant.
    pub fn not_found(detail: impl Into<String>) -> Self {
        Self::new(
            crate::gts::ERR_ROUTE_NOT_FOUND,
            "Not found",
            StatusCode::NOT_FOUND,
        )
        .with_detail(detail)
    }

    /// 409 — resource already exists.
    pub fn already_exists(detail: impl Into<String>) -> Self {
        Self::new(
            crate::gts::ERR_ALREADY_EXISTS,
            "Already exists",
            StatusCode::CONFLICT,
        )
        .with_detail(detail)
    }

    /// Re-label a validation failure as a 409 (e.g. duplicate route
    /// match), keeping the RFC 9457 envelope.
    pub fn as_conflict(self) -> Self {
        OagwError {
            status: StatusCode::CONFLICT,
            ..self
        }
    }

    /// 400 — `X-OAGW-Target-Host` format invalid.
    pub fn invalid_target_host(detail: impl Into<String>) -> Self {
        Self::new(
            crate::gts::ERR_INVALID_TARGET_HOST,
            "Target host invalid",
            StatusCode::BAD_REQUEST,
        )
        .with_detail(detail)
    }

    /// 400 — `X-OAGW-Target-Host` does not match any configured endpoint.
    pub fn unknown_target_host(detail: impl Into<String>) -> Self {
        Self::new(
            crate::gts::ERR_UNKNOWN_TARGET_HOST,
            "Target host unknown",
            StatusCode::BAD_REQUEST,
        )
        .with_detail(detail)
    }

    /// 401 — upstream authentication failed.
    pub fn auth_failed(detail: impl Into<String>) -> Self {
        Self::new(
            crate::gts::ERR_AUTH_FAILED,
            "Authentication failed",
            StatusCode::UNAUTHORIZED,
        )
        .with_detail(detail)
    }

    /// 403 — cross-origin origin rejected.
    pub fn cors_origin_not_allowed(detail: impl Into<String>) -> Self {
        Self::new(
            crate::gts::ERR_CORS_ORIGIN_NOT_ALLOWED,
            "Origin not allowed",
            StatusCode::FORBIDDEN,
        )
        .with_detail(detail)
    }

    /// 403 — cross-origin method rejected.
    pub fn cors_method_not_allowed(detail: impl Into<String>) -> Self {
        Self::new(
            crate::gts::ERR_CORS_METHOD_NOT_ALLOWED,
            "Method not allowed",
            StatusCode::FORBIDDEN,
        )
        .with_detail(detail)
    }

    /// 404 — no upstream matches the alias, or no route matches.
    pub fn route_not_found(detail: impl Into<String>) -> Self {
        Self::new(
            crate::gts::ERR_ROUTE_NOT_FOUND,
            "Route not found",
            StatusCode::NOT_FOUND,
        )
        .with_detail(detail)
    }

    /// 409 — plugin is still referenced by an upstream or route.
    pub fn plugin_in_use(detail: impl Into<String>) -> Self {
        Self::new(
            crate::gts::ERR_PLUGIN_IN_USE,
            "Plugin in use",
            StatusCode::CONFLICT,
        )
        .with_detail(detail)
    }

    /// 413 — request payload exceeds the limit.
    pub fn payload_too_large(detail: impl Into<String>) -> Self {
        Self::new(
            crate::gts::ERR_PAYLOAD_TOO_LARGE,
            "Payload too large",
            StatusCode::PAYLOAD_TOO_LARGE,
        )
        .with_detail(detail)
    }

    /// 429 — rate limit exceeded.
    pub fn rate_limit_exceeded(detail: impl Into<String>, retry_after_seconds: u64) -> Self {
        Self::new(
            crate::gts::ERR_RATE_LIMIT_EXCEEDED,
            "Rate limit exceeded",
            StatusCode::TOO_MANY_REQUESTS,
        )
        .with_detail(detail)
        .with_ctx("retry_after_seconds", retry_after_seconds)
        .with_retry_after(retry_after_seconds)
    }

    /// 500 — referenced secret not found in the credential store.
    pub fn secret_not_found(detail: impl Into<String>) -> Self {
        Self::new(
            crate::gts::ERR_SECRET_NOT_FOUND,
            "Secret not found",
            StatusCode::INTERNAL_SERVER_ERROR,
        )
        .with_detail(detail)
    }

    /// 502 — protocol-level failure talking to the upstream.
    pub fn protocol_error(detail: impl Into<String>) -> Self {
        Self::new(
            crate::gts::ERR_PROTOCOL_ERROR,
            "Protocol error",
            StatusCode::BAD_GATEWAY,
        )
        .with_detail(detail)
    }

    /// 502 — stream aborted mid-flight.
    pub fn stream_aborted(detail: impl Into<String>) -> Self {
        Self::new(
            crate::gts::ERR_STREAM_ABORTED,
            "Stream aborted",
            StatusCode::BAD_GATEWAY,
        )
        .with_detail(detail)
    }

    /// 503 — upstream disabled or unreachable.
    pub fn link_unavailable(detail: impl Into<String>) -> Self {
        Self::new(
            crate::gts::ERR_LINK_UNAVAILABLE,
            "Upstream link unavailable",
            StatusCode::SERVICE_UNAVAILABLE,
        )
        .with_detail(detail)
    }

    /// 503 — referenced plugin cannot be resolved.
    pub fn plugin_not_found(detail: impl Into<String>) -> Self {
        Self::new(
            crate::gts::ERR_PLUGIN_NOT_FOUND,
            "Plugin not found",
            StatusCode::SERVICE_UNAVAILABLE,
        )
        .with_detail(detail)
    }

    /// 504 — connection-establishment timeout.
    pub fn timeout_connection(detail: impl Into<String>) -> Self {
        Self::new(
            crate::gts::ERR_TIMEOUT_CONNECTION,
            "Connection timeout",
            StatusCode::GATEWAY_TIMEOUT,
        )
        .with_detail(detail)
    }

    /// 504 — overall request timeout.
    pub fn timeout_request(detail: impl Into<String>) -> Self {
        Self::new(
            crate::gts::ERR_TIMEOUT_REQUEST,
            "Request timeout",
            StatusCode::GATEWAY_TIMEOUT,
        )
        .with_detail(detail)
    }

    /// 504 — idle timeout on a proxied connection.
    pub fn timeout_idle(detail: impl Into<String>) -> Self {
        Self::new(
            crate::gts::ERR_TIMEOUT_IDLE,
            "Idle timeout",
            StatusCode::GATEWAY_TIMEOUT,
        )
        .with_detail(detail)
    }

    fn with_retry_after(mut self, seconds: u64) -> Self {
        self.retry_after_seconds = Some(seconds);
        self
    }
}

impl IntoResponse for OagwError {
    fn into_response(self) -> Response {
        let mut body = Map::new();
        body.insert("type".to_owned(), Value::String(self.problem_type.to_owned()));
        body.insert("title".to_owned(), Value::String(self.title.to_owned()));
        body.insert(
            "status".to_owned(),
            Value::Number(self.status.as_u16().into()),
        );
        body.insert("detail".to_owned(), Value::String(self.detail));
        if let Some(instance) = self.instance {
            body.insert("instance".to_owned(), Value::String(instance));
        }
        if let Some(retry) = self.retry_after_seconds {
            body.insert(
                "retry_after_seconds".to_owned(),
                Value::Number(retry.into()),
            );
        }
        for (key, value) in self.context {
            body.entry(key).or_insert(value);
        }

        let bytes = serde_json::to_vec(&Value::Object(body)).unwrap_or_default();
        let mut response = axum::response::Response::new(bytes.into());
        *response.status_mut() = self.status;
        let headers = response.headers_mut();
        headers.insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/problem+json; charset=utf-8"),
        );
        headers.insert(
            header::HeaderName::from_static(ERROR_SOURCE_HEADER),
            header::HeaderValue::from_static(ERROR_SOURCE_GATEWAY),
        );
        if let Some(retry) = self.retry_after_seconds {
            if let Ok(value) = retry.to_string().parse() {
                headers.insert(header::RETRY_AFTER, value);
            }
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_renders_rfc9457_problem_with_gts_type() {
        let err = OagwError::rate_limit_exceeded("too many", 13);
        assert_eq!(err.problem_type, crate::gts::ERR_RATE_LIMIT_EXCEEDED);
        assert_eq!(err.status, StatusCode::TOO_MANY_REQUESTS);
    }
}
