// Created: 2026-09-02 by Constructor Tech
//! Gateway error type and its RFC 9457 wire mapping.
//!
//! Every gateway-originated failure is a [`GatewayError`]. It renders as
//! `application/problem+json` with the GTS `type` identifier from
//! `DESIGN.md` §3.3 and carries `X-OAGW-Error-Source: gateway` (ADR-0007), so a
//! client can always tell a gateway fault from a passthrough upstream error.
//!
//! Upstream errors are **not** represented here: they are forwarded verbatim and
//! only annotated with `X-OAGW-Error-Source: upstream`.
//!
//! OAGW-specific extension fields (`upstream_id`, `alias`, `host`, `path`,
//! `valid_hosts`, `invalid_value`, `plugin_id`, `referenced_by`,
//! `retry_after_seconds`, …) travel in the problem's `context` member. That is
//! the extension point the platform's `Problem` type provides: the gateway's
//! canonical-error middleware re-serialises every `problem+json` body through
//! `Problem`, so top-level members outside the RFC 9457 set would be dropped.

use axum::body::Body;
use axum::http::HeaderName;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use toolkit_canonical_errors::problem::APPLICATION_PROBLEM_JSON;

use crate::gts::{self, errors as err};

/// Machine-readable error codes surfaced in the problem's `error_code` member.
pub mod codes {
    /// Upstream rejected the credentials.
    pub const AUTHENTICATION_FAILED: &str = "AUTHENTICATION_FAILED";
    /// A guard plugin rejected the request.
    pub const GUARD_REJECTED: &str = "GUARD_REJECTED";
    /// ADR-0009: a required header was missing.
    pub const REQUIRED_HEADER_MISSING: &str = "REQUIRED_HEADER_MISSING";
    /// Rate limiter rejected the request.
    pub const RATE_LIMIT_EXCEEDED: &str = "RATE_LIMIT_EXCEEDED";
    /// SSRF policy rejected the upstream target.
    pub const SSRF_BLOCKED: &str = "SSRF_BLOCKED";
    /// Plaintext upstream dialled while `allow_http_upstream` is off.
    pub const INSECURE_UPSTREAM: &str = "INSECURE_UPSTREAM";
}

/// A gateway-originated error, ready to render as `problem+json`.
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum GatewayError {
    /// 400 — request validation failed (body, headers, query, path suffix).
    #[error("validation failed: {0}")]
    Validation(String),
    /// 400 — `X-OAGW-Target-Host` required but absent (common-suffix pool).
    #[error("missing X-OAGW-Target-Host header")]
    MissingTargetHost {
        upstream_id: String,
        alias: String,
        valid_hosts: Vec<String>,
    },
    /// 400 — `X-OAGW-Target-Host` is not a bare hostname or IP.
    #[error("invalid X-OAGW-Target-Host value")]
    InvalidTargetHost {
        upstream_id: String,
        invalid_value: String,
    },
    /// 400 — `X-OAGW-Target-Host` names no configured endpoint.
    #[error("unknown X-OAGW-Target-Host value")]
    UnknownTargetHost {
        upstream_id: String,
        invalid_value: String,
        valid_hosts: Vec<String>,
    },
    /// 400 — the dialled plaintext upstream while `allow_http_upstream` is off.
    #[error("plaintext upstream connections are not permitted: {0}")]
    InsecureUpstream(String),
    /// 401 — upstream authentication failed (credentials rejected or unresolvable).
    #[error("authentication to upstream failed: {0}")]
    AuthenticationFailed(String),
    /// 403 — SSRF policy rejected the upstream target.
    #[error("upstream target blocked by SSRF policy: {0}")]
    SsrfBlocked(String),
    /// 403 — CORS preflight/actual-request rejection.
    #[error("{kind}: {detail}")]
    CorsRejected { kind: &'static str, detail: String },
    /// 404 — management resource missing or invisible to the caller.
    #[error("{0}")]
    NotFound(String),
    /// 404 — no route matched the request.
    #[error("no matching route: {0}")]
    RouteNotFound(String),
    /// 409 — immutable field changed, or a uniqueness constraint was violated.
    #[error("conflict: {0}")]
    Conflict(String),
    /// 409 — plugin still referenced.
    #[error("plugin in use")]
    PluginInUse {
        plugin_id: String,
        upstreams: Vec<String>,
        routes: Vec<String>,
    },
    /// 413 — request body above the hard limit.
    #[error("request payload too large")]
    PayloadTooLarge,
    /// 429 — rate limit exceeded.
    #[error("rate limit exceeded: {detail}")]
    RateLimited {
        detail: String,
        limit: u64,
        remaining: u64,
        reset_secs: u64,
        retry_after_secs: u64,
    },
    /// 500 — referenced secret does not exist in the credential store.
    #[error("secret not found: {0}")]
    SecretNotFound(String),
    /// 502 — protocol-level failure talking to the upstream.
    #[error("protocol error: {0}")]
    ProtocolError(String),
    /// 502 — the upstream returned a response the gateway cannot relay.
    #[error("downstream error: {0}")]
    DownstreamError(String),
    /// 502 — a proxied stream aborted mid-flight.
    #[error("stream aborted: {0}")]
    StreamAborted(String),
    /// 503 — upstream unreachable, or disabled.
    #[error("upstream link unavailable: {0}")]
    LinkUnavailable(String),
    /// 503 — circuit breaker open.
    #[error("circuit breaker open: {0}")]
    CircuitBreakerOpen(String),
    /// 503 — a bound plugin cannot be resolved.
    #[error("plugin not found: {0}")]
    PluginNotFound(String),
    /// 504 — timeout, classified by the phase that elapsed.
    #[error("upstream timeout: {detail}")]
    Timeout { kind: TimeoutKind, detail: String },
    /// 500 — unexpected internal failure.
    #[error("internal error")]
    Internal(String),
}

/// Which phase of the upstream call timed out (`DESIGN.md` §3.3 timeout rows).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutKind {
    /// Connect/TLS handshake did not complete.
    Connection,
    /// Response headers did not arrive in time.
    Request,
    /// A streaming response went idle.
    Idle,
}

impl TimeoutKind {
    /// GTS error instance id for this timeout class.
    #[must_use]
    pub fn gts_type(self) -> &'static str {
        match self {
            Self::Connection => err::TIMEOUT_CONNECTION,
            Self::Request => err::TIMEOUT_REQUEST,
            Self::Idle => err::TIMEOUT_IDLE,
        }
    }

    /// Human-facing title for this timeout class.
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::Connection => "Connection Timeout",
            Self::Request => "Request Timeout",
            Self::Idle => "Idle Timeout",
        }
    }
}

impl GatewayError {
    /// HTTP status for this error.
    #[must_use]
    pub fn status(&self) -> axum::http::StatusCode {
        use axum::http::StatusCode as S;
        match self {
            Self::Validation(_)
            | Self::MissingTargetHost { .. }
            | Self::InvalidTargetHost { .. }
            | Self::UnknownTargetHost { .. } => S::BAD_REQUEST,
            Self::InsecureUpstream(_) => S::BAD_REQUEST,
            Self::AuthenticationFailed(_) => S::UNAUTHORIZED,
            Self::SsrfBlocked(_) => S::FORBIDDEN,
            Self::CorsRejected { .. } => S::FORBIDDEN,
            Self::NotFound(_) | Self::RouteNotFound(_) => S::NOT_FOUND,
            Self::Conflict(_) | Self::PluginInUse { .. } => S::CONFLICT,
            Self::PayloadTooLarge => S::PAYLOAD_TOO_LARGE,
            Self::RateLimited { .. } => S::TOO_MANY_REQUESTS,
            Self::SecretNotFound(_) => S::INTERNAL_SERVER_ERROR,
            Self::ProtocolError(_)
            | Self::DownstreamError(_)
            | Self::StreamAborted(_) => S::BAD_GATEWAY,
            Self::LinkUnavailable(_)
            | Self::CircuitBreakerOpen(_)
            | Self::PluginNotFound(_) => S::SERVICE_UNAVAILABLE,
            Self::Timeout { .. } => S::GATEWAY_TIMEOUT,
            Self::Internal(_) => S::INTERNAL_SERVER_ERROR,
        }
    }

    /// GTS error instance id for this error.
    #[must_use]
    pub fn gts_type(&self) -> &'static str {
        match self {
            Self::InvalidTargetHost { .. } => err::ROUTING_INVALID_TARGET_HOST,
            Self::UnknownTargetHost { .. } => err::ROUTING_UNKNOWN_TARGET_HOST,
            Self::Validation(_) => err::VALIDATION,
            Self::MissingTargetHost { .. } => err::ROUTING_MISSING_TARGET_HOST,
            Self::InsecureUpstream(_) => err::INSECURE_UPSTREAM,
            Self::AuthenticationFailed(_) => err::AUTH_FAILED,
            Self::SsrfBlocked(_) => err::SSRF_BLOCKED,
            Self::CorsRejected { kind, .. } => match *kind {
                "origin" => gts::CORS_ORIGIN_NOT_ALLOWED,
                _ => gts::CORS_METHOD_NOT_ALLOWED,
            },
            Self::NotFound(_) => err::RESOURCE_NOT_FOUND,
            Self::RouteNotFound(_) => err::ROUTE_NOT_FOUND,
            Self::Conflict(_) => err::CONFLICT,
            Self::PluginInUse { .. } => err::PLUGIN_IN_USE,
            Self::PayloadTooLarge => err::PAYLOAD_TOO_LARGE,
            Self::RateLimited { .. } => err::RATE_LIMIT_EXCEEDED,
            Self::SecretNotFound(_) => err::SECRET_NOT_FOUND,
            Self::ProtocolError(_) => err::PROTOCOL_ERROR,
            Self::DownstreamError(_) => err::DOWNSTREAM_ERROR,
            Self::StreamAborted(_) => err::STREAM_ABORTED,
            Self::LinkUnavailable(_) => err::LINK_UNAVAILABLE,
            Self::CircuitBreakerOpen(_) => err::CIRCUIT_BREAKER_OPEN,
            Self::PluginNotFound(_) => err::PLUGIN_NOT_FOUND,
            Self::Timeout { kind, .. } => kind.gts_type(),
            Self::Internal(_) => err::VALIDATION,
        }
    }

    /// Human-facing title for this error.
    #[must_use]
    pub fn title(&self) -> &'static str {
        match self {
            Self::Validation(_) => "Validation Error",
            Self::MissingTargetHost { .. } => "Missing Target Host Header",
            Self::InvalidTargetHost { .. } => "Invalid Target Host Format",
            Self::UnknownTargetHost { .. } => "Unknown Target Host",
            Self::InsecureUpstream(_) => "Insecure Upstream",
            Self::AuthenticationFailed(_) => "Authentication Failed",
            Self::SsrfBlocked(_) => "Upstream Target Blocked",
            Self::CorsRejected { kind, .. } => match *kind {
                "origin" => "Origin Not Allowed",
                _ => "Method Not Allowed",
            },
            Self::NotFound(_) => "Not Found",
            Self::RouteNotFound(_) => "Route Not Found",
            Self::Conflict(_) => "Conflict",
            Self::PluginInUse { .. } => "Plugin In Use",
            Self::PayloadTooLarge => "Payload Too Large",
            Self::RateLimited { .. } => "Rate Limit Exceeded",
            Self::SecretNotFound(_) => "Secret Not Found",
            Self::ProtocolError(_) => "Protocol Error",
            Self::DownstreamError(_) => "Downstream Error",
            Self::StreamAborted(_) => "Stream Aborted",
            Self::LinkUnavailable(_) => "Upstream Link Unavailable",
            Self::CircuitBreakerOpen(_) => "Circuit Breaker Open",
            Self::PluginNotFound(_) => "Plugin Not Found",
            Self::Timeout { kind, .. } => kind.title(),
            Self::Internal(_) => "Internal Error",
        }
    }

    /// Machine-readable code for the problem's `error_code` member.
    #[must_use]
    pub fn error_code(&self) -> Option<&'static str> {
        match self {
            Self::AuthenticationFailed(_) => Some(codes::AUTHENTICATION_FAILED),
            Self::RateLimited { .. } => Some(codes::RATE_LIMIT_EXCEEDED),
            Self::Validation(_) | Self::CorsRejected { .. } => Some(codes::GUARD_REJECTED),
            Self::SsrfBlocked(_) => Some(codes::SSRF_BLOCKED),
            Self::InsecureUpstream(_) => Some(codes::INSECURE_UPSTREAM),
            _ => None,
        }
    }

    /// OAGW extension fields carried in the problem's `context` member.
    #[must_use]
    pub fn context(&self) -> Value {
        match self {
            Self::Validation(_) => json!({}),
            Self::MissingTargetHost {
                upstream_id,
                alias,
                valid_hosts,
            } => json!({
                "upstream_id": upstream_id,
                "alias": alias,
                "valid_hosts": valid_hosts,
            }),
            Self::InvalidTargetHost {
                upstream_id,
                invalid_value,
            } => json!({ "upstream_id": upstream_id, "invalid_value": invalid_value }),
            Self::UnknownTargetHost {
                upstream_id,
                invalid_value,
                valid_hosts,
            } => json!({
                "upstream_id": upstream_id,
                "invalid_value": invalid_value,
                "valid_hosts": valid_hosts,
            }),
            Self::CorsRejected { kind, detail: _ } => json!({ "cors": kind }),
            Self::PluginInUse {
                plugin_id,
                upstreams,
                routes,
            } => json!({
                "plugin_id": plugin_id,
                "referenced_by": { "upstreams": upstreams, "routes": routes },
            }),
            Self::RateLimited {
                limit,
                remaining,
                reset_secs,
                retry_after_secs,
                ..
            } => json!({
                "limit": limit,
                "remaining": remaining,
                "reset_seconds": reset_secs,
                "retry_after_seconds": retry_after_secs,
            }),
            _ => json!({}),
        }
    }

    /// `Retry-After` value in seconds, when the client should back off.
    #[must_use]
    pub fn retry_after_secs(&self) -> Option<u64> {
        match self {
            Self::RateLimited {
                retry_after_secs, ..
            } => Some(*retry_after_secs),
            Self::LinkUnavailable(_) | Self::CircuitBreakerOpen(_) => Some(1),
            Self::Timeout { .. } => Some(1),
            _ => None,
        }
    }

    /// Render as an axum `Response` with `instance` set to the request path.
    #[must_use]
    pub fn render_problem(&self, instance: Option<&str>) -> Response {
        let status = self.status();
        // `Internal` diagnostics never reach the wire (platform redaction rule);
        // the client sees the category-level detail only.
        let detail = match self {
            Self::Internal(_) => "an internal error occurred while processing the request"
                .to_owned(),
            _ => self.to_string(),
        };
        let mut context = match self.context() {
            Value::Object(map) => map,
            other => {
                let mut map = serde_json::Map::new();
                map.insert("value".to_owned(), other);
                map
            }
        };
        if let Some(instance) = instance {
            context.insert("instance".to_owned(), Value::String(instance.to_owned()));
        }

        let body = json!({
            "type": self.gts_type(),
            "title": self.title(),
            "status": status.as_u16(),
            "detail": detail,
            "instance": instance,
            "trace_id": Value::Null,
            "context": context,
            "error_code": self.error_code(),
            "error_domain": "oagw",
        });

        // Drop the nulls the gateway does not mean to publish (`trace_id` is
        // filled by the canonical-error middleware; `error_code` may be absent).
        let body = strip_nulls(body);

        let mut response = Response::builder()
            .status(status)
            .header(axum::http::header::CONTENT_TYPE, APPLICATION_PROBLEM_JSON)
            .header(
                HeaderName::from_static(gts::ERROR_SOURCE_HEADER),
                "gateway",
            )
            .body(Body::from(body.to_string()))
            .unwrap_or_else(|_| Response::new(Body::empty()));

        if let Some(secs) = self.retry_after_secs()
            && let Ok(value) = axum::http::HeaderValue::from_str(&secs.to_string()) {
                response.headers_mut().insert(
                    axum::http::header::RETRY_AFTER,
                    value,
                );
            }
        response
    }
}

/// Recursively removes `null` values from a JSON value so problem bodies stay tidy.
fn strip_nulls(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k, strip_nulls(v)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(strip_nulls).collect()),
        other => other,
    }
}

impl IntoResponse for GatewayError {
    fn into_response(self) -> Response {
        self.render_problem(None)
    }
}

impl From<anyhow::Error> for GatewayError {
    fn from(err: anyhow::Error) -> Self {
        Self::Internal(err.to_string())
    }
}

impl From<std::io::Error> for GatewayError {
    fn from(err: std::io::Error) -> Self {
        Self::Internal(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    #[test]
    fn status_mapping_covers_the_error_table() {
        assert_eq!(
            GatewayError::Validation("bad".into()).status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            GatewayError::AuthenticationFailed("no".into()).status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            GatewayError::RouteNotFound("no".into()).status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(GatewayError::PayloadTooLarge.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            GatewayError::RateLimited {
                detail: "d".into(),
                limit: 1,
                remaining: 0,
                reset_secs: 1,
                retry_after_secs: 5
            }
            .status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(
            GatewayError::SecretNotFound("cred://x".into()).status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            GatewayError::LinkUnavailable("down".into()).status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            GatewayError::Timeout {
                kind: TimeoutKind::Request,
                detail: "t".into()
            }
            .status(),
            StatusCode::GATEWAY_TIMEOUT
        );
    }

    #[test]
    fn retriable_errors_carry_retry_after() {
        let e = GatewayError::RateLimited {
            detail: "d".into(),
            limit: 10,
            remaining: 0,
            reset_secs: 7,
            retry_after_secs: 7,
        };
        assert_eq!(e.retry_after_secs(), Some(7));
        assert_eq!(e.gts_type(), err::RATE_LIMIT_EXCEEDED);
        assert_eq!(e.title(), "Rate Limit Exceeded");
    }

    #[test]
    fn internal_diagnostics_are_redacted_on_the_wire() {
        let e = GatewayError::Internal("secret at 10.0.0.1:5432".into());
        assert_eq!(e.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!e.to_string().contains("secret"));
        assert_eq!(e.title(), "Internal Error");
    }

    #[test]
    fn target_host_errors_carry_their_context() {
        let e = GatewayError::UnknownTargetHost {
            upstream_id: "u-1".into(),
            invalid_value: "apac.vendor.com".into(),
            valid_hosts: vec!["us.vendor.com".into()],
        };
        let ctx = e.context();
        assert_eq!(ctx["invalid_value"], "apac.vendor.com");
        assert_eq!(ctx["valid_hosts"][0], "us.vendor.com");
        assert_eq!(e.gts_type(), err::ROUTING_UNKNOWN_TARGET_HOST);
    }

    #[test]
    fn problem_body_carries_error_source_and_content_type() {
        let e = GatewayError::Validation("nope".into());
        let response = e.render_problem(Some("/oagw/v1/proxy/x"));
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response
                .headers()
                .get(gts::ERROR_SOURCE_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some("gateway")
        );
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some(APPLICATION_PROBLEM_JSON)
        );
    }

    #[test]
    fn plugin_in_use_reports_references() {
        let e = GatewayError::PluginInUse {
            plugin_id: "p".into(),
            upstreams: vec!["u1".into(), "u2".into()],
            routes: vec!["r1".into()],
        };
        let ctx = e.context();
        assert_eq!(ctx["referenced_by"]["upstreams"].as_array().unwrap().len(), 2);
        assert_eq!(ctx["referenced_by"]["routes"].as_array().unwrap().len(), 1);
        assert_eq!(e.status(), StatusCode::CONFLICT);
    }
}
