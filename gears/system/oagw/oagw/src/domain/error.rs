//! OAGW error model.
//!
//! Every gateway error is rendered as an RFC 9457 `application/problem+json`
//! document whose `type` member is a GTS identifier
//! (`gts.cf.core.errors.err.v1~cf.oagw.<code>`), and carries the
//! `X-OAGW-Error-Source: gateway` header (ADR-0007). Upstream failures that are
//! passed through verbatim never produce a problem document — they are
//! forwarded with `X-OAGW-Error-Source: upstream`.

use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use serde_json::json;

use crate::domain::gts_helpers::error_id;

/// Side that produced the response (ADR-0007).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorSource {
    /// Originated inside OAGW.
    Gateway,
    /// Passed through from the upstream service.
    Upstream,
}

impl ErrorSource {
    /// Header value for `X-OAGW-Error-Source`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gateway => "gateway",
            Self::Upstream => "upstream",
        }
    }

    /// Name of the error-source header.
    pub const HEADER_NAME: &'static str = "X-OAGW-Error-Source";
}

/// Every OAGW failure mode, with its HTTP status, GTS type and title.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum OagwError {
    /// 400 — configuration or request failed validation.
    #[error("validation error: {0}")]
    Validation(String),
    /// 400 — `X-OAGW-Target-Host` is required for this upstream but absent.
    #[error("X-OAGW-Target-Host header is required")]
    MissingTargetHost,
    /// 400 — `X-OAGW-Target-Host` is malformed.
    #[error("X-OAGW-Target-Host header is invalid")]
    InvalidTargetHost,
    /// 400 — `X-OAGW-Target-Host` does not match any configured endpoint.
    #[error("X-OAGW-Target-Host does not match a configured endpoint")]
    UnknownTargetHost,
    /// 401 — outbound authentication to the upstream failed.
    #[error("authentication to upstream failed: {0}")]
    AuthenticationFailed(String),
    /// 404 — no matching route/upstream.
    #[error("route not found: {0}")]
    RouteNotFound(String),
    /// 409 — alias already used by another upstream in this tenant.
    #[error("alias conflict: {0}")]
    AliasConflict(String),
    /// 409 — route match rule collides with an existing route.
    #[error("route conflict: {0}")]
    RouteConflict(String),
    /// 409 — plugin still referenced by an upstream or route.
    #[error("plugin in use: {0}")]
    PluginInUse(String),
    /// 413 — request body exceeds the configured limit.
    #[error("payload too large")]
    PayloadTooLarge,
    /// 429 — rate limit exceeded.
    #[error("rate limit exceeded")]
    RateLimitExceeded {
        retry_after_secs: u64,
        limit: u64,
        remaining: u64,
        reset_epoch_secs: u64,
    },
    /// 500 — referenced credential secret is missing or inaccessible.
    #[error("secret not found: {0}")]
    SecretNotFound(String),
    /// 403 — CORS policy rejected the actual cross-origin request.
    #[error("cors policy rejected the request: {0}")]
    CorsForbidden(String),
    /// 502 — protocol-level error talking to the upstream.
    #[error("protocol error: {0}")]
    ProtocolError(String),
    /// 502 — upstream failed.
    #[error("downstream error: {0}")]
    DownstreamError(String),
    /// 502 — streaming connection aborted mid-response.
    #[error("stream aborted: {0}")]
    StreamAborted(String),
    /// 503 — upstream link unavailable.
    #[error("link unavailable: {0}")]
    LinkUnavailable(String),
    /// 503 — circuit breaker for this upstream is open.
    #[error("circuit breaker open")]
    CircuitBreakerOpen { retry_after_secs: u64 },
    /// 503 — referenced plugin is not registered.
    #[error("plugin not found: {0}")]
    PluginNotFound(String),
    /// 501 — the plugin exists but this build has no engine to run it.
    #[error("plugin execution is not available: {0}")]
    PluginRuntimeUnavailable(String),
    /// 504 — connection to the upstream timed out.
    #[error("connection timeout")]
    ConnectionTimeout,
    /// 504 — upstream did not answer in time.
    #[error("request timeout")]
    RequestTimeout,
    /// 504 — idle timeout while streaming.
    #[error("idle timeout")]
    IdleTimeout,
    /// 500 — unexpected internal failure.
    #[error("internal error: {0}")]
    Internal(String),
}

impl OagwError {
    /// HTTP status for this error.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        match self {
            Self::Validation(_)
            | Self::MissingTargetHost
            | Self::InvalidTargetHost
            | Self::UnknownTargetHost => StatusCode::BAD_REQUEST,
            Self::AuthenticationFailed(_) => StatusCode::UNAUTHORIZED,
            Self::RouteNotFound(_) => StatusCode::NOT_FOUND,
            Self::AliasConflict(_) | Self::RouteConflict(_) | Self::PluginInUse(_) => {
                StatusCode::CONFLICT
            }
            Self::CorsForbidden(_) => StatusCode::FORBIDDEN,
            Self::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::RateLimitExceeded { .. } => StatusCode::TOO_MANY_REQUESTS,
            Self::SecretNotFound(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::ProtocolError(_) | Self::DownstreamError(_) | Self::StreamAborted(_) => {
                StatusCode::BAD_GATEWAY
            }
            Self::LinkUnavailable(_) | Self::CircuitBreakerOpen { .. } | Self::PluginNotFound(_) => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            Self::PluginRuntimeUnavailable(_) => StatusCode::NOT_IMPLEMENTED,
            Self::ConnectionTimeout | Self::RequestTimeout | Self::IdleTimeout => {
                StatusCode::GATEWAY_TIMEOUT
            }
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// GTS `type` member for this error.
    #[must_use]
    pub fn gts_type(&self) -> String {
        error_id(match self {
            Self::Validation(_) => "validation.error.v1",
            Self::MissingTargetHost => "routing.missing_target_host.v1",
            Self::InvalidTargetHost => "routing.invalid_target_host.v1",
            Self::UnknownTargetHost => "routing.unknown_target_host.v1",
            Self::AuthenticationFailed(_) => "auth.failed.v1",
            Self::RouteNotFound(_) => "route.not_found.v1",
            Self::AliasConflict(_) => "upstream.alias.conflict.v1",
            Self::RouteConflict(_) => "route.match.conflict.v1",
            Self::PluginInUse(_) => "plugin.in_use.v1",
            Self::PayloadTooLarge => "payload.too_large.v1",
            Self::CorsForbidden(_) => "cors.forbidden.v1",
            Self::RateLimitExceeded { .. } => "rate_limit.exceeded.v1",
            Self::SecretNotFound(_) => "secret.not_found.v1",
            Self::ProtocolError(_) => "protocol.error.v1",
            Self::DownstreamError(_) => "downstream.error.v1",
            Self::StreamAborted(_) => "stream.aborted.v1",
            Self::LinkUnavailable(_) => "link.unavailable.v1",
            Self::CircuitBreakerOpen { .. } => "circuit_breaker.open.v1",
            Self::PluginNotFound(_) => "plugin.not_found.v1",
            Self::PluginRuntimeUnavailable(_) => "plugin.runtime_unavailable.v1",
            Self::ConnectionTimeout => "timeout.connection.v1",
            Self::RequestTimeout => "timeout.request.v1",
            Self::IdleTimeout => "timeout.idle.v1",
            Self::Internal(_) => "validation.error.v1",
        })
    }

    /// Human-readable `title` member.
    #[must_use]
    pub fn title(&self) -> &'static str {
        match self {
            Self::Validation(_) => "Validation Error",
            Self::MissingTargetHost => "Missing Target Host",
            Self::InvalidTargetHost => "Invalid Target Host",
            Self::UnknownTargetHost => "Unknown Target Host",
            Self::AuthenticationFailed(_) => "Authentication Failed",
            Self::RouteNotFound(_) => "Route Not Found",
            Self::AliasConflict(_) => "Alias Conflict",
            Self::RouteConflict(_) => "Route Conflict",
            Self::PluginInUse(_) => "Plugin In Use",
            Self::CorsForbidden(_) => "Cors Forbidden",
            Self::PayloadTooLarge => "Payload Too Large",
            Self::RateLimitExceeded { .. } => "Rate Limit Exceeded",
            Self::SecretNotFound(_) => "Secret Not Found",
            Self::ProtocolError(_) => "Protocol Error",
            Self::DownstreamError(_) => "Downstream Error",
            Self::StreamAborted(_) => "Stream Aborted",
            Self::LinkUnavailable(_) => "Link Unavailable",
            Self::CircuitBreakerOpen { .. } => "Circuit Breaker Open",
            Self::PluginNotFound(_) => "Plugin Not Found",
            Self::PluginRuntimeUnavailable(_) => "Plugin Runtime Unavailable",
            Self::ConnectionTimeout => "Connection Timeout",
            Self::RequestTimeout => "Request Timeout",
            Self::IdleTimeout => "Idle Timeout",
            Self::Internal(_) => "Internal Error",
        }
    }

    /// `detail` member: the error's own explanation.
    #[must_use]
    pub fn detail(&self) -> String {
        match self {
            Self::Validation(detail)
            | Self::AuthenticationFailed(detail)
            | Self::RouteNotFound(detail)
            | Self::AliasConflict(detail)
            | Self::RouteConflict(detail)
            | Self::SecretNotFound(detail)
            | Self::ProtocolError(detail)
            | Self::DownstreamError(detail)
            | Self::StreamAborted(detail)
            | Self::LinkUnavailable(detail)
            | Self::PluginNotFound(detail)
            | Self::PluginRuntimeUnavailable(detail)
            | Self::CorsForbidden(detail)
            | Self::Internal(detail) => detail.clone(),
            Self::RateLimitExceeded { .. } => "Rate limit exceeded for this tenant.".to_owned(),
            Self::PluginInUse(id) => {
                format!("plugin {id} is still referenced by an upstream or route")
            }
            _ => self.to_string(),
        }
    }

    /// Retry guidance in seconds, when the error is retriable.
    #[must_use]
    pub const fn retry_after_secs(&self) -> Option<u64> {
        match self {
            Self::RateLimitExceeded {
                retry_after_secs, ..
            }
            | Self::CircuitBreakerOpen { retry_after_secs } => Some(*retry_after_secs),
            _ => None,
        }
    }

    /// OAGW extension members carried in the problem document.
    #[must_use]
    pub fn extensions(&self) -> Vec<(&'static str, serde_json::Value)> {
        let mut fields: Vec<(&'static str, serde_json::Value)> = Vec::new();
        match self {
            Self::RateLimitExceeded {
                limit,
                remaining,
                reset_epoch_secs,
                ..
            } => {
                fields.push(("rate_limit", json!(*limit)));
                fields.push(("rate_limit_remaining", json!(remaining)));
                fields.push(("rate_limit_reset", json!(reset_epoch_secs)));
            }
            Self::PluginInUse(id) => {
                fields.push(("plugin_id", json!(id)));
            }
            _ => {}
        }
        if let Some(retry) = self.retry_after_secs() {
            fields.push(("retry_after_seconds", json!(retry)));
        }
        fields
    }

    /// RFC 9457 problem document for this error.
    #[must_use]
    pub fn to_problem(&self, instance: Option<&str>) -> serde_json::Value {
        let mut doc = json!({
            "type": self.gts_type(),
            "title": self.title(),
            "status": self.status().as_u16(),
            "detail": self.detail(),
            // The canonical error contract always carries a (possibly empty)
            // context object; OAGW's own extension fields stay at the top
            // level per ADR-0007.
            "context": {},
        });
        if let Some(instance) = instance {
            doc["instance"] = json!(instance);
        }
        if let Some(obj) = doc.as_object_mut() {
            for (key, value) in self.extensions() {
                obj.insert(key.to_owned(), value);
            }
        }
        doc
    }

    /// Headers that must accompany a gateway error response.
    #[must_use]
    pub fn error_headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            ErrorSource::HEADER_NAME,
            HeaderValue::from_static(ErrorSource::Gateway.as_str()),
        );
        if let Some(retry) = self.retry_after_secs() {
            if let Ok(value) = HeaderValue::from_str(&retry.to_string()) {
                headers.insert(header::RETRY_AFTER, value);
            }
        }
        if let Self::RateLimitExceeded {
            limit,
            remaining,
            reset_epoch_secs,
            ..
        } = self
        {
            let mut push = |name: &'static str, value: String| {
                if let Ok(value) = HeaderValue::from_str(&value) {
                    headers.insert(name, value);
                }
            };
            push("X-RateLimit-Limit", limit.to_string());
            push("X-RateLimit-Remaining", remaining.to_string());
            push("X-RateLimit-Reset", reset_epoch_secs.to_string());
        }
        headers
    }
}

impl IntoResponse for OagwError {
    fn into_response(self) -> axum::response::Response {
        let status = self.status();
        let headers = self.error_headers();
        let body = self.to_problem(None);
        let mut response = (status, axum::Json(body)).into_response();
        // RFC 9457 problem documents carry their own media type; axum's
        // `Json` would have stamped plain `application/json`.
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/problem+json"),
        );
        for (name, value) in &headers {
            response.headers_mut().insert(name.clone(), value.clone());
        }
        response
    }
}

/// Result alias used across the OAGW crate.
pub type OagwResult<T> = Result<T, OagwError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limit_error_carries_headers() {
        let err = OagwError::RateLimitExceeded {
            retry_after_secs: 30,
            limit: 100,
            remaining: 0,
            reset_epoch_secs: 1_706_626_800,
        };
        assert_eq!(err.status(), StatusCode::TOO_MANY_REQUESTS);
        let headers = err.error_headers();
        assert_eq!(
            headers.get("X-RateLimit-Limit").and_then(|v| v.to_str().ok()),
            Some("100")
        );
        assert_eq!(
            headers.get(header::RETRY_AFTER).and_then(|v| v.to_str().ok()),
            Some("30")
        );
        assert_eq!(
            headers
                .get(ErrorSource::HEADER_NAME)
                .and_then(|v| v.to_str().ok()),
            Some("gateway")
        );
    }

    #[test]
    fn problem_document_is_rfc9457() {
        let err = OagwError::RouteNotFound("no route for GET /x".to_owned());
        let doc = err.to_problem(Some("/oagw/v1/proxy/foo/x"));
        assert_eq!(doc["status"], 404);
        assert_eq!(
            doc["type"],
            "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
        );
        assert_eq!(doc["instance"], "/oagw/v1/proxy/foo/x");
        assert!(doc["detail"].as_str().is_some());
    }

    #[test]
    fn conflict_statuses() {
        assert_eq!(OagwError::AliasConflict("dup".into()).status(), StatusCode::CONFLICT);
        assert_eq!(OagwError::RouteConflict("dup".into()).status(), StatusCode::CONFLICT);
    }
}
