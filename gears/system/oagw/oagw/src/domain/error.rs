//! Domain error type and its mapping to the documented error surface.
//!
//! Every externally visible failure is one of these variants, mapped at the
//! transport boundary to the RFC 9457 table in `DESIGN.md` § 3.3 and tagged
//! with `X-OAGW-Error-Source: gateway` (see `api::rest::error`).

/// Domain error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DomainError {
    /// Request validation failed (`400`).
    #[error("{0}")]
    Validation(String),

    /// An alias collision within a tenant (`409`).
    #[error("upstream alias already exists: {0}")]
    AliasConflict(String),

    /// The referenced resource does not exist, or belongs to another tenant
    /// (`404`).
    #[error("resource not found")]
    NotFound,

    /// No route matches the request (`404`).
    #[error("no route matches the request")]
    RouteNotFound,

    /// A match rule collides with an existing route (`409`).
    #[error("duplicate route match rule")]
    DuplicateMatchRule,

    /// A custom plugin of the same kind already carries the name (`409`).
    #[error("a {kind} plugin named '{name}' already exists")]
    DuplicatePlugin {
        /// Plugin family of the rejected definition.
        kind: String,
        /// Rejected name.
        name: String,
    },

    /// The plugin is still referenced by an upstream or route (`409`).
    #[error("plugin is still in use: {0}")]
    PluginInUse(String),

    /// The upstream has more than one endpoint and no target was selected
    /// (`400`).
    #[error("X-OAGW-Target-Host is required for this upstream")]
    MissingTargetHost {
        /// The endpoints that would have been acceptable.
        valid_hosts: Vec<String>,
    },

    /// The target host header is malformed (`400`).
    #[error("invalid X-OAGW-Target-Host value")]
    InvalidTargetHost {
        /// The rejected value.
        value: String,
    },

    /// The target host is not one of the configured endpoints (`400`).
    #[error("unknown X-OAGW-Target-Host value")]
    UnknownTargetHost {
        /// The rejected value.
        value: String,
        /// The endpoints that would have been acceptable.
        valid_hosts: Vec<String>,
    },

    /// Authentication against the upstream failed (`401`).
    #[error("upstream authentication failed")]
    AuthenticationFailed,

    /// A referenced credential could not be resolved (`500`).
    #[error("referenced secret not found")]
    SecretNotFound,

    /// The request body exceeds the hard size limit (`413`).
    #[error("request payload exceeds the {0} byte limit")]
    PayloadTooLarge(usize),

    /// The rate limit is exhausted (`429`).
    #[error("rate limit exceeded")]
    RateLimitExceeded {
        /// Effective sustained rate, tokens per window.
        limit: u32,
        /// Window of the effective rate, in seconds.
        window_secs: u64,
        /// Seconds until the bucket refills.
        retry_after_secs: u64,
    },

    /// A browser origin is not allowed (`403`).
    #[error("origin is not allowed: {0}")]
    CorsOriginNotAllowed(String),

    /// A browser method is not allowed (`403`).
    #[error("method is not allowed: {0}")]
    CorsMethodNotAllowed(String),

    /// The declared protocol has no proxy code path (`502`).
    #[error("protocol is not supported by the data plane: {0}")]
    ProtocolError(String),

    /// The upstream returned a malformed response (`502`).
    #[error("upstream returned a malformed response")]
    DownstreamError,

    /// A stream was aborted mid-flight (`502`).
    #[error("stream aborted")]
    StreamAborted,

    /// The upstream link is unavailable (`503`).
    #[error("upstream link unavailable")]
    LinkUnavailable,

    /// The circuit breaker is open (`503`).
    #[error("circuit breaker open")]
    CircuitBreakerOpen,

    /// A bound plugin has no implementation (`503`).
    #[error("plugin not found: {0}")]
    PluginNotFound(String),

    /// Establishing the upstream connection timed out (`504`).
    #[error("connection to upstream timed out")]
    ConnectionTimeout,

    /// The upstream call exceeded its deadline (`504`).
    #[error("upstream request timed out")]
    RequestTimeout,

    /// The upstream stopped sending mid-response (`504`).
    #[error("upstream idle timeout")]
    IdleTimeout,

    /// An unexpected internal condition.
    #[error("internal error: {0}")]
    Internal(String),
}

impl DomainError {
    /// HTTP status for the variant.
    #[must_use]
    pub fn status(&self) -> u16 {
        use DomainError as E;
        match self {
            E::Validation(_)
            | E::MissingTargetHost { .. }
            | E::InvalidTargetHost { .. }
            | E::UnknownTargetHost { .. } => 400,
            E::AuthenticationFailed => 401,
            E::NotFound | E::RouteNotFound => 404,
            E::AliasConflict(_)
            | E::DuplicateMatchRule
            | E::DuplicatePlugin { .. }
            | E::PluginInUse(_) => 409,
            E::PayloadTooLarge(_) => 413,
            E::RateLimitExceeded { .. } => 429,
            E::CorsOriginNotAllowed(_) | E::CorsMethodNotAllowed(_) => 403,
            E::SecretNotFound => 500,
            E::ProtocolError(_) | E::DownstreamError | E::StreamAborted => 502,
            E::LinkUnavailable | E::CircuitBreakerOpen => 503,
            E::PluginNotFound(_) => 503,
            E::ConnectionTimeout | E::RequestTimeout | E::IdleTimeout => 504,
            E::Internal(_) => 500,
        }
    }

    /// GTS error `type` instance part, relative to
    /// `gts.cf.core.errors.err.v1~` (see `DESIGN.md` § 3.3).
    #[must_use]
    pub fn error_type(&self) -> &'static str {
        use DomainError as E;
        match self {
            E::Validation(_) => "cf.oagw.validation.error.v1",
            E::MissingTargetHost { .. } => "cf.oagw.routing.missing_target_host.v1",
            E::InvalidTargetHost { .. } => "cf.oagw.routing.invalid_target_host.v1",
            E::UnknownTargetHost { .. } => "cf.oagw.routing.unknown_target_host.v1",
            E::AuthenticationFailed => "cf.oagw.auth.failed.v1",
            E::NotFound => "cf.oagw.validation.error.v1",
            E::RouteNotFound => "cf.oagw.route.not_found.v1",
            E::AliasConflict(_) => "cf.oagw.validation.error.v1",
            E::DuplicateMatchRule => "cf.oagw.validation.error.v1",
            E::DuplicatePlugin { .. } => "cf.oagw.validation.error.v1",
            E::PluginInUse(_) => "cf.oagw.plugin.in_use.v1",
            E::PayloadTooLarge(_) => "cf.oagw.payload.too_large.v1",
            E::RateLimitExceeded { .. } => "cf.oagw.rate_limit.exceeded.v1",
            E::CorsOriginNotAllowed(_) => "cf.oagw.cors.origin_not_allowed.v1",
            E::CorsMethodNotAllowed(_) => "cf.oagw.cors.method_not_allowed.v1",
            E::SecretNotFound => "cf.oagw.secret.not_found.v1",
            E::ProtocolError(_) => "cf.oagw.protocol.error.v1",
            E::DownstreamError => "cf.oagw.downstream.error.v1",
            E::StreamAborted => "cf.oagw.stream.aborted.v1",
            E::LinkUnavailable => "cf.oagw.link.unavailable.v1",
            E::CircuitBreakerOpen => "cf.oagw.circuit_breaker.open.v1",
            E::PluginNotFound(_) => "cf.oagw.plugin.not_found.v1",
            E::ConnectionTimeout => "cf.oagw.timeout.connection.v1",
            E::RequestTimeout => "cf.oagw.timeout.request.v1",
            E::IdleTimeout => "cf.oagw.timeout.idle.v1",
            E::Internal(_) => "cf.oagw.validation.error.v1",
        }
    }

    /// Human-readable `title` for the problem document.
    #[must_use]
    pub fn title(&self) -> &'static str {
        use DomainError as E;
        match self {
            E::Validation(_)
            | E::AliasConflict(_)
            | E::DuplicateMatchRule
            | E::DuplicatePlugin { .. }
            | E::NotFound
            | E::Internal(_) => "Validation Error",
            E::MissingTargetHost { .. } => "Missing Target Host",
            E::InvalidTargetHost { .. } => "Invalid Target Host",
            E::UnknownTargetHost { .. } => "Unknown Target Host",
            E::AuthenticationFailed => "Authentication Failed",
            E::RouteNotFound => "Route Not Found",
            E::PluginInUse(_) => "Plugin In Use",
            E::PayloadTooLarge(_) => "Payload Too Large",
            E::RateLimitExceeded { .. } => "Rate Limit Exceeded",
            E::CorsOriginNotAllowed(_) => "Origin Not Allowed",
            E::CorsMethodNotAllowed(_) => "Method Not Allowed",
            E::SecretNotFound => "Secret Not Found",
            E::ProtocolError(_) => "Protocol Error",
            E::DownstreamError => "Downstream Error",
            E::StreamAborted => "Stream Aborted",
            E::LinkUnavailable => "Link Unavailable",
            E::CircuitBreakerOpen => "Circuit Breaker Open",
            E::PluginNotFound(_) => "Plugin Not Found",
            E::ConnectionTimeout => "Connection Timeout",
            E::RequestTimeout => "Request Timeout",
            E::IdleTimeout => "Idle Timeout",
        }
    }
}

/// Full GTS error `type` for a domain error.
#[must_use]
pub fn error_type_id(err: &DomainError) -> String {
    format!("gts.cf.core.errors.err.v1~{}", err.error_type())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_match_the_documented_table() {
        assert_eq!(DomainError::Validation("x".to_owned()).status(), 400);
        assert_eq!(
            DomainError::MissingTargetHost {
                valid_hosts: Vec::new()
            }
            .status(),
            400
        );
        assert_eq!(
            DomainError::UnknownTargetHost {
                value: "h".to_owned(),
                valid_hosts: Vec::new()
            }
            .status(),
            400
        );
        assert_eq!(DomainError::AuthenticationFailed.status(), 401);
        assert_eq!(DomainError::RouteNotFound.status(), 404);
        assert_eq!(DomainError::PluginInUse("p".to_owned()).status(), 409);
        assert_eq!(DomainError::PayloadTooLarge(1).status(), 413);
        assert_eq!(
            DomainError::RateLimitExceeded {
                limit: 1,
                window_secs: 1,
                retry_after_secs: 1
            }
            .status(),
            429
        );
        assert_eq!(DomainError::SecretNotFound.status(), 500);
        assert_eq!(DomainError::ProtocolError("grpc".to_owned()).status(), 502);
        assert_eq!(DomainError::LinkUnavailable.status(), 503);
        assert_eq!(DomainError::RequestTimeout.status(), 504);
    }

    #[test]
    fn error_types_use_the_documented_gts_ids() {
        assert_eq!(
            error_type_id(&DomainError::RouteNotFound),
            "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
        );
        assert_eq!(
            error_type_id(&DomainError::MissingTargetHost {
                valid_hosts: Vec::new(),
            }),
            "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1"
        );
        assert_eq!(
            error_type_id(&DomainError::RateLimitExceeded {
                limit: 1,
                window_secs: 1,
                retry_after_secs: 1
            }),
            "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1"
        );
    }
}
