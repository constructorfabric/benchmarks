//! Domain error type for the OAGW control and data planes.
//!
//! Every variant carries the GTS error instance id from `DESIGN.md` §3.3 so
//! the REST layer can emit a `problem+json` body whose `type` is stable and
//! client-branchable.

use crate::domain::gts_helpers::errors as err_ids;

/// Response header that tells a client whether OAGW or the upstream produced
/// the response it is looking at (ADR-0007).
pub const ERROR_SOURCE_HEADER: &str = "x-oagw-error-source";
/// Value of [`ERROR_SOURCE_HEADER`] when OAGW itself generated the response.
pub const SOURCE_GATEWAY: &str = "gateway";
/// Value of [`ERROR_SOURCE_HEADER`] when the response came from the upstream.
pub const SOURCE_UPSTREAM: &str = "upstream";

/// Everything that can go wrong inside OAGW.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum DomainError {
    /// Request payload failed structural validation (400).
    #[error("validation failed: {0}")]
    Validation(String),
    /// A configured rule is internally inconsistent (400).
    #[error("invalid configuration: {0}")]
    InvalidConfiguration(String),
    /// The upstream has no usable endpoint host (400).
    #[error("upstream target host is missing")]
    MissingTargetHost,
    /// The upstream host is not a syntactically valid hostname (400).
    #[error("upstream target host is invalid: {0}")]
    InvalidTargetHost(String),
    /// The upstream host could not be resolved (400).
    #[error("upstream target host could not be resolved: {0}")]
    UnknownTargetHost(String),
    /// The referenced upstream does not exist (404).
    #[error("upstream not found")]
    UpstreamNotFound,
    /// The referenced route does not exist (404).
    #[error("route not found")]
    RouteNotFound,
    /// The referenced plugin does not exist (404).
    #[error("plugin not found")]
    PluginNotFound,
    /// The referenced resource is owned by another tenant (404, never 403).
    #[error("resource not found in this tenant")]
    TenantMismatch,
    /// The alias is already taken by another upstream (409).
    #[error("alias '{0}' is already in use")]
    AliasConflict(String),
    /// The route's match rules collide with an existing route (409).
    #[error("route conflicts with an existing route")]
    RouteConflict,
    /// The plugin is still referenced by an upstream or route (409).
    #[error("plugin is still referenced")]
    PluginInUse,
    /// The entity is referenced by routes and cannot be deleted (409).
    #[error("upstream is referenced by routes")]
    UpstreamInUse(usize),
    /// The request body exceeds the configured ceiling (413).
    #[error("request body too large")]
    PayloadTooLarge,
    /// Rate limit exhausted (429).
    #[error("rate limit exceeded")]
    RateLimitExceeded {
        /// Seconds until the bucket refills enough to admit a request.
        retry_after_seconds: u64,
        /// Configured sustained limit.
        limit: u32,
        /// Tokens remaining in the bucket.
        remaining: u32,
        /// Epoch seconds at which the bucket is fully replenished.
        reset_epoch_seconds: u64,
    },
    /// CORS preflight or actual cross-origin request rejected (403).
    #[error("CORS request rejected: {0}")]
    CorsRejected(&'static str),
    /// A required header guard plugin rejected the request (400).
    #[error("required header missing: {0}")]
    RequiredHeaderMissing(String),
    /// A required response header guard plugin rejected the upstream's
    /// response (502, ADR-0009 response phase).
    #[error("required response header missing: {0}")]
    ResponseHeaderMissing(String),
    /// The upstream requires credentials that are not configured (500).
    #[error("credential reference could not be resolved")]
    SecretNotFound,
    /// The client credential was rejected by the upstream (401).
    #[error("upstream rejected the client credentials")]
    AuthFailed(String),
    /// The configured auth plugin is unknown to this build (503).
    #[error("auth plugin is not available: {0}")]
    AuthPluginUnavailable(String),
    /// The upstream returned an unusable response (502).
    #[error("protocol error: {0}")]
    ProtocolError(String),
    /// The upstream connection failed before a response arrived (502).
    #[error("downstream error: {0}")]
    DownstreamError(String),
    /// A streaming response was aborted mid-flight (502).
    #[error("stream aborted: {0}")]
    StreamAborted(String),
    /// No connection could be established (503).
    #[error("link unavailable: {0}")]
    LinkUnavailable(String),
    /// The circuit breaker for this upstream is open (503).
    #[error("circuit breaker open for '{0}'")]
    CircuitBreakerOpen(String),
    /// Connecting to the upstream timed out (504).
    #[error("connection timed out")]
    ConnectionTimeout,
    /// The upstream did not produce headers in time (504).
    #[error("request timed out")]
    RequestTimeout,
    /// The upstream stalled mid-response (504).
    #[error("idle timeout")]
    IdleTimeout,
    /// The endpoint scheme is not permitted by policy (400).
    #[error("{0}")]
    SchemeNotAllowed(String),
}

impl DomainError {
    /// HTTP status code for this error.
    #[must_use]
    pub fn status(&self) -> http::StatusCode {
        use http::StatusCode;
        match self {
            Self::Validation(_)
            | Self::InvalidConfiguration(_)
            | Self::MissingTargetHost
            | Self::InvalidTargetHost(_)
            | Self::UnknownTargetHost(_)
            | Self::RequiredHeaderMissing(_)
            | Self::SchemeNotAllowed(_) => StatusCode::BAD_REQUEST,
            Self::AuthFailed(_) => StatusCode::UNAUTHORIZED,
            Self::UpstreamNotFound
            | Self::RouteNotFound
            | Self::PluginNotFound
            | Self::TenantMismatch => StatusCode::NOT_FOUND,
            Self::AliasConflict(_)
            | Self::RouteConflict
            | Self::PluginInUse
            | Self::UpstreamInUse(_) => StatusCode::CONFLICT,
            Self::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::RateLimitExceeded { .. } => StatusCode::TOO_MANY_REQUESTS,
            Self::CorsRejected(_) => StatusCode::FORBIDDEN,
            Self::SecretNotFound => StatusCode::INTERNAL_SERVER_ERROR,
            Self::ProtocolError(_)
            | Self::DownstreamError(_)
            | Self::StreamAborted(_)
            | Self::ResponseHeaderMissing(_) => StatusCode::BAD_GATEWAY,
            Self::AuthPluginUnavailable(_)
            | Self::LinkUnavailable(_)
            | Self::CircuitBreakerOpen(_) => StatusCode::SERVICE_UNAVAILABLE,
            Self::ConnectionTimeout | Self::RequestTimeout | Self::IdleTimeout => {
                StatusCode::GATEWAY_TIMEOUT
            }
        }
    }

    /// The GTS error instance id (DESIGN §3.3 error table).
    #[must_use]
    pub fn gts_type(&self) -> &'static str {
        match self {
            Self::Validation(_) | Self::InvalidConfiguration(_) => err_ids::VALIDATION,
            Self::MissingTargetHost => err_ids::ROUTING_MISSING_TARGET_HOST,
            Self::InvalidTargetHost(_) => err_ids::ROUTING_INVALID_TARGET_HOST,
            Self::UnknownTargetHost(_) => err_ids::ROUTING_UNKNOWN_TARGET_HOST,
            Self::AuthFailed(_) => err_ids::AUTH_FAILED,
            Self::UpstreamNotFound => err_ids::UPSTREAM_CONFLICT,
            Self::RouteNotFound => err_ids::ROUTE_NOT_FOUND,
            Self::PluginNotFound => err_ids::PLUGIN_NOT_FOUND,
            Self::TenantMismatch => err_ids::ROUTE_NOT_FOUND,
            Self::AliasConflict(_) => err_ids::UPSTREAM_CONFLICT,
            Self::RouteConflict => err_ids::ROUTE_CONFLICT,
            Self::PluginInUse | Self::UpstreamInUse(_) => err_ids::PLUGIN_IN_USE,
            Self::PayloadTooLarge => err_ids::PAYLOAD_TOO_LARGE,
            Self::RateLimitExceeded { .. } => err_ids::RATE_LIMIT_EXCEEDED,
            Self::CorsRejected("origin not allowed") => err_ids::CORS_ORIGIN_NOT_ALLOWED,
            Self::CorsRejected(_) => err_ids::CORS_METHOD_NOT_ALLOWED,
            Self::RequiredHeaderMissing(_) | Self::ResponseHeaderMissing(_) => {
                err_ids::REQUIRED_HEADER_MISSING
            }
            Self::SecretNotFound => err_ids::SECRET_NOT_FOUND,
            Self::AuthPluginUnavailable(_) => err_ids::PLUGIN_NOT_FOUND,
            Self::ProtocolError(_) => err_ids::PROTOCOL_ERROR,
            Self::DownstreamError(_) => err_ids::DOWNSTREAM_ERROR,
            Self::StreamAborted(_) => err_ids::STREAM_ABORTED,
            Self::LinkUnavailable(_) => err_ids::LINK_UNAVAILABLE,
            Self::CircuitBreakerOpen(_) => err_ids::CIRCUIT_BREAKER_OPEN,
            Self::ConnectionTimeout => err_ids::TIMEOUT_CONNECTION,
            Self::RequestTimeout => err_ids::TIMEOUT_REQUEST,
            Self::IdleTimeout => err_ids::TIMEOUT_IDLE,
            Self::SchemeNotAllowed(_) => err_ids::VALIDATION,
        }
    }

    /// Short human title used as the problem `title`.
    #[must_use]
    pub fn title(&self) -> &'static str {
        match self {
            Self::Validation(_) | Self::InvalidConfiguration(_) | Self::SchemeNotAllowed(_) => {
                "Validation failed"
            }
            Self::MissingTargetHost | Self::InvalidTargetHost(_) | Self::UnknownTargetHost(_) => {
                "Invalid target"
            }
            Self::AuthFailed(_) => "Authentication failed",
            Self::UpstreamNotFound => "Upstream not found",
            Self::RouteNotFound => "Route not found",
            Self::PluginNotFound => "Plugin not found",
            Self::TenantMismatch => "Not found",
            Self::AliasConflict(_) | Self::RouteConflict => "Conflict",
            Self::PluginInUse | Self::UpstreamInUse(_) => "Resource in use",
            Self::PayloadTooLarge => "Payload too large",
            Self::RateLimitExceeded { .. } => "Rate limit exceeded",
            Self::CorsRejected(_) => "CORS rejected",
            Self::RequiredHeaderMissing(_) => "Required header missing",
            Self::ResponseHeaderMissing(_) => "Required response header missing",
            Self::SecretNotFound => "Credential not found",
            Self::AuthPluginUnavailable(_) => "Plugin unavailable",
            Self::ProtocolError(_) => "Protocol error",
            Self::DownstreamError(_) => "Downstream error",
            Self::StreamAborted(_) => "Stream aborted",
            Self::LinkUnavailable(_) => "Link unavailable",
            Self::CircuitBreakerOpen(_) => "Circuit breaker open",
            Self::ConnectionTimeout | Self::RequestTimeout | Self::IdleTimeout => "Timeout",
        }
    }

    /// Machine-readable error code (the last GTS id label).
    #[must_use]
    pub fn code(&self) -> String {
        let id = self.gts_type();
        id.split('~')
            .next_back()
            .unwrap_or(id)
            .trim_end_matches(".v1")
            .to_owned()
    }

    /// Structured context that survives the gateway's problem middleware.
    #[must_use]
    pub fn context(&self) -> serde_json::Value {
        match self {
            Self::Validation(detail) | Self::InvalidConfiguration(detail) => {
                serde_json::json!({ "reason": detail })
            }
            Self::InvalidTargetHost(host) | Self::UnknownTargetHost(host) => {
                serde_json::json!({ "host": host })
            }
            Self::AliasConflict(alias) => serde_json::json!({ "alias": alias }),
            Self::AuthFailed(detail) => serde_json::json!({ "reason": detail }),
            Self::RequiredHeaderMissing(name) | Self::ResponseHeaderMissing(name) => {
                serde_json::json!({ "header": name })
            }
            Self::UpstreamInUse(routes) => serde_json::json!({ "referenced_by": routes }),
            Self::AuthPluginUnavailable(id) => serde_json::json!({ "plugin_id": id }),
            Self::ProtocolError(detail)
            | Self::DownstreamError(detail)
            | Self::StreamAborted(detail)
            | Self::LinkUnavailable(detail) => serde_json::json!({ "reason": detail }),
            Self::CircuitBreakerOpen(alias) => serde_json::json!({ "alias": alias }),
            Self::RateLimitExceeded {
                retry_after_seconds,
                limit,
                remaining,
                reset_epoch_seconds,
            } => serde_json::json!({
                "retry_after_seconds": retry_after_seconds,
                "limit": limit,
                "remaining": remaining,
                "reset_epoch_seconds": reset_epoch_seconds,
            }),
            _ => serde_json::Value::Null,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_follow_the_design_error_table() {
        assert_eq!(DomainError::Validation("x".into()).status(), 400);
        assert_eq!(DomainError::UpstreamNotFound.status(), 404);
        assert_eq!(DomainError::AliasConflict("a".into()).status(), 409);
        assert_eq!(DomainError::PayloadTooLarge.status(), 413);
        assert_eq!(
            DomainError::RateLimitExceeded {
                retry_after_seconds: 1,
                limit: 1,
                remaining: 0,
                reset_epoch_seconds: 0,
            }
            .status(),
            429
        );
        assert_eq!(DomainError::DownstreamError("x".into()).status(), 502);
        assert_eq!(DomainError::LinkUnavailable("x".into()).status(), 503);
        assert_eq!(DomainError::RequestTimeout.status(), 504);
    }

    #[test]
    fn gts_types_match_the_design_error_table() {
        assert_eq!(
            DomainError::RouteNotFound.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
        );
        assert_eq!(
            DomainError::PayloadTooLarge.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1"
        );
        assert_eq!(
            DomainError::CircuitBreakerOpen("x".into()).gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1"
        );
    }

    #[test]
    fn no_error_message_contains_a_secret() {
        let e = DomainError::AuthFailed("apikey header rejected".into());
        assert!(!e.to_string().contains("sk-"));
        assert_eq!(e.context()["reason"], "apikey header rejected");
    }
}
