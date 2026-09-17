//! OAGW domain error taxonomy.
//!
//! Mirrors the error table in `docs/DESIGN.md` §3.3 ("Error Response Format").
//! Every variant carries the GTS error-type identifier the data plane renders
//! into the `type` member of its RFC 9457 `problem+json` body, plus the HTTP
//! status the transport layer maps it to.

use thiserror::Error;

/// Prefix shared by every OAGW error type identifier.
///
/// The canonical error type id `gts.cf.core.errors.err.v1~` is the *type* part;
/// the suffix after `~` is the OAGW-specific instance part quoted verbatim in
/// the DESIGN error table.
pub const ERROR_TYPE_PREFIX: &str = "gts.cf.core.errors.err.v1~";

/// Builds an OAGW error GTS instance id at compile time.
macro_rules! oagw_error_type {
    ($suffix:literal) => {
        concat!("gts.cf.core.errors.err.v1~cf.oagw.", $suffix)
    };
}

/// OAGW domain error.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DomainError {
    // ---------------------------------------------------------------------------
    // 400 — validation / routing
    // ---------------------------------------------------------------------------
    /// General route validation error (`RouteError`).
    #[error("route validation failed: {0}")]
    Route(String),

    /// Request validation failed (`ValidationError`).
    #[error("request validation failed: {0}")]
    Validation(String),

    /// `X-OAGW-Target-Host` is required to disambiguate a multi-endpoint
    /// upstream whose alias is a common domain suffix.
    #[error("X-OAGW-Target-Host header is required for upstream '{0}'")]
    MissingTargetHost(String),

    /// `X-OAGW-Target-Host` is not a bare hostname or IP.
    #[error("X-OAGW-Target-Host value '{0}' is not a valid host")]
    InvalidTargetHost(String),

    /// `X-OAGW-Target-Host` does not match any configured endpoint.
    #[error("X-OAGW-Target-Host '{0}' does not match any endpoint of upstream '{1}'")]
    UnknownTargetHost(String, String),

    // ---------------------------------------------------------------------------
    // 401
    // ---------------------------------------------------------------------------
    /// Authentication to the upstream failed.
    #[error("authentication to upstream '{0}' failed")]
    AuthenticationFailed(String),

    // ---------------------------------------------------------------------------
    // 404
    // ---------------------------------------------------------------------------
    /// No route matched the request, or a referenced resource does not exist.
    #[error("{0}")]
    NotFound(String),

    /// No route matched the resolved upstream for this method/path.
    #[error("no route matches {method} {path} on upstream '{alias}'")]
    RouteNotFound { method: String, path: String, alias: String },

    // ---------------------------------------------------------------------------
    // 409 — conflicts
    // ---------------------------------------------------------------------------
    /// An upstream with this alias already exists for the tenant.
    #[error("an upstream with alias '{alias}' already exists (id {existing_id})")]
    AliasConflict { alias: String, existing_id: uuid::Uuid },

    /// An enabled route under the same upstream already matches this
    /// `(method, path prefix, priority)` triple.
    #[error("a route for upstream '{0}' already matches this path and method")]
    DuplicateRouteMatch(String),

    /// A field that is immutable once the resource is created was supplied.
    #[error("field '{field}' is immutable on {resource}")]
    ImmutableField { resource: &'static str, field: &'static str },

    /// Referenced plugin is still bound to an upstream or route.
    #[error("plugin '{0}' is still referenced")]
    PluginInUse(String),

    // ---------------------------------------------------------------------------
    // 413
    // ---------------------------------------------------------------------------
    /// Request payload exceeds the 100 MB hard limit.
    #[error("request payload exceeds the {0} byte limit")]
    PayloadTooLarge(usize),

    // ---------------------------------------------------------------------------
    // 422 — semantically invalid body
    // ---------------------------------------------------------------------------
    /// The body is well-formed but semantically invalid.
    #[error("semantic validation failed: {0}")]
    Semantic(String),

    // ---------------------------------------------------------------------------
    // 429
    // ---------------------------------------------------------------------------
    /// Rate limit exceeded for the resolved upstream/route.
    #[error("rate limit exceeded for upstream '{0}'")]
    RateLimitExceeded(String),

    // ---------------------------------------------------------------------------
    // 5xx — gateway / upstream failures
    // ---------------------------------------------------------------------------
    /// A referenced `cred://` secret could not be resolved.
    #[error("secret reference '{0}' could not be resolved")]
    SecretNotFound(String),

    /// Protocol-level error talking to the upstream.
    #[error("protocol error from upstream '{0}': {1}")]
    ProtocolError(String, String),

    /// The upstream answered with an error status (passthrough).
    #[error("upstream '{0}' returned an error response")]
    DownstreamError(String),

    /// A streaming (SSE/WebSocket) upstream exchange aborted mid-flight.
    #[error("stream to upstream '{0}' aborted")]
    StreamAborted(String),

    /// The upstream link is unavailable (connection refused, DNS failure,
    /// plaintext transport refused by policy).
    #[error("upstream link unavailable for '{0}': {1}")]
    LinkUnavailable(String, String),

    /// The circuit breaker for this host is open.
    #[error("circuit breaker open for upstream '{0}'")]
    CircuitBreakerOpen(String),

    /// A bound plugin could not be resolved at request time.
    #[error("plugin '{0}' not found")]
    PluginNotFound(String),

    // ---------------------------------------------------------------------------
    // 504 — timeouts
    // ---------------------------------------------------------------------------
    /// Connection establishment exceeded the deadline.
    #[error("connection to upstream '{0}' timed out")]
    ConnectionTimeout(String),

    /// The upstream request exceeded [`crate::config::OagwConfig::proxy_timeout_secs`].
    #[error("upstream '{0}' did not respond in time")]
    RequestTimeout(String),

    /// An established stream was idle for too long.
    #[error("stream to upstream '{0}' timed out while idle")]
    IdleTimeout(String),

    // ---------------------------------------------------------------------------
    // 500
    // ---------------------------------------------------------------------------
    /// Unexpected internal failure.
    #[error("internal error: {0}")]
    Internal(String),
}

impl DomainError {
    /// The GTS error type identifier carried in the `type` member of the
    /// RFC 9457 problem document.
    #[must_use]
    pub fn gts_type(&self) -> &'static str {
        match self {
            Self::Route(_)
            | Self::Validation(_)
            | Self::Semantic(_)
            | Self::ImmutableField { .. } => {
                oagw_error_type!("validation.error.v1")
            }
            Self::MissingTargetHost(_) => oagw_error_type!("routing.missing_target_host.v1"),
            Self::InvalidTargetHost(_) => oagw_error_type!("routing.invalid_target_host.v1"),
            Self::UnknownTargetHost(_, _) => oagw_error_type!("routing.unknown_target_host.v1"),
            Self::AuthenticationFailed(_) => oagw_error_type!("auth.failed.v1"),
            Self::NotFound(_) | Self::RouteNotFound { .. } => {
                oagw_error_type!("route.not_found.v1")
            }
            Self::AliasConflict { .. } => oagw_error_type!("upstream.alias_conflict.v1"),
            Self::DuplicateRouteMatch(_) => oagw_error_type!("route.duplicate_match.v1"),
            Self::PluginInUse(_) => oagw_error_type!("plugin.in_use.v1"),
            Self::PayloadTooLarge(_) => oagw_error_type!("payload.too_large.v1"),
            Self::RateLimitExceeded(_) => oagw_error_type!("rate_limit.exceeded.v1"),
            Self::SecretNotFound(_) => oagw_error_type!("secret.not_found.v1"),
            Self::ProtocolError(_, _) => oagw_error_type!("protocol.error.v1"),
            Self::DownstreamError(_) => oagw_error_type!("downstream.error.v1"),
            Self::StreamAborted(_) => oagw_error_type!("stream.aborted.v1"),
            Self::LinkUnavailable(_, _) => oagw_error_type!("link.unavailable.v1"),
            Self::CircuitBreakerOpen(_) => oagw_error_type!("circuit_breaker.open.v1"),
            Self::PluginNotFound(_) => oagw_error_type!("plugin.not_found.v1"),
            Self::ConnectionTimeout(_) => oagw_error_type!("timeout.connection.v1"),
            Self::RequestTimeout(_) => oagw_error_type!("timeout.request.v1"),
            Self::IdleTimeout(_) => oagw_error_type!("timeout.idle.v1"),
            Self::Internal(_) => oagw_error_type!("internal.error.v1"),
        }
    }

    /// HTTP status this error maps to, per the DESIGN error table.
    #[must_use]
    pub fn http_status(&self) -> u16 {
        match self {
            Self::Route(_)
            | Self::Validation(_)
            | Self::MissingTargetHost(_)
            | Self::InvalidTargetHost(_)
            | Self::UnknownTargetHost(_, _) => 400,
            Self::ImmutableField { .. } => 409,
            Self::Semantic(_) => 422,
            Self::AuthenticationFailed(_) => 401,
            Self::NotFound(_) | Self::RouteNotFound { .. } => 404,
            Self::AliasConflict { .. }
            | Self::DuplicateRouteMatch(_)
            | Self::PluginInUse(_) => 409,
            Self::PayloadTooLarge(_) => 413,
            Self::RateLimitExceeded(_) => 429,
            Self::SecretNotFound(_) => 500,
            Self::ProtocolError(_, _) | Self::DownstreamError(_) | Self::StreamAborted(_) => 502,
            Self::LinkUnavailable(_, _)
            | Self::CircuitBreakerOpen(_)
            | Self::PluginNotFound(_) => 503,
            Self::ConnectionTimeout(_) | Self::RequestTimeout(_) | Self::IdleTimeout(_) => 504,
            Self::Internal(_) => 500,
        }
    }

    /// Short human-readable summary used as the problem `title`.
    #[must_use]
    pub fn title(&self) -> &'static str {
        match self {
            Self::Route(_) => "Route Error",
            Self::Validation(_) | Self::Semantic(_) | Self::ImmutableField { .. } => {
                "Validation Error"
            }
            Self::MissingTargetHost(_) => "Missing Target Host",
            Self::InvalidTargetHost(_) => "Invalid Target Host",
            Self::UnknownTargetHost(_, _) => "Unknown Target Host",
            Self::AuthenticationFailed(_) => "Authentication Failed",
            Self::NotFound(_) | Self::RouteNotFound { .. } => "Route Not Found",
            Self::AliasConflict { .. } => "Alias Conflict",
            Self::DuplicateRouteMatch(_) => "Duplicate Route Match",
            Self::PluginInUse(_) => "Plugin In Use",
            Self::PayloadTooLarge(_) => "Payload Too Large",
            Self::RateLimitExceeded(_) => "Rate Limit Exceeded",
            Self::SecretNotFound(_) => "Secret Not Found",
            Self::ProtocolError(_, _) => "Protocol Error",
            Self::DownstreamError(_) => "Downstream Error",
            Self::StreamAborted(_) => "Stream Aborted",
            Self::LinkUnavailable(_, _) => "Link Unavailable",
            Self::CircuitBreakerOpen(_) => "Circuit Breaker Open",
            Self::PluginNotFound(_) => "Plugin Not Found",
            Self::ConnectionTimeout(_) => "Connection Timeout",
            Self::RequestTimeout(_) => "Request Timeout",
            Self::IdleTimeout(_) => "Idle Timeout",
            Self::Internal(_) => "Internal Error",
        }
    }

    /// Human-readable explanation for this occurrence.
    #[must_use]
    pub fn detail(&self) -> String {
        match self {
            Self::Internal(message)
            | Self::NotFound(message)
            | Self::Route(message)
            | Self::Validation(message)
            | Self::Semantic(message)
            | Self::DuplicateRouteMatch(message)
            | Self::PluginInUse(message)
            | Self::PluginNotFound(message) => message.clone(),
            _ => self.to_string(),
        }
    }

    /// `true` when the DESIGN table marks the error as retriable.
    #[must_use]
    pub fn retriable(&self) -> bool {
        matches!(
            self,
            Self::RateLimitExceeded(_)
                | Self::LinkUnavailable(_, _)
                | Self::CircuitBreakerOpen(_)
                | Self::ConnectionTimeout(_)
                | Self::RequestTimeout(_)
                | Self::IdleTimeout(_)
        )
    }
}

/// Every OAGW error type identifier, for the registry/catalogue tests.
pub const ALL_ERROR_TYPES: &[&str] = &[
    oagw_error_type!("validation.error.v1"),
    oagw_error_type!("routing.missing_target_host.v1"),
    oagw_error_type!("routing.invalid_target_host.v1"),
    oagw_error_type!("routing.unknown_target_host.v1"),
    oagw_error_type!("auth.failed.v1"),
    oagw_error_type!("route.not_found.v1"),
    oagw_error_type!("upstream.alias_conflict.v1"),
    oagw_error_type!("route.duplicate_match.v1"),
    oagw_error_type!("plugin.in_use.v1"),
    oagw_error_type!("payload.too_large.v1"),
    oagw_error_type!("rate_limit.exceeded.v1"),
    oagw_error_type!("secret.not_found.v1"),
    oagw_error_type!("protocol.error.v1"),
    oagw_error_type!("downstream.error.v1"),
    oagw_error_type!("stream.aborted.v1"),
    oagw_error_type!("link.unavailable.v1"),
    oagw_error_type!("circuit_breaker.open.v1"),
    oagw_error_type!("plugin.not_found.v1"),
    oagw_error_type!("timeout.connection.v1"),
    oagw_error_type!("timeout.request.v1"),
    oagw_error_type!("timeout.idle.v1"),
];

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn error_type_prefix_is_the_canonical_error_type() {
        assert_eq!(ERROR_TYPE_PREFIX, "gts.cf.core.errors.err.v1~");
    }

    #[test]
    fn validation_maps_to_400_and_the_table_uri() {
        let e = DomainError::Validation("bad alias".to_owned());
        assert_eq!(e.http_status(), 400);
        assert_eq!(e.gts_type(), "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1");
        assert_eq!(e.title(), "Validation Error");
    }

    #[test]
    fn not_found_maps_to_404() {
        let e = DomainError::RouteNotFound {
            method: "GET".to_owned(),
            path: "/v1/x".to_owned(),
            alias: "api.openai.com".to_owned(),
        };
        assert_eq!(e.http_status(), 404);
        assert_eq!(e.gts_type(), "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1");
        assert_eq!(e.title(), "Route Not Found");
        assert!(!e.retriable());
    }

    #[test]
    fn alias_conflict_maps_to_409() {
        let e = DomainError::AliasConflict {
            alias: "vendor.com".to_owned(),
            existing_id: uuid::Uuid::nil(),
        };
        assert_eq!(e.http_status(), 409);
        assert_eq!(e.gts_type(), "gts.cf.core.errors.err.v1~cf.oagw.upstream.alias_conflict.v1");
    }

    #[test]
    fn upstream_failures_map_to_the_5xx_band() {
        assert_eq!(
            DomainError::DownstreamError("x".to_owned()).http_status(),
            502
        );
        assert_eq!(
            DomainError::LinkUnavailable("x".to_owned(), "refused".to_owned()).http_status(),
            503
        );
        assert_eq!(
            DomainError::RequestTimeout("x".to_owned()).http_status(),
            504
        );
        assert_eq!(DomainError::PayloadTooLarge(1).http_status(), 413);
        assert_eq!(DomainError::RateLimitExceeded("x".to_owned()).http_status(), 429);
    }

    #[test]
    fn every_table_error_type_is_distinct_and_covered() {
        let mut seen = std::collections::BTreeSet::new();
        for t in ALL_ERROR_TYPES {
            assert!(seen.insert(*t), "duplicate error type: {t}");
            assert!(t.starts_with(ERROR_TYPE_PREFIX), "{t} must sit under the canonical error type");
        }
        assert_eq!(ALL_ERROR_TYPES.len(), 21);
    }

    #[test]
    fn semantic_body_errors_map_to_422() {
        let e = DomainError::Semantic("route must match exactly one protocol".to_owned());
        assert_eq!(e.http_status(), 422);
    }

    #[test]
    fn detail_is_human_readable() {
        assert_eq!(
            DomainError::PluginInUse("still bound".to_owned()).detail(),
            "still bound"
        );
        assert_eq!(
            DomainError::ConnectionTimeout("api.openai.com".to_owned()).detail(),
            "connection to upstream 'api.openai.com' timed out"
        );
    }
}
