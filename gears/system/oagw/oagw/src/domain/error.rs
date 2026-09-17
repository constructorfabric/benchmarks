//! Domain error model for the `oagw` gear.
//!
//! Every gateway error is a [`DomainError`] carrying an [`ErrorKind`] (one row
//! of the oagw error table in `DESIGN.md` §3.3), a human readable `detail` and
//! the extension fields an RFC 9457 problem body exposes at the top level
//! (`upstream_id`, `host`, `path`, `valid_hosts`, ...). Response headers that
//! belong to the error itself (`Retry-After`, `X-RateLimit-*`) travel in
//! [`DomainError::headers`].

use std::fmt;

use serde_json::Value;
use toolkit_canonical_errors::{CanonicalError, resource_error};

/// Categorises a gateway error. One variant per row of the error table in
/// `DESIGN.md` §3.3 plus the CORS codes of ADR-0004 and the generic
/// resource/not-found kinds used by the management API.
#[allow(clippy::enum_variant_names)] // the documented names share suffixes on purpose
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    /// Request or resource validation failed (400).
    ValidationError,
    /// `X-OAGW-Target-Host` required but absent (400).
    MissingTargetHost,
    /// `X-OAGW-Target-Host` is not a hostname/IP (400).
    InvalidTargetHost,
    /// `X-OAGW-Target-Host` matches no configured endpoint (400).
    UnknownTargetHost,
    /// Authentication to the upstream failed (401).
    AuthenticationFailed,
    /// No route matched the request (404).
    RouteNotFound,
    /// The referenced upstream does not exist (404).
    UpstreamNotFound,
    /// The tenant resolver does not know the tenant (404).
    TenantNotFound,
    /// The referenced resource does not exist (404).
    ResourceNotFound,
    /// Uniqueness or state conflict (409).
    Conflict,
    /// Plugin is still referenced by an upstream or route (409).
    PluginInUse,
    /// Request payload exceeds the configured limit (413).
    PayloadTooLarge,
    /// Rate limit exhausted (429).
    RateLimitExceeded,
    /// Referenced credential does not exist (500).
    SecretNotFound,
    /// Unexpected internal failure (500).
    Internal,
    /// Upstream spoke a protocol we do not speak (502).
    ProtocolError,
    /// Upstream returned an unusable response (502).
    DownstreamError,
    /// A stream was aborted mid-flight (502).
    StreamAborted,
    /// Upstream link unavailable (503).
    LinkUnavailable,
    /// Circuit breaker is open (503).
    CircuitBreakerOpen,
    /// Referenced plugin cannot be resolved (503).
    PluginNotFound,
    /// Connecting to the upstream timed out (504).
    ConnectionTimeout,
    /// The upstream request timed out (504).
    RequestTimeout,
    /// An idle stream timed out (504).
    IdleTimeout,
    /// Origin is not allowed by the CORS configuration (403).
    CorsOriginNotAllowed,
    /// Method is not allowed by the CORS configuration (403).
    CorsMethodNotAllowed,
}

impl ErrorKind {
    /// HTTP status this error kind is rendered with.
    #[must_use]
    pub const fn status(self) -> u16 {
        match self {
            Self::ValidationError
            | Self::MissingTargetHost
            | Self::InvalidTargetHost
            | Self::UnknownTargetHost => 400,
            Self::AuthenticationFailed => 401,
            Self::CorsOriginNotAllowed | Self::CorsMethodNotAllowed => 403,
            Self::RouteNotFound
            | Self::UpstreamNotFound
            | Self::ResourceNotFound
            | Self::TenantNotFound => 404,
            Self::Conflict | Self::PluginInUse => 409,
            Self::PayloadTooLarge => 413,
            Self::RateLimitExceeded => 429,
            Self::SecretNotFound | Self::Internal => 500,
            Self::ProtocolError | Self::DownstreamError | Self::StreamAborted => 502,
            Self::LinkUnavailable | Self::CircuitBreakerOpen | Self::PluginNotFound => 503,
            Self::ConnectionTimeout | Self::RequestTimeout | Self::IdleTimeout => 504,
        }
    }

    /// GTS instance part of the error type id, e.g. `cf.oagw.auth.failed.v1`.
    #[must_use]
    pub const fn gts_instance(self) -> &'static str {
        match self {
            Self::ValidationError => "cf.oagw.validation.error.v1",
            Self::MissingTargetHost => "cf.oagw.routing.missing_target_host.v1",
            Self::InvalidTargetHost => "cf.oagw.routing.invalid_target_host.v1",
            Self::UnknownTargetHost => "cf.oagw.routing.unknown_target_host.v1",
            Self::AuthenticationFailed => "cf.oagw.auth.failed.v1",
            Self::RouteNotFound => "cf.oagw.route.not_found.v1",
            Self::UpstreamNotFound => "cf.oagw.upstream.not_found.v1",
            Self::TenantNotFound => "cf.oagw.tenant.not_found.v1",
            Self::ResourceNotFound => "cf.oagw.resource.not_found.v1",
            Self::Conflict => "cf.oagw.conflict.v1",
            Self::PluginInUse => "cf.oagw.plugin.in_use.v1",
            Self::PayloadTooLarge => "cf.oagw.payload.too_large.v1",
            Self::RateLimitExceeded => "cf.oagw.rate_limit.exceeded.v1",
            Self::SecretNotFound => "cf.oagw.secret.not_found.v1",
            Self::Internal => "cf.oagw.internal.error.v1",
            Self::ProtocolError => "cf.oagw.protocol.error.v1",
            Self::DownstreamError => "cf.oagw.downstream.error.v1",
            Self::StreamAborted => "cf.oagw.stream.aborted.v1",
            Self::LinkUnavailable => "cf.oagw.link.unavailable.v1",
            Self::CircuitBreakerOpen => "cf.oagw.circuit_breaker.open.v1",
            Self::PluginNotFound => "cf.oagw.plugin.not_found.v1",
            Self::ConnectionTimeout => "cf.oagw.timeout.connection.v1",
            Self::RequestTimeout => "cf.oagw.timeout.request.v1",
            Self::IdleTimeout => "cf.oagw.timeout.idle.v1",
            Self::CorsOriginNotAllowed => "cf.oagw.cors.origin_not_allowed.v1",
            Self::CorsMethodNotAllowed => "cf.oagw.cors.method_not_allowed.v1",
        }
    }

    /// Full GTS `type` identifier of the RFC 9457 problem body.
    #[must_use]
    pub fn gts_type(self) -> String {
        format!("gts.cf.core.errors.err.v1~{}", self.gts_instance())
    }

    /// Short human readable title for the problem body.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::ValidationError
            | Self::MissingTargetHost
            | Self::InvalidTargetHost
            | Self::UnknownTargetHost => "Bad Request",
            Self::AuthenticationFailed => "Authentication Failed",
            Self::CorsOriginNotAllowed | Self::CorsMethodNotAllowed => "Forbidden",
            Self::RouteNotFound
            | Self::UpstreamNotFound
            | Self::ResourceNotFound
            | Self::TenantNotFound => "Not Found",
            Self::Conflict | Self::PluginInUse => "Conflict",
            Self::PayloadTooLarge => "Payload Too Large",
            Self::RateLimitExceeded => "Rate Limit Exceeded",
            Self::SecretNotFound => "Secret Not Found",
            Self::Internal => "Internal Server Error",
            Self::ProtocolError | Self::DownstreamError | Self::StreamAborted => "Bad Gateway",
            Self::LinkUnavailable | Self::CircuitBreakerOpen | Self::PluginNotFound => {
                "Service Unavailable"
            }
            Self::ConnectionTimeout | Self::RequestTimeout | Self::IdleTimeout => "Gateway Timeout",
        }
    }

    /// Whether a client may retry the request as-is (`Retriable` column of the
    /// error table).
    #[must_use]
    pub const fn retriable(self) -> bool {
        matches!(
            self,
            Self::RateLimitExceeded
                | Self::LinkUnavailable
                | Self::CircuitBreakerOpen
                | Self::ConnectionTimeout
                | Self::RequestTimeout
                | Self::IdleTimeout
        )
    }
}

/// A gateway error with its kind, detail and RFC 9457 extension fields.
#[derive(Debug, Clone)]
pub struct DomainError {
    /// The error category.
    pub kind: ErrorKind,
    /// Human readable explanation of this occurrence.
    pub detail: String,
    /// Extension fields rendered at the top level of the problem body.
    pub fields: Vec<(&'static str, Value)>,
    /// Extra response headers (`Retry-After`, `X-RateLimit-*`, ...).
    pub headers: Vec<(String, String)>,
}

impl DomainError {
    /// Creates a new error of `kind` with the given detail.
    #[must_use]
    pub fn new(kind: ErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
            fields: Vec::new(),
            headers: Vec::new(),
        }
    }

    /// Attaches an extension field to the problem body.
    #[must_use]
    pub fn with_field(mut self, name: &'static str, value: Value) -> Self {
        self.fields.push((name, value));
        self
    }

    /// Attaches an extension field only when `value` is `Some`.
    #[must_use]
    pub fn with_optional_field(mut self, name: &'static str, value: Option<Value>) -> Self {
        if let Some(v) = value {
            self.fields.push((name, v));
        }
        self
    }

    /// Adds a response header to the error response.
    #[must_use]
    pub fn with_header(mut self, name: String, value: String) -> Self {
        self.headers.push((name, value));
        self
    }

    /// Looks up an attached extension field.
    #[must_use]
    pub fn field(&self, name: &str) -> Option<&Value> {
        self.fields
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value)
    }

    /// HTTP status code of this error.
    #[must_use]
    pub const fn status(&self) -> u16 {
        self.kind.status()
    }
}

impl fmt::Display for DomainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind.gts_instance(), self.detail)
    }
}

impl std::error::Error for DomainError {}

/// Resource type used for the canonical (`CanonicalError`) projection of
/// oagw errors. Platform interop only — the wire contract of the gear's own
/// REST surface is the oagw problem body rendered by `crate::api::rest::error`.
#[resource_error(gts_id!("cf.oagw.oagw.upstream.v1~"))]
pub struct OagwResourceError;

impl From<DomainError> for CanonicalError {
    fn from(err: DomainError) -> Self {
        let detail = err.detail.clone();
        // Nearest canonical category per status class. `413` and `502` have no
        // canonical category of their own; the oagw statuses are preserved on
        // the wire by `OagwError`, this projection is platform interop only.
        match err.kind.status() {
            401 => CanonicalError::unauthenticated()
                .with_reason(detail)
                .create(),
            403 => OagwResourceError::permission_denied()
                .with_reason(detail)
                .create(),
            404 => OagwResourceError::not_found(detail)
                .with_resource(err.kind.gts_instance())
                .create(),
            409 => OagwResourceError::already_exists(detail)
                .with_resource(err.kind.gts_instance())
                .create(),
            413 => OagwResourceError::out_of_range(detail.clone())
                .with_field_violation("body", detail, "payload_too_large")
                .create(),
            429 => OagwResourceError::resource_exhausted(detail.clone())
                .with_quota_violation("requests", detail)
                .create(),
            502 => OagwResourceError::unknown(detail).create(),
            503 => CanonicalError::service_unavailable()
                .with_detail(detail)
                .create(),
            504 => OagwResourceError::deadline_exceeded(detail).create(),
            _ => OagwResourceError::invalid_argument()
                .with_field_violation("request", detail.clone(), "invalid_request")
                .create(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn documented_error_kinds_render_documented_status_and_type() {
        let cases = [
            (
                ErrorKind::ValidationError,
                400_u16,
                "cf.oagw.validation.error.v1",
            ),
            (
                ErrorKind::MissingTargetHost,
                400,
                "cf.oagw.routing.missing_target_host.v1",
            ),
            (
                ErrorKind::InvalidTargetHost,
                400,
                "cf.oagw.routing.invalid_target_host.v1",
            ),
            (
                ErrorKind::UnknownTargetHost,
                400,
                "cf.oagw.routing.unknown_target_host.v1",
            ),
            (
                ErrorKind::AuthenticationFailed,
                401,
                "cf.oagw.auth.failed.v1",
            ),
            (ErrorKind::RouteNotFound, 404, "cf.oagw.route.not_found.v1"),
            (ErrorKind::PluginInUse, 409, "cf.oagw.plugin.in_use.v1"),
            (
                ErrorKind::PayloadTooLarge,
                413,
                "cf.oagw.payload.too_large.v1",
            ),
            (
                ErrorKind::RateLimitExceeded,
                429,
                "cf.oagw.rate_limit.exceeded.v1",
            ),
            (
                ErrorKind::SecretNotFound,
                500,
                "cf.oagw.secret.not_found.v1",
            ),
            (ErrorKind::ProtocolError, 502, "cf.oagw.protocol.error.v1"),
            (
                ErrorKind::DownstreamError,
                502,
                "cf.oagw.downstream.error.v1",
            ),
            (ErrorKind::StreamAborted, 502, "cf.oagw.stream.aborted.v1"),
            (
                ErrorKind::LinkUnavailable,
                503,
                "cf.oagw.link.unavailable.v1",
            ),
            (
                ErrorKind::CircuitBreakerOpen,
                503,
                "cf.oagw.circuit_breaker.open.v1",
            ),
            (
                ErrorKind::PluginNotFound,
                503,
                "cf.oagw.plugin.not_found.v1",
            ),
            (
                ErrorKind::ConnectionTimeout,
                504,
                "cf.oagw.timeout.connection.v1",
            ),
            (ErrorKind::RequestTimeout, 504, "cf.oagw.timeout.request.v1"),
            (ErrorKind::IdleTimeout, 504, "cf.oagw.timeout.idle.v1"),
            (
                ErrorKind::CorsOriginNotAllowed,
                403,
                "cf.oagw.cors.origin_not_allowed.v1",
            ),
            (
                ErrorKind::CorsMethodNotAllowed,
                403,
                "cf.oagw.cors.method_not_allowed.v1",
            ),
        ];
        for (kind, status, instance) in cases {
            assert_eq!(kind.status(), status, "{instance} status");
            assert_eq!(kind.gts_instance(), instance, "{instance} gts instance");
            assert_eq!(
                kind.gts_type(),
                format!("gts.cf.core.errors.err.v1~{instance}"),
                "{instance} full gts type"
            );
        }
    }

    #[test]
    fn retriable_column_matches_the_table() {
        for kind in [
            ErrorKind::RateLimitExceeded,
            ErrorKind::LinkUnavailable,
            ErrorKind::CircuitBreakerOpen,
            ErrorKind::ConnectionTimeout,
            ErrorKind::RequestTimeout,
            ErrorKind::IdleTimeout,
        ] {
            assert!(kind.retriable(), "{kind:?} must be retriable");
        }
        assert!(!ErrorKind::ValidationError.retriable());
        assert!(!ErrorKind::DownstreamError.retriable());
    }

    #[test]
    fn fields_are_attached_and_read_back() {
        let err = DomainError::new(ErrorKind::UnknownTargetHost, "no such endpoint")
            .with_field("valid_hosts", json!(["api.example.com"]))
            .with_optional_field("host", Some(json!("nope.example.com")))
            .with_optional_field("upstream_id", None);
        assert_eq!(err.field("valid_hosts"), Some(&json!(["api.example.com"])));
        assert_eq!(err.field("host"), Some(&json!("nope.example.com")));
        assert!(err.field("upstream_id").is_none());
        assert_eq!(err.status(), 400);
        assert_eq!(
            err.to_string(),
            "cf.oagw.routing.unknown_target_host.v1: no such endpoint"
        );
    }

    #[test]
    fn headers_round_trip() {
        let err = DomainError::new(ErrorKind::RateLimitExceeded, "too many requests")
            .with_header("Retry-After".to_owned(), "7".to_owned());
        assert_eq!(err.headers.len(), 1);
        assert_eq!(err.headers[0].0, "Retry-After");
        assert_eq!(err.headers[0].1, "7");
    }
}
