//! OAGW error type with RFC 9457 `application/problem+json` wire projection.
//!
//! Every gateway-generated error carries:
//! - `X-OAGW-Error-Source: gateway` header (ADR-0007)
//! - an `application/problem+json` body with a GTS `type` identifier from
//!   the error catalog in `docs/DESIGN.md`
//! - extension fields (`upstream_id`, `host`, `path`, `alias`,
//!   `retry_after_seconds`, ...) where the catalog specifies them
//!
//! Upstream (passthrough) errors are NOT represented here — the data plane
//! forwards the upstream body unchanged with `X-OAGW-Error-Source: upstream`.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use serde_json::{json, Value};

use crate::gts_helpers;

/// Shared request context attached to gateway errors (DESIGN.md extension
/// fields). Populated by the handlers after upstream/route resolution so a
/// single `OagwError` can be returned from anywhere in the flow.
#[derive(Debug, Clone, Default)]
pub struct ErrorContext {
    /// URI reference identifying the occurrence (RFC 9457 `instance`).
    pub instance: Option<String>,
    /// GTS identifier of the upstream involved, when known.
    pub upstream_id: Option<String>,
    /// Effective target host.
    pub host: Option<String>,
    /// The alias used to route the request.
    pub alias: Option<String>,
}

/// Gateway-generated errors. Maps to the error catalog in `DESIGN.md`.
#[derive(Debug, Clone)]
pub enum OagwError {
    /// 400 — general validation error.
    Validation { detail: String },
    /// 400 — `X-OAGW-Target-Host` required but missing (multi-endpoint,
    /// common-suffix alias).
    MissingTargetHost {
        detail: String,
        valid_hosts: Vec<String>,
    },
    /// 400 — `X-OAGW-Target-Host` malformed.
    InvalidTargetHost {
        detail: String,
        invalid_value: String,
        valid_hosts: Vec<String>,
    },
    /// 400 — `X-OAGW-Target-Host` does not match a configured endpoint.
    UnknownTargetHost {
        detail: String,
        invalid_value: String,
        valid_hosts: Vec<String>,
    },
    /// 403 — CORS origin not allowed (actual request).
    CorsOriginNotAllowed { detail: String },
    /// 403 — CORS method not allowed (actual request).
    CorsMethodNotAllowed { detail: String },
    /// 401 — upstream authentication failed / secret inaccessible.
    AuthFailed { detail: String },
    /// 404 — no route matched, or resource not found on the management API.
    NotFound { detail: String },
    /// 409 — resource conflict (alias clash, plugin in use, match clash).
    Conflict { detail: String },
    /// 413 — request payload exceeds limits.
    PayloadTooLarge { detail: String },
    /// 429 — rate limit exceeded.
    RateLimitExceeded {
        detail: String,
        retry_after_seconds: u64,
        limit: u64,
        remaining: u64,
        reset_epoch_secs: u64,
    },
    /// 500 — referenced secret not found / credstore failure.
    SecretNotFound { detail: String },
    /// 502 — protocol-level error.
    ProtocolError { detail: String },
    /// 502 — downstream/upstream transport error.
    DownstreamError { detail: String },
    /// 502 — upstream stream aborted.
    StreamAborted { detail: String },
    /// 503 — upstream link unavailable.
    LinkUnavailable { detail: String },
    /// 503 — plugin referenced but not resolvable.
    PluginNotFound { detail: String },
    /// 504 — connection timeout.
    ConnectionTimeout { detail: String },
    /// 504 — request timeout.
    RequestTimeout { detail: String },
    /// 504 — idle timeout.
    IdleTimeout { detail: String },
    /// 500 — internal fallback.
    Internal { detail: String },
    /// 403 — scope/permission denied.
    Forbidden { detail: String },
    /// Guard plugin rejection with an explicit status/type (e.g.
    /// `REQUIRED_HEADER_MISSING`: 400 request phase / 502 response phase).
    GuardRejected {
        status: u16,
        problem_type: &'static str,
        detail: String,
    },
}

impl OagwError {
    /// A 400 validation error.
    #[must_use]
    pub fn validation(detail: impl Into<String>) -> Self {
        Self::Validation {
            detail: detail.into(),
        }
    }

    /// A 404 not-found error.
    #[must_use]
    pub fn not_found(detail: impl Into<String>) -> Self {
        Self::NotFound {
            detail: detail.into(),
        }
    }

    /// A 409 conflict error.
    #[must_use]
    pub fn conflict(detail: impl Into<String>) -> Self {
        Self::Conflict {
            detail: detail.into(),
        }
    }

    /// An internal 500 error.
    #[must_use]
    pub fn internal(detail: impl Into<String>) -> Self {
        Self::Internal {
            detail: detail.into(),
        }
    }

    /// The RFC 9457 GTS `type` identifier for this error.
    #[must_use]
    pub fn problem_type(&self) -> &'static str {
        match self {
            Self::Validation { .. } => gts_helpers::ERR_VALIDATION,
            Self::MissingTargetHost { .. } => gts_helpers::ERR_MISSING_TARGET_HOST,
            Self::InvalidTargetHost { .. } => gts_helpers::ERR_INVALID_TARGET_HOST,
            Self::UnknownTargetHost { .. } => gts_helpers::ERR_UNKNOWN_TARGET_HOST,
            Self::CorsOriginNotAllowed { .. } => gts_helpers::ERR_CORS_ORIGIN_NOT_ALLOWED,
            Self::CorsMethodNotAllowed { .. } => gts_helpers::ERR_CORS_METHOD_NOT_ALLOWED,
            Self::AuthFailed { .. } => gts_helpers::ERR_AUTH_FAILED,
            Self::NotFound { .. } => gts_helpers::ERR_ROUTE_NOT_FOUND,
            Self::Conflict { .. } => gts_helpers::ERR_PLUGIN_IN_USE,
            Self::PayloadTooLarge { .. } => gts_helpers::ERR_PAYLOAD_TOO_LARGE,
            Self::RateLimitExceeded { .. } => gts_helpers::ERR_RATE_LIMIT_EXCEEDED,
            Self::SecretNotFound { .. } => gts_helpers::ERR_SECRET_NOT_FOUND,
            Self::ProtocolError { .. } => gts_helpers::ERR_PROTOCOL_ERROR,
            Self::DownstreamError { .. } => gts_helpers::ERR_DOWNSTREAM_ERROR,
            Self::StreamAborted { .. } => gts_helpers::ERR_STREAM_ABORTED,
            Self::LinkUnavailable { .. } => gts_helpers::ERR_LINK_UNAVAILABLE,
            Self::PluginNotFound { .. } => gts_helpers::ERR_PLUGIN_NOT_FOUND,
            Self::ConnectionTimeout { .. } => gts_helpers::ERR_CONNECTION_TIMEOUT,
            Self::RequestTimeout { .. } => gts_helpers::ERR_REQUEST_TIMEOUT,
            Self::IdleTimeout { .. } => gts_helpers::ERR_IDLE_TIMEOUT,
            Self::Internal { .. } => gts_helpers::ERR_DOWNSTREAM_ERROR,
            Self::Forbidden { .. } => gts_helpers::ERR_VALIDATION,
            Self::GuardRejected {
                problem_type, ..
            } => problem_type,
        }
    }

    /// Human-readable title.
    #[must_use]
    pub fn title(&self) -> &'static str {
        match self {
            Self::Validation { .. } => "Validation Error",
            Self::MissingTargetHost { .. } => "Missing Target Host Header",
            Self::InvalidTargetHost { .. } => "Invalid Target Host Format",
            Self::UnknownTargetHost { .. } => "Unknown Target Host",
            Self::CorsOriginNotAllowed { .. } => "CORS Origin Not Allowed",
            Self::CorsMethodNotAllowed { .. } => "CORS Method Not Allowed",
            Self::AuthFailed { .. } => "Authentication Failed",
            Self::NotFound { .. } => "Route Not Found",
            Self::Conflict { .. } => "Conflict",
            Self::PayloadTooLarge { .. } => "Payload Too Large",
            Self::RateLimitExceeded { .. } => "Rate Limit Exceeded",
            Self::SecretNotFound { .. } => "Secret Not Found",
            Self::ProtocolError { .. } => "Protocol Error",
            Self::DownstreamError { .. } => "Downstream Error",
            Self::StreamAborted { .. } => "Stream Aborted",
            Self::LinkUnavailable { .. } => "Link Unavailable",
            Self::PluginNotFound { .. } => "Plugin Not Found",
            Self::ConnectionTimeout { .. } => "Connection Timeout",
            Self::RequestTimeout { .. } => "Request Timeout",
            Self::IdleTimeout { .. } => "Idle Timeout",
            Self::Internal { .. } => "Internal Error",
            Self::Forbidden { .. } => "Forbidden",
            Self::GuardRejected { .. } => "Request Rejected",
        }
    }

    /// HTTP status for this error.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        match self {
            Self::Validation { .. }
            | Self::MissingTargetHost { .. }
            | Self::InvalidTargetHost { .. }
            | Self::UnknownTargetHost { .. } => StatusCode::BAD_REQUEST,
            Self::CorsOriginNotAllowed { .. } | Self::CorsMethodNotAllowed { .. } | Self::Forbidden { .. } => {
                StatusCode::FORBIDDEN
            }
            Self::AuthFailed { .. } => StatusCode::UNAUTHORIZED,
            Self::NotFound { .. } => StatusCode::NOT_FOUND,
            Self::Conflict { .. } => StatusCode::CONFLICT,
            Self::PayloadTooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
            Self::RateLimitExceeded { .. } => StatusCode::TOO_MANY_REQUESTS,
            Self::SecretNotFound { .. } | Self::Internal { .. } => StatusCode::INTERNAL_SERVER_ERROR,
            Self::GuardRejected { status, .. } => {
                StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_REQUEST)
            }
            Self::ProtocolError { .. }
            | Self::DownstreamError { .. }
            | Self::StreamAborted { .. } => StatusCode::BAD_GATEWAY,
            Self::LinkUnavailable { .. } | Self::PluginNotFound { .. } => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            Self::ConnectionTimeout { .. }
            | Self::RequestTimeout { .. }
            | Self::IdleTimeout { .. } => StatusCode::GATEWAY_TIMEOUT,
        }
    }

    /// Detail message.
    #[must_use]
    pub fn detail(&self) -> &str {
        match self {
            Self::Validation { detail }
            | Self::MissingTargetHost { detail, .. }
            | Self::InvalidTargetHost { detail, .. }
            | Self::UnknownTargetHost { detail, .. }
            | Self::CorsOriginNotAllowed { detail }
            | Self::CorsMethodNotAllowed { detail }
            | Self::AuthFailed { detail }
            | Self::NotFound { detail }
            | Self::Conflict { detail }
            | Self::PayloadTooLarge { detail }
            | Self::RateLimitExceeded { detail, .. }
            | Self::SecretNotFound { detail }
            | Self::ProtocolError { detail }
            | Self::DownstreamError { detail }
            | Self::StreamAborted { detail }
            | Self::LinkUnavailable { detail }
            | Self::PluginNotFound { detail }
            | Self::ConnectionTimeout { detail }
            | Self::RequestTimeout { detail }
            | Self::IdleTimeout { detail }
            | Self::Internal { detail }
            | Self::Forbidden { detail }
            | Self::GuardRejected { detail, .. } => detail,
        }
    }

    /// Per-error extra extension fields (beyond the shared `ErrorContext`).
    fn extensions(&self) -> Value {
        match self {
            Self::MissingTargetHost { valid_hosts, .. }
            | Self::InvalidTargetHost { valid_hosts, .. }
            | Self::UnknownTargetHost { valid_hosts, .. } => {
                json!({ "valid_hosts": valid_hosts })
            }
            Self::InvalidTargetHost { invalid_value, .. }
            | Self::UnknownTargetHost { invalid_value, .. } => {
                json!({ "invalid_value": invalid_value })
            }
            Self::RateLimitExceeded {
                retry_after_seconds,
                limit,
                remaining,
                reset_epoch_secs,
                ..
            } => json!({
                "retry_after_seconds": retry_after_seconds,
                "limit": limit,
                "remaining": remaining,
                "reset": reset_epoch_secs,
            }),
            _ => Value::Null,
        }
    }

    /// Build the RFC 9457 problem body value.
    fn problem_body(&self, ctx: &ErrorContext) -> Value {
        let mut body = json!({
            "type": self.problem_type(),
            "title": self.title(),
            "status": self.status().as_u16(),
            "detail": self.detail(),
        });
        if let Some(instance) = &ctx.instance {
            body["instance"] = Value::String(instance.clone());
        }
        if let Some(upstream_id) = &ctx.upstream_id {
            body["upstream_id"] = Value::String(upstream_id.clone());
        }
        if let Some(host) = &ctx.host {
            body["host"] = Value::String(host.clone());
        }
        if let Some(alias) = &ctx.alias {
            body["alias"] = Value::String(alias.clone());
        }
        if let Value::Object(ext) = self.extensions() {
            for (k, v) in ext {
                body[k] = v;
            }
        }
        body
    }
}

impl std::fmt::Display for OagwError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.detail(), self.status())
    }
}

impl std::error::Error for OagwError {}

/// Wire projection for a gateway error.
#[derive(Serialize)]
struct ProblemBody {
    #[serde(rename = "type")]
    problem_type: &'static str,
    title: &'static str,
    status: u16,
    detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    instance: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    alias: Option<String>,
    #[serde(skip_serializing_if = "Value::is_null", flatten)]
    extensions: Value,
}

impl IntoResponse for OagwError {
    fn into_response(self) -> Response {
        let ctx = ErrorContext::default();
        self.into_response_with_context(ctx)
    }
}

impl OagwError {
    /// Render this error with an explicit request context (instance
    /// reference, upstream id, host, alias).
    #[must_use]
    pub fn into_response_with_context(self, ctx: ErrorContext) -> Response {
        let status = self.status();
        let retry_after = match &self {
            Self::RateLimitExceeded {
                retry_after_seconds, ..
            } => Some(*retry_after_seconds),
            _ => None,
        };

        let body = ProblemBody {
            problem_type: self.problem_type(),
            title: self.title(),
            status: status.as_u16(),
            detail: self.detail().to_owned(),
            instance: ctx.instance.clone(),
            upstream_id: ctx.upstream_id.clone(),
            host: ctx.host.clone(),
            alias: ctx.alias.clone(),
            extensions: self.extensions(),
        };

        let mut response = (status, Json(body)).into_response();
        response.headers_mut().insert(
            "x-oagw-error-source",
            axum::http::HeaderValue::from_static("gateway"),
        );
        if let Some(secs) = retry_after {
            response
                .headers_mut()
                .insert(axum::http::header::RETRY_AFTER, secs.to_string().parse().unwrap());
        }
        response
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::header;
    use http_body_util::BodyExt;

    async fn body_json(resp: Response) -> serde_json::Value {
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn rate_limit_error_wire_shape() {
        let err = OagwError::RateLimitExceeded {
            detail: "Rate limit exceeded for upstream api.openai.com".into(),
            retry_after_seconds: 15,
            limit: 100,
            remaining: 0,
            reset_epoch_secs: 1706626800,
        };
        let resp = err.into_response_with_context(ErrorContext {
            instance: Some("/oagw/v1/proxy/api.openai.com/v1/chat/completions".into()),
            upstream_id: Some("gts.cf.core.oagw.upstream.v1~7c9e6679-7425-40de-944b-e07fc1f90ae7".into()),
            host: Some("api.openai.com".into()),
            alias: Some("api.openai.com".into()),
        });

        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        let headers = resp.headers();
        assert_eq!(
            headers.get("x-oagw-error-source").unwrap(),
            "gateway"
        );
        assert_eq!(headers.get(header::RETRY_AFTER).unwrap(), "15");

        let body = body_json(resp).await;
        assert_eq!(
            body["type"],
            gts_helpers::ERR_RATE_LIMIT_EXCEEDED
        );
        assert_eq!(body["status"], 429);
        assert_eq!(body["retry_after_seconds"], 15);
        assert_eq!(body["limit"], 100);
        assert_eq!(body["remaining"], 0);
        assert_eq!(body["host"], "api.openai.com");
        assert_eq!(
            body["upstream_id"],
            "gts.cf.core.oagw.upstream.v1~7c9e6679-7425-40de-944b-e07fc1f90ae7"
        );
        assert_eq!(
            body["instance"],
            "/oagw/v1/proxy/api.openai.com/v1/chat/completions"
        );
    }

    #[tokio::test]
    async fn missing_target_host_wire_shape() {
        let err = OagwError::MissingTargetHost {
            detail: "X-OAGW-Target-Host header required for multi-endpoint upstream with common suffix alias. Valid hosts: [us.vendor.com, eu.vendor.com]".into(),
            valid_hosts: vec!["us.vendor.com".into(), "eu.vendor.com".into()],
        };
        let resp = err.into_response_with_context(ErrorContext {
            instance: Some("/oagw/v1/proxy/vendor.com/v1/api/resource".into()),
            ..Default::default()
        });
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_json(resp).await;
        assert_eq!(body["type"], gts_helpers::ERR_MISSING_TARGET_HOST);
        assert_eq!(body["valid_hosts"][0], "us.vendor.com");
        assert_eq!(body["valid_hosts"][1], "eu.vendor.com");
    }

    #[test]
    fn route_not_found_default() {
        let err = OagwError::not_found("No matching route found");
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
        assert_eq!(err.problem_type(), gts_helpers::ERR_ROUTE_NOT_FOUND);
    }
}
