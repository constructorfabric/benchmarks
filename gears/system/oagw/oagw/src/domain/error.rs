//! Gateway error taxonomy.
//!
//! Every variant maps 1:1 onto a row of the "Error Response Format" table in
//! `DESIGN.md` §3.3: an HTTP status, a GTS `type` identifier and a title. The
//! wire rendering (RFC 9457 `application/problem+json` plus the
//! `X-OAGW-Error-Source` header of ADR-0007) lives in `api::rest::error`.

use std::collections::BTreeMap;

use serde_json::Value;

/// Whether an error was produced by the gateway or passed through from the
/// upstream service (ADR-0007).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorSource {
    Gateway,
    Upstream,
}

impl ErrorSource {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gateway => "gateway",
            Self::Upstream => "upstream",
        }
    }
}

/// Canonical OAGW error kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Validation,
    MissingTargetHost,
    InvalidTargetHost,
    UnknownTargetHost,
    AuthenticationFailed,
    CorsOriginNotAllowed,
    CorsMethodNotAllowed,
    Forbidden,
    RouteNotFound,
    NotFound,
    Conflict,
    PluginInUse,
    PayloadTooLarge,
    RateLimitExceeded,
    SecretNotFound,
    Internal,
    ProtocolError,
    DownstreamError,
    StreamAborted,
    LinkUnavailable,
    CircuitBreakerOpen,
    PluginNotFound,
    UpstreamDisabled,
    ConnectionTimeout,
    RequestTimeout,
    IdleTimeout,
}

impl ErrorKind {
    #[must_use]
    pub fn status(self) -> u16 {
        match self {
            Self::Validation
            | Self::MissingTargetHost
            | Self::InvalidTargetHost
            | Self::UnknownTargetHost => 400,
            Self::AuthenticationFailed => 401,
            Self::CorsOriginNotAllowed | Self::CorsMethodNotAllowed | Self::Forbidden => 403,
            Self::RouteNotFound | Self::NotFound => 404,
            Self::Conflict | Self::PluginInUse => 409,
            Self::PayloadTooLarge => 413,
            Self::RateLimitExceeded => 429,
            Self::SecretNotFound | Self::Internal => 500,
            Self::ProtocolError | Self::DownstreamError | Self::StreamAborted => 502,
            Self::LinkUnavailable
            | Self::CircuitBreakerOpen
            | Self::PluginNotFound
            | Self::UpstreamDisabled => 503,
            Self::ConnectionTimeout | Self::RequestTimeout | Self::IdleTimeout => 504,
        }
    }

    #[must_use]
    pub fn gts_type(self) -> &'static str {
        match self {
            Self::Validation => "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
            Self::MissingTargetHost => {
                "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1"
            }
            Self::InvalidTargetHost => {
                "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1"
            }
            Self::UnknownTargetHost => {
                "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1"
            }
            Self::AuthenticationFailed => "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1",
            Self::CorsOriginNotAllowed => {
                "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1"
            }
            Self::CorsMethodNotAllowed => {
                "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1"
            }
            Self::Forbidden => "gts.cf.core.errors.err.v1~cf.oagw.access.denied.v1",
            Self::RouteNotFound | Self::NotFound => {
                "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
            }
            Self::Conflict => "gts.cf.core.errors.err.v1~cf.oagw.conflict.v1",
            Self::PluginInUse => "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1",
            Self::PayloadTooLarge => "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1",
            Self::RateLimitExceeded => "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1",
            Self::SecretNotFound => "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1",
            Self::Internal => "gts.cf.core.errors.err.v1~cf.oagw.internal.error.v1",
            Self::ProtocolError => "gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1",
            Self::DownstreamError => "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1",
            Self::StreamAborted => "gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1",
            Self::LinkUnavailable | Self::UpstreamDisabled => {
                "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1"
            }
            Self::CircuitBreakerOpen => "gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1",
            Self::PluginNotFound => "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1",
            Self::ConnectionTimeout => "gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1",
            Self::RequestTimeout => "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1",
            Self::IdleTimeout => "gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1",
        }
    }

    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::Validation => "Validation Error",
            Self::MissingTargetHost => "Missing Target Host Header",
            Self::InvalidTargetHost => "Invalid Target Host Format",
            Self::UnknownTargetHost => "Unknown Target Host",
            Self::AuthenticationFailed => "Authentication Failed",
            Self::CorsOriginNotAllowed => "CORS Origin Not Allowed",
            Self::CorsMethodNotAllowed => "CORS Method Not Allowed",
            Self::Forbidden => "Access Denied",
            Self::RouteNotFound | Self::NotFound => "Route Not Found",
            Self::Conflict => "Conflict",
            Self::PluginInUse => "Plugin In Use",
            Self::PayloadTooLarge => "Payload Too Large",
            Self::RateLimitExceeded => "Rate Limit Exceeded",
            Self::SecretNotFound => "Secret Not Found",
            Self::Internal => "Internal Error",
            Self::ProtocolError => "Protocol Error",
            Self::DownstreamError => "Downstream Error",
            Self::StreamAborted => "Stream Aborted",
            Self::LinkUnavailable => "Link Unavailable",
            Self::UpstreamDisabled => "Upstream Disabled",
            Self::CircuitBreakerOpen => "Circuit Breaker Open",
            Self::PluginNotFound => "Plugin Not Found",
            Self::ConnectionTimeout => "Connection Timeout",
            Self::RequestTimeout => "Request Timeout",
            Self::IdleTimeout => "Idle Timeout",
        }
    }

    /// Retriability as tabulated in `PRD.md` §5.6 / `DESIGN.md` §3.3.
    #[must_use]
    pub fn retriable(self) -> bool {
        matches!(
            self,
            Self::RateLimitExceeded
                | Self::CircuitBreakerOpen
                | Self::LinkUnavailable
                | Self::UpstreamDisabled
                | Self::ConnectionTimeout
                | Self::RequestTimeout
                | Self::IdleTimeout
        )
    }
}

/// A gateway error with its RFC 9457 extension members.
#[derive(Debug, Clone)]
pub struct OagwError {
    pub kind: ErrorKind,
    pub detail: String,
    /// RFC 9457 extension members, rendered as sibling keys of the envelope.
    pub extensions: BTreeMap<String, Value>,
    /// Seconds to place in `Retry-After` (and `retry_after_seconds`).
    pub retry_after_seconds: Option<u64>,
    /// Extra response headers to emit alongside the Problem document, e.g.
    /// the `X-RateLimit-*` family on a 429.
    pub extra_headers: Vec<(String, String)>,
}

impl OagwError {
    #[must_use]
    pub fn new(kind: ErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
            extensions: BTreeMap::new(),
            retry_after_seconds: None,
            extra_headers: Vec::new(),
        }
    }

    #[must_use]
    pub fn validation(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::Validation, detail)
    }

    #[must_use]
    pub fn not_found(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, detail)
    }

    #[must_use]
    pub fn conflict(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::Conflict, detail)
    }

    #[must_use]
    pub fn internal(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::Internal, detail)
    }

    #[must_use]
    pub fn with_ext(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.extensions.insert(key.to_owned(), value.into());
        self
    }

    #[must_use]
    pub fn with_retry_after(mut self, seconds: u64) -> Self {
        self.retry_after_seconds = Some(seconds);
        self
    }

    /// Attach a response header to emit with the Problem document.
    #[must_use]
    pub fn with_header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.extra_headers.push((name.to_owned(), value.into()));
        self
    }

    /// Attach every header of `headers`.
    #[must_use]
    pub fn with_headers(mut self, headers: &http::HeaderMap) -> Self {
        for (name, value) in headers {
            if let Ok(value) = value.to_str() {
                self.extra_headers
                    .push((name.as_str().to_owned(), value.to_owned()));
            }
        }
        self
    }

    #[must_use]
    pub fn status(&self) -> u16 {
        self.kind.status()
    }
}

impl std::fmt::Display for OagwError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind.title(), self.detail)
    }
}

impl std::error::Error for OagwError {}

pub type OagwResult<T> = Result<T, OagwError>;

#[cfg(test)]
mod tests {
    use super::{ErrorKind, ErrorSource, OagwError};

    #[test]
    fn status_table_matches_the_prd() {
        assert_eq!(ErrorKind::Validation.status(), 400);
        assert_eq!(ErrorKind::AuthenticationFailed.status(), 401);
        assert_eq!(ErrorKind::RouteNotFound.status(), 404);
        assert_eq!(ErrorKind::PluginInUse.status(), 409);
        assert_eq!(ErrorKind::PayloadTooLarge.status(), 413);
        assert_eq!(ErrorKind::RateLimitExceeded.status(), 429);
        assert_eq!(ErrorKind::SecretNotFound.status(), 500);
        assert_eq!(ErrorKind::DownstreamError.status(), 502);
        assert_eq!(ErrorKind::CircuitBreakerOpen.status(), 503);
        assert_eq!(ErrorKind::RequestTimeout.status(), 504);
    }

    #[test]
    fn every_type_is_a_gts_error_identifier() {
        for kind in [
            ErrorKind::Validation,
            ErrorKind::MissingTargetHost,
            ErrorKind::RouteNotFound,
            ErrorKind::RateLimitExceeded,
            ErrorKind::IdleTimeout,
        ] {
            assert!(
                kind.gts_type().starts_with("gts.cf.core.errors.err.v1~cf.oagw."),
                "{}",
                kind.gts_type()
            );
        }
    }

    #[test]
    fn error_source_renders_the_header_value() {
        assert_eq!(ErrorSource::Gateway.as_str(), "gateway");
        assert_eq!(ErrorSource::Upstream.as_str(), "upstream");
    }

    #[test]
    fn extensions_are_preserved() {
        let err = OagwError::validation("bad").with_ext("alias", "vendor.com");
        assert_eq!(
            err.extensions.get("alias").and_then(serde_json::Value::as_str),
            Some("vendor.com")
        );
    }
}
