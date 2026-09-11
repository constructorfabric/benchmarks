//! Domain errors and their RFC 9457 / GTS mapping.
//!
//! The status code, GTS `type`, title and `Retry-After` behaviour of every
//! variant is fixed by `docs/DESIGN.md` §"Error Response Format".

use axum::http::StatusCode;

/// Domain-level failures, independent of any transport.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DomainError {
    /// Request or resource validation failed (400).
    #[error("{detail}")]
    Validation {
        /// Human-readable explanation.
        detail: String,
        /// Optional machine-readable code (e.g. `REQUIRED_HEADER_MISSING`).
        code: Option<String>,
    },
    /// A referenced resource does not exist (404).
    #[error("resource not found: {0}")]
    NotFound(String),
    /// No route matched the request (404).
    #[error("no matching route found")]
    RouteNotFound,
    /// Resource conflict, e.g. a duplicate alias (409).
    #[error("{0}")]
    Conflict(String),
    /// A plugin is still referenced by an upstream or route (409).
    #[error("plugin is still referenced: {0}")]
    PluginInUse(String),
    /// A multi-endpoint upstream requires `X-OAGW-Target-Host` (400).
    #[error("X-OAGW-Target-Host header is required for this upstream")]
    MissingTargetHost,
    /// `X-OAGW-Target-Host` is malformed (400).
    #[error("invalid X-OAGW-Target-Host value: {0}")]
    InvalidTargetHost(String),
    /// `X-OAGW-Target-Host` names no configured endpoint (400).
    #[error("unknown X-OAGW-Target-Host value: {0}")]
    UnknownTargetHost(String),
    /// Upstream authentication failed (401).
    #[error("authentication failed: {0}")]
    AuthenticationFailed(String),
    /// CORS origin is not allowed (403).
    #[error("origin not allowed: {0}")]
    CorsOriginNotAllowed(String),
    /// CORS method is not allowed (403).
    #[error("method not allowed: {0}")]
    CorsMethodNotAllowed(String),
    /// Request body exceeds the hard limit (413).
    #[error("request payload too large")]
    PayloadTooLarge,
    /// Token bucket exhausted (429).
    #[error("rate limit exceeded")]
    RateLimitExceeded {
        /// Seconds until a token becomes available.
        retry_after_secs: u64,
        /// Effective bucket capacity.
        limit: u64,
        /// Tokens left in the bucket.
        remaining: u64,
        /// Seconds until the bucket is fully replenished.
        reset_secs: u64,
    },
    /// A referenced secret cannot be resolved (500).
    #[error("referenced secret not found")]
    SecretNotFound,
    /// Protocol-level failure talking to the upstream (502).
    #[error("protocol error: {0}")]
    ProtocolError(String),
    /// Upstream service error (502).
    #[error("upstream service error: {0}")]
    DownstreamError(String),
    /// A streaming connection was aborted (502).
    #[error("stream aborted")]
    StreamAborted,
    /// The upstream is unreachable (503).
    #[error("upstream link unavailable: {0}")]
    LinkUnavailable(String),
    /// A referenced plugin cannot be resolved (503).
    #[error("plugin not found: {0}")]
    PluginNotFound(String),
    /// Connecting to the upstream timed out (504).
    #[error("connection timeout")]
    ConnectionTimeout,
    /// The upstream did not answer in time (504).
    #[error("request timeout")]
    RequestTimeout,
    /// An established stream was idle for too long (504).
    #[error("idle timeout")]
    IdleTimeout,
}

impl DomainError {
    /// Build a plain validation error.
    #[must_use]
    pub fn validation(detail: impl Into<String>) -> Self {
        Self::Validation {
            detail: detail.into(),
            code: None,
        }
    }

    /// Build a validation error carrying a machine-readable code.
    #[must_use]
    pub fn validation_with_code(detail: impl Into<String>, code: impl Into<String>) -> Self {
        Self::Validation {
            detail: detail.into(),
            code: Some(code.into()),
        }
    }

    /// HTTP status code for this error.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        match self {
            Self::Validation { .. }
            | Self::MissingTargetHost
            | Self::InvalidTargetHost(_)
            | Self::UnknownTargetHost(_) => StatusCode::BAD_REQUEST,
            Self::NotFound(_) | Self::RouteNotFound => StatusCode::NOT_FOUND,
            Self::Conflict(_) | Self::PluginInUse(_) => StatusCode::CONFLICT,
            Self::AuthenticationFailed(_) => StatusCode::UNAUTHORIZED,
            Self::CorsOriginNotAllowed(_) | Self::CorsMethodNotAllowed(_) => StatusCode::FORBIDDEN,
            Self::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::RateLimitExceeded { .. } => StatusCode::TOO_MANY_REQUESTS,
            Self::SecretNotFound => StatusCode::INTERNAL_SERVER_ERROR,
            Self::ProtocolError(_) | Self::DownstreamError(_) | Self::StreamAborted => {
                StatusCode::BAD_GATEWAY
            }
            Self::LinkUnavailable(_) | Self::PluginNotFound(_) => StatusCode::SERVICE_UNAVAILABLE,
            Self::ConnectionTimeout | Self::RequestTimeout | Self::IdleTimeout => {
                StatusCode::GATEWAY_TIMEOUT
            }
        }
    }

    /// GTS error identifier from the DESIGN error table.
    #[must_use]
    pub fn gts_type(&self) -> &'static str {
        match self {
            Self::Validation { .. } | Self::NotFound(_) | Self::Conflict(_) => {
                "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
            }
            Self::RouteNotFound => "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1",
            Self::PluginInUse(_) => "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1",
            Self::MissingTargetHost => {
                "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1"
            }
            Self::InvalidTargetHost(_) => {
                "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1"
            }
            Self::UnknownTargetHost(_) => {
                "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1"
            }
            Self::AuthenticationFailed(_) => "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1",
            Self::CorsOriginNotAllowed(_) => {
                "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1"
            }
            Self::CorsMethodNotAllowed(_) => {
                "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1"
            }
            Self::PayloadTooLarge => "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1",
            Self::RateLimitExceeded { .. } => {
                "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1"
            }
            Self::SecretNotFound => "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1",
            Self::ProtocolError(_) => "gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1",
            Self::DownstreamError(_) => "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1",
            Self::StreamAborted => "gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1",
            Self::LinkUnavailable(_) => "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1",
            Self::PluginNotFound(_) => "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1",
            Self::ConnectionTimeout => "gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1",
            Self::RequestTimeout => "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1",
            Self::IdleTimeout => "gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1",
        }
    }

    /// Short human-readable title for the problem document.
    #[must_use]
    pub fn title(&self) -> &'static str {
        match self {
            Self::Validation { .. } | Self::NotFound(_) | Self::Conflict(_) => "Validation Error",
            Self::RouteNotFound => "Route Not Found",
            Self::PluginInUse(_) => "Plugin In Use",
            Self::MissingTargetHost => "Missing Target Host Header",
            Self::InvalidTargetHost(_) => "Invalid Target Host Format",
            Self::UnknownTargetHost(_) => "Unknown Target Host",
            Self::AuthenticationFailed(_) => "Authentication Failed",
            Self::CorsOriginNotAllowed(_) => "Origin Not Allowed",
            Self::CorsMethodNotAllowed(_) => "Method Not Allowed",
            Self::PayloadTooLarge => "Payload Too Large",
            Self::RateLimitExceeded { .. } => "Rate Limit Exceeded",
            Self::SecretNotFound => "Secret Not Found",
            Self::ProtocolError(_) => "Protocol Error",
            Self::DownstreamError(_) => "Upstream Service Error",
            Self::StreamAborted => "Stream Aborted",
            Self::LinkUnavailable(_) => "Upstream Link Unavailable",
            Self::PluginNotFound(_) => "Plugin Not Found",
            Self::ConnectionTimeout => "Connection Timeout",
            Self::RequestTimeout => "Request Timeout",
            Self::IdleTimeout => "Idle Timeout",
        }
    }

    /// Machine-readable detail string.
    #[must_use]
    pub fn detail(&self) -> String {
        match self {
            Self::Validation { detail, .. }
            | Self::NotFound(detail)
            | Self::Conflict(detail)
            | Self::PluginInUse(detail)
            | Self::InvalidTargetHost(detail)
            | Self::UnknownTargetHost(detail)
            | Self::AuthenticationFailed(detail)
            | Self::CorsOriginNotAllowed(detail)
            | Self::CorsMethodNotAllowed(detail)
            | Self::ProtocolError(detail)
            | Self::DownstreamError(detail)
            | Self::LinkUnavailable(detail)
            | Self::PluginNotFound(detail) => detail.clone(),
            Self::RouteNotFound => "no route matched the request path and method".to_owned(),
            Self::MissingTargetHost => {
                "this upstream has multiple endpoints sharing a common alias; \
                 provide the X-OAGW-Target-Host header"
                    .to_owned()
            }
            Self::PayloadTooLarge => "request payload exceeds the maximum allowed size".to_owned(),
            Self::RateLimitExceeded { .. } => "rate limit exceeded".to_owned(),
            Self::SecretNotFound => "the referenced secret could not be resolved".to_owned(),
            Self::StreamAborted => "the proxied stream was aborted".to_owned(),
            Self::ConnectionTimeout => "connecting to the upstream timed out".to_owned(),
            Self::RequestTimeout => "the upstream did not respond in time".to_owned(),
            Self::IdleTimeout => "the upstream stream was idle for too long".to_owned(),
        }
    }

    /// `Retry-After` value in seconds, when the error carries one.
    #[must_use]
    pub const fn retry_after_secs(&self) -> Option<u64> {
        match self {
            Self::RateLimitExceeded {
                retry_after_secs, ..
            } => Some(*retry_after_secs),
            _ => None,
        }
    }

    /// Optional machine-readable code carried by the error.
    #[must_use]
    pub fn code(&self) -> Option<String> {
        match self {
            Self::Validation { code, .. } => code.clone(),
            _ => None,
        }
    }
}

/// RFC 9457 problem document extension fields.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct ProblemExtensions {
    /// Upstream identifier the request resolved to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_id: Option<String>,
    /// Upstream host the request targeted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Proxy path that produced the error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Seconds until a retriable error may be retried.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u64>,
    /// Machine-readable error code (e.g. `REQUIRED_HEADER_MISSING`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// Request trace identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

/// RFC 9457 problem document.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Problem {
    /// GTS error type identifier.
    #[serde(rename = "type")]
    pub type_uri: String,
    /// Short human-readable summary.
    pub title: String,
    /// HTTP status code.
    pub status: u16,
    /// Occurrence-specific explanation.
    pub detail: String,
    /// URI reference identifying the occurrence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    /// Extension fields.
    #[serde(flatten)]
    pub extensions: ProblemExtensions,
}

impl Problem {
    /// Build a problem document for a domain error.
    #[must_use]
    pub fn from_domain_error(err: &DomainError, instance: Option<String>) -> Self {
        let extensions = ProblemExtensions {
            retry_after_seconds: err.retry_after_secs(),
            error_code: err.code(),
            ..ProblemExtensions::default()
        };
        Self {
            type_uri: err.gts_type().to_owned(),
            title: err.title().to_owned(),
            status: err.status().as_u16(),
            detail: err.detail(),
            instance,
            extensions,
        }
    }

    /// Turn the document into an axum response with the gateway marker set.
    #[must_use]
    pub fn into_gateway_response(self) -> axum::response::Response {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut builder = axum::http::Response::builder()
            .status(status)
            .header(axum::http::header::CONTENT_TYPE, PROBLEM_CONTENT_TYPE)
            .header(
                axum::http::header::HeaderName::from_static(
                    crate::infra::proxy::headers::ERROR_SOURCE,
                ),
                axum::http::HeaderValue::from_static(crate::infra::proxy::cors::GATEWAY_SOURCE),
            );
        if let Some(retry) = self.extensions.retry_after_seconds {
            builder = builder.header(axum::http::header::RETRY_AFTER, retry);
        }
        let body = serde_json::to_string(&self).unwrap_or_else(|err| {
            format!(r#"{{"type":"gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1","title":"Protocol Error","status":500,"detail":"problem document could not be serialized: {err}"}}"#)
        });
        builder
            .body(axum::body::Body::from(body))
            .unwrap_or_else(|_| {
                axum::response::IntoResponse::into_response(
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                )
            })
    }
}

/// Media type of every gateway-generated error.
pub const PROBLEM_CONTENT_TYPE: &str = "application/problem+json";

impl From<DomainError> for Problem {
    fn from(err: DomainError) -> Self {
        Self::from_domain_error(&err, None)
    }
}

impl Problem {
    /// Set the `instance` field, returning `self` for chaining.
    #[must_use]
    pub fn with_instance_uri(mut self, instance: impl Into<String>) -> Self {
        self.instance = Some(instance.into());
        self
    }
}

impl axum::response::IntoResponse for Problem {
    fn into_response(self) -> axum::response::Response {
        self.into_gateway_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_maps_to_400_and_the_validation_type() {
        let err = DomainError::validation("bad");
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            err.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
        );
        assert_eq!(err.title(), "Validation Error");
        assert_eq!(err.detail(), "bad");
    }

    #[test]
    fn rate_limit_carries_retry_after() {
        let err = DomainError::RateLimitExceeded {
            retry_after_secs: 7,
            limit: 2,
            remaining: 0,
            reset_secs: 7,
        };
        assert_eq!(err.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(err.retry_after_secs(), Some(7));
    }

    #[test]
    fn timeouts_map_to_504() {
        for err in [
            DomainError::ConnectionTimeout,
            DomainError::RequestTimeout,
            DomainError::IdleTimeout,
        ] {
            assert_eq!(err.status(), StatusCode::GATEWAY_TIMEOUT);
        }
    }

    #[test]
    fn problem_document_carries_the_core_fields() {
        let problem = Problem::from_domain_error(&DomainError::RouteNotFound, Some("/p".into()));
        assert_eq!(problem.type_uri, DomainError::RouteNotFound.gts_type());
        assert_eq!(problem.status, 404);
        assert_eq!(problem.title, "Route Not Found");
        assert_eq!(problem.instance.as_deref(), Some("/p"));
    }
}
