//! The gateway error table.
//!
//! One enum, one `(status, GTS type, title, retriable)` row per error, so the
//! table in `docs/DESIGN.md` §3.3 is checkable in a single place.

use thiserror::Error;

use crate::domain::gts_helpers::error_id;

/// Every error the gateway can generate.
#[derive(Debug, Error)]
pub enum DomainError {
    /// Configuration or request validation failed.
    #[error("{0}")]
    Validation(String),
    /// A required `X-OAGW-Target-Host` header was absent.
    #[error("a target host header is required for this upstream")]
    MissingTargetHost,
    /// `X-OAGW-Target-Host` was not a bare hostname.
    #[error("invalid target host: {0}")]
    InvalidTargetHost(String),
    /// `X-OAGW-Target-Host` named no configured endpoint.
    #[error("unknown target host: {0}")]
    UnknownTargetHost(String),
    /// Credentials could not be resolved or the auth plugin is unknown.
    #[error("authentication failed: {0}")]
    AuthenticationFailed(String),
    /// No route matched the request.
    #[error("{0}")]
    RouteNotFound(String),
    /// A referenced plugin would be invalidated by the operation.
    #[error("{0}")]
    PluginInUse(String),
    /// The request body exceeded the configured limit.
    #[error("{0}")]
    PayloadTooLarge(String),
    /// The caller exhausted its token budget.
    #[error("{0}")]
    RateLimitExceeded(String),
    /// A secret reference could not be resolved.
    #[error("{0}")]
    SecretNotFound(String),
    /// The upstream returned a transport-level failure.
    #[error("{0}")]
    DownstreamError(String),
    /// The circuit breaker for this upstream is open.
    #[error("circuit breaker open")]
    CircuitBreakerOpen,
    /// No upstream endpoint could be reached.
    #[error("upstream link unavailable")]
    LinkUnavailable,
    /// The upstream did not answer within the request budget.
    #[error("upstream request timed out")]
    RequestTimeout,
    /// Establishing the upstream connection timed out.
    #[error("upstream connection timed out")]
    ConnectionTimeout,
    /// A resource already exists with the same key.
    #[error("{0}")]
    Conflict(String),
    /// A named resource does not exist for this tenant.
    #[error("{0}")]
    NotFound(String),
    /// A cross-origin request used an origin the policy does not allow.
    #[error("{0}")]
    CorsOriginNotAllowed(String),
    /// A cross-origin request used a method the policy does not allow.
    #[error("{0}")]
    CorsMethodNotAllowed(String),
    /// The upstream answered 101 Switching Protocols but no upgrade followed.
    #[error("{0}")]
    UpgradeFailed(String),
}

impl DomainError {
    /// The error-table row for this variant.
    #[must_use]
    pub fn descriptor(&self) -> ErrorDescriptor {
        match self {
            Self::Validation(_) => row(400, "validation", "error", "Validation Error", false),
            Self::MissingTargetHost => row(
                400,
                "routing",
                "missing_target_host",
                "Missing Target Host",
                false,
            ),
            Self::InvalidTargetHost(_) => row(
                400,
                "routing",
                "invalid_target_host",
                "Invalid Target Host",
                false,
            ),
            Self::UnknownTargetHost(_) => row(
                400,
                "routing",
                "unknown_target_host",
                "Unknown Target Host",
                false,
            ),
            Self::AuthenticationFailed(_) => row(401, "auth", "failed", "Authentication Failed", false),
            Self::RouteNotFound(_) => row(404, "route", "not_found", "Route Not Found", false),
            Self::PluginInUse(_) => row(409, "plugin", "in_use", "Plugin In Use", false),
            Self::PayloadTooLarge(_) => row(413, "payload", "too_large", "Payload Too Large", false),
            Self::RateLimitExceeded(_) => row(
                429,
                "rate_limit",
                "exceeded",
                "Rate Limit Exceeded",
                true,
            ),
            Self::SecretNotFound(_) => row(500, "secret", "not_found", "Secret Not Found", false),
            Self::DownstreamError(text) => row(
                502,
                "downstream",
                "error",
                "Downstream Error",
                downstream_is_retriable(text),
            ),
            Self::CircuitBreakerOpen => row(
                503,
                "circuit_breaker",
                "open",
                "Circuit Breaker Open",
                true,
            ),
            Self::LinkUnavailable => row(503, "link", "unavailable", "Link Unavailable", true),
            Self::RequestTimeout => row(504, "timeout", "request", "Request Timeout", true),
            Self::ConnectionTimeout => row(504, "timeout", "connection", "Connection Timeout", true),
            Self::Conflict(_) => row(409, "validation", "conflict", "Conflict", false),
            Self::NotFound(_) => row(404, "route", "not_found", "Not Found", false),
            Self::CorsOriginNotAllowed(_) => row(
                403,
                "cors",
                "origin_not_allowed",
                "Origin Not Allowed",
                false,
            ),
            Self::CorsMethodNotAllowed(_) => row(
                403,
                "cors",
                "method_not_allowed",
                "Method Not Allowed",
                false,
            ),
            Self::UpgradeFailed(_) => row(502, "downstream", "error", "Upgrade Failed", true),
        }
    }

    /// HTTP status code for this error.
    #[must_use]
    pub fn status(&self) -> u16 {
        self.descriptor().status
    }

    /// GTS error type identifier.
    #[must_use]
    pub fn gts_type(&self) -> String {
        self.descriptor().gts_type
    }

    /// Whether retrying the same request may succeed.
    #[must_use]
    pub fn retriable(&self) -> bool {
        self.descriptor().retriable
    }
}

/// Whether a downstream failure looks like one a retry could clear.
///
/// The error table marks `DownstreamError` "Depends": an upstream that dropped
/// the connection or never answered may well answer next time, while a request
/// the gateway could not even build will fail identically forever. The message
/// is the only evidence available at this point, so it is what decides.
fn downstream_is_retriable(detail: &str) -> bool {
    const TRANSIENT: [&str; 12] = [
        "connection reset",
        "broken pipe",
        "connection closed",
        "incomplete message",
        "timed out",
        "deadline",
        "refused",
        "unreachable",
        "dns",
        "handshake",
        "tls",
        "io error",
    ];
    let lowered = detail.to_ascii_lowercase();
    TRANSIENT.iter().any(|marker| lowered.contains(marker))
}

/// The classification of one `DomainError` variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorDescriptor {
    /// HTTP status code.
    pub status: u16,
    /// GTS error type identifier.
    pub gts_type: String,
    /// Human readable title.
    pub title: String,
    /// Whether a retry may succeed.
    pub retriable: bool,
}

fn row(
    status: u16,
    family: &'static str,
    name: &'static str,
    title: &'static str,
    retriable: bool,
) -> ErrorDescriptor {
    ErrorDescriptor {
        status,
        gts_type: error_id(family, name),
        title: title.to_owned(),
        retriable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_row(error: &DomainError, status: u16, gts_type: &str, retriable: bool) {
        assert_eq!(error.status(), status, "{error}");
        assert_eq!(error.gts_type(), gts_type, "{error}");
        assert_eq!(error.retriable(), retriable, "{error}");
    }

    #[test]
    fn error_table_matches_design() {
        assert_row(
            &DomainError::Validation("x".into()),
            400,
            "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
            false,
        );
        assert_row(
            &DomainError::MissingTargetHost,
            400,
            "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1",
            false,
        );
        assert_row(
            &DomainError::InvalidTargetHost("x".into()),
            400,
            "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1",
            false,
        );
        assert_row(
            &DomainError::UnknownTargetHost("x".into()),
            400,
            "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1",
            false,
        );
        assert_row(
            &DomainError::AuthenticationFailed("x".into()),
            401,
            "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1",
            false,
        );
        assert_row(
            &DomainError::RouteNotFound("x".into()),
            404,
            "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1",
            false,
        );
        assert_row(
            &DomainError::PluginInUse("x".into()),
            409,
            "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1",
            false,
        );
        assert_row(
            &DomainError::PayloadTooLarge("x".into()),
            413,
            "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1",
            false,
        );
        assert_row(
            &DomainError::RateLimitExceeded("x".into()),
            429,
            "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1",
            true,
        );
        assert_row(
            &DomainError::SecretNotFound("x".into()),
            500,
            "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1",
            false,
        );
        assert_row(
            &DomainError::DownstreamError("x".into()),
            502,
            "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1",
            false,
        );
        assert!(
            DomainError::DownstreamError("connection closed before message completed".into()).retriable(),
            "a dropped connection may succeed on a retry"
        );
        assert!(
            !DomainError::DownstreamError("cannot build the upstream request: x".into())
                .retriable(),
            "a request the gateway could not build fails identically next time"
        );
        assert_row(
            &DomainError::CircuitBreakerOpen,
            503,
            "gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1",
            true,
        );
        assert_row(
            &DomainError::LinkUnavailable,
            503,
            "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1",
            true,
        );
        assert_row(
            &DomainError::RequestTimeout,
            504,
            "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1",
            true,
        );
        assert_row(
            &DomainError::ConnectionTimeout,
            504,
            "gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1",
            true,
        );
    }

    #[test]
    fn cors_rows_use_the_cors_family() {
        assert_row(
            &DomainError::CorsOriginNotAllowed("origin".into()),
            403,
            "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1",
            false,
        );
        assert_row(
            &DomainError::CorsMethodNotAllowed("POST".into()),
            403,
            "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1",
            false,
        );
    }
}
