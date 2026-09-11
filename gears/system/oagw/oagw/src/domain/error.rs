//! Domain error type and its mapping onto the gateway error catalogue.
//!
//! The catalogue is the error table of `DESIGN.md` section 3.3, which is the
//! authoritative superset of the shorter table in `PRD.md`.

use std::fmt;

/// Where an error response originated.
///
/// Emitted as the `X-OAGW-Error-Source` header on every response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorSource {
    /// The gateway itself produced the response.
    Gateway,
    /// The response was relayed from the upstream service.
    Upstream,
}

impl ErrorSource {
    /// Header value for this source.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gateway => "gateway",
            Self::Upstream => "upstream",
        }
    }
}

/// Name of the error-source header.
pub const ERROR_SOURCE_HEADER: &str = "x-oagw-error-source";

// @cpt-begin:cpt-cf-oagw-dod-gear-foundation-error-type-catalog:p1:inst-catalog
/// Gateway error kinds, one per row of the `DESIGN.md` section 3.3 table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// General route validation error.
    RouteError,
    /// Request validation failed.
    ValidationError,
    /// `X-OAGW-Target-Host` is required but absent.
    MissingTargetHost,
    /// `X-OAGW-Target-Host` is malformed.
    InvalidTargetHost,
    /// `X-OAGW-Target-Host` matches no configured endpoint.
    UnknownTargetHost,
    /// Authentication failed.
    AuthenticationFailed,
    /// No matching route was found.
    RouteNotFound,
    /// A plugin is still referenced and cannot be deleted.
    PluginInUse,
    /// An upstream alias collides with an existing one.
    UpstreamAliasConflict,
    /// A route match rule collides with an existing one.
    RouteMatchConflict,
    /// The request payload exceeds the limit.
    PayloadTooLarge,
    /// A rate limit was exceeded.
    RateLimitExceeded,
    /// A referenced secret could not be found.
    SecretNotFound,
    /// A protocol-level error occurred.
    ProtocolError,
    /// The upstream service returned an error the gateway raised itself.
    DownstreamError,
    /// A stream was aborted after it had begun.
    StreamAborted,
    /// The upstream link is unavailable.
    LinkUnavailable,
    /// The circuit breaker is open.
    CircuitBreakerOpen,
    /// A referenced plugin could not be resolved.
    PluginNotFound,
    /// Establishing the upstream connection timed out.
    ConnectionTimeout,
    /// The upstream request timed out.
    RequestTimeout,
    /// An idle stream timed out.
    IdleTimeout,
}

impl ErrorKind {
    /// HTTP status code for this error kind.
    #[must_use]
    pub const fn status(self) -> u16 {
        match self {
            Self::RouteError
            | Self::ValidationError
            | Self::MissingTargetHost
            | Self::InvalidTargetHost
            | Self::UnknownTargetHost => 400,
            Self::AuthenticationFailed => 401,
            Self::RouteNotFound => 404,
            Self::PluginInUse | Self::UpstreamAliasConflict | Self::RouteMatchConflict => 409,
            Self::PayloadTooLarge => 413,
            Self::RateLimitExceeded => 429,
            Self::SecretNotFound => 500,
            Self::ProtocolError | Self::DownstreamError | Self::StreamAborted => 502,
            Self::LinkUnavailable | Self::CircuitBreakerOpen | Self::PluginNotFound => 503,
            Self::ConnectionTimeout | Self::RequestTimeout | Self::IdleTimeout => 504,
        }
    }

    /// Global type system identifier for this error kind.
    #[must_use]
    pub const fn gts_type(self) -> &'static str {
        match self {
            Self::RouteError | Self::ValidationError => {
                "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
            }
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
            Self::RouteNotFound => "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1",
            Self::PluginInUse => "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1",
            Self::UpstreamAliasConflict => {
                "gts.cf.core.errors.err.v1~cf.oagw.upstream.alias_conflict.v1"
            }
            Self::RouteMatchConflict => "gts.cf.core.errors.err.v1~cf.oagw.route.match_conflict.v1",
            Self::PayloadTooLarge => "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1",
            Self::RateLimitExceeded => "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1",
            Self::SecretNotFound => "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1",
            Self::ProtocolError => "gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1",
            Self::DownstreamError => "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1",
            Self::StreamAborted => "gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1",
            Self::LinkUnavailable => "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1",
            Self::CircuitBreakerOpen => "gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1",
            Self::PluginNotFound => "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1",
            Self::ConnectionTimeout => "gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1",
            Self::RequestTimeout => "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1",
            Self::IdleTimeout => "gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1",
        }
    }

    /// Short human-readable title for this error kind.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::RouteError => "Route error",
            Self::ValidationError => "Validation error",
            Self::MissingTargetHost => "Missing target host",
            Self::InvalidTargetHost => "Invalid target host",
            Self::UnknownTargetHost => "Unknown target host",
            Self::AuthenticationFailed => "Authentication failed",
            Self::RouteNotFound => "Route not found",
            Self::PluginInUse => "Plugin in use",
            Self::UpstreamAliasConflict => "Upstream alias conflict",
            Self::RouteMatchConflict => "Route match conflict",
            Self::PayloadTooLarge => "Payload too large",
            Self::RateLimitExceeded => "Rate limit exceeded",
            Self::SecretNotFound => "Secret not found",
            Self::ProtocolError => "Protocol error",
            Self::DownstreamError => "Downstream error",
            Self::StreamAborted => "Stream aborted",
            Self::LinkUnavailable => "Link unavailable",
            Self::CircuitBreakerOpen => "Circuit breaker open",
            Self::PluginNotFound => "Plugin not found",
            Self::ConnectionTimeout => "Connection timeout",
            Self::RequestTimeout => "Request timeout",
            Self::IdleTimeout => "Idle timeout",
        }
    }
}
// @cpt-end:cpt-cf-oagw-dod-gear-foundation-error-type-catalog:p1:inst-catalog

/// An error raised by the gateway itself.
#[derive(Debug, Clone)]
pub struct DomainError {
    /// Which catalogue entry this error is.
    pub kind: ErrorKind,
    /// Occurrence-specific explanation.
    pub detail: String,
    /// Extra members carried in the problem document's `context`.
    pub context: serde_json::Value,
}

impl DomainError {
    /// Build an error of the given kind with a detail message.
    #[must_use]
    pub fn new(kind: ErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
            context: serde_json::Value::Null,
        }
    }

    /// Attach extension members to the problem document's `context`.
    #[must_use]
    pub fn with_context(mut self, context: serde_json::Value) -> Self {
        self.context = context;
        self
    }

    /// A `400` validation error.
    #[must_use]
    pub fn validation(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::ValidationError, detail)
    }

    /// A `404` not-found error.
    #[must_use]
    pub fn not_found(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::RouteNotFound, detail)
    }

    /// HTTP status code for this error.
    #[must_use]
    pub const fn status(&self) -> u16 {
        self.kind.status()
    }
}

impl fmt::Display for DomainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind.title(), self.detail)
    }
}

impl std::error::Error for DomainError {}

/// Result alias for domain operations.
pub type DomainResult<T> = Result<T, DomainError>;

#[cfg(test)]
mod tests {
    use super::{ErrorKind, ErrorSource};

    #[test]
    fn error_source_header_values() {
        assert_eq!(ErrorSource::Gateway.as_str(), "gateway");
        assert_eq!(ErrorSource::Upstream.as_str(), "upstream");
    }

    #[test]
    fn catalogue_statuses_match_the_design_table() {
        assert_eq!(ErrorKind::ValidationError.status(), 400);
        assert_eq!(ErrorKind::MissingTargetHost.status(), 400);
        assert_eq!(ErrorKind::AuthenticationFailed.status(), 401);
        assert_eq!(ErrorKind::RouteNotFound.status(), 404);
        assert_eq!(ErrorKind::PluginInUse.status(), 409);
        assert_eq!(ErrorKind::PayloadTooLarge.status(), 413);
        assert_eq!(ErrorKind::RateLimitExceeded.status(), 429);
        assert_eq!(ErrorKind::SecretNotFound.status(), 500);
        assert_eq!(ErrorKind::DownstreamError.status(), 502);
        assert_eq!(ErrorKind::StreamAborted.status(), 502);
        assert_eq!(ErrorKind::LinkUnavailable.status(), 503);
        assert_eq!(ErrorKind::PluginNotFound.status(), 503);
        assert_eq!(ErrorKind::RequestTimeout.status(), 504);
    }

    #[test]
    fn catalogue_types_are_gts_identifiers() {
        for kind in [
            ErrorKind::ValidationError,
            ErrorKind::RouteNotFound,
            ErrorKind::RateLimitExceeded,
            ErrorKind::StreamAborted,
        ] {
            assert!(
                kind.gts_type()
                    .starts_with("gts.cf.core.errors.err.v1~cf.oagw.")
            );
        }
    }
}
