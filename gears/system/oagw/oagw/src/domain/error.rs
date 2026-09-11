//! The OAGW error vocabulary.
//!
//! `OagwError` is the single error type surfaced on the wire; each variant maps
//! to exactly one row of the error table in `contracts/errors.md`, carrying its
//! HTTP status, GTS type identifier, title and detail.

pub use crate::domain::model::DomainError;
use crate::gts_helpers::error_type_id;
use std::time::Duration;

/// Every gateway-generated failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OagwError {
    /// Request or configuration validation failed (400).
    ValidationError(String),
    /// A route body violated its contract (400).
    RouteError(String),
    /// `X-OAGW-Target-Host` missing for a multi-endpoint upstream (400).
    MissingTargetHost(String),
    /// `X-OAGW-Target-Host` is not a bare hostname or IP (400).
    InvalidTargetHost(String),
    /// `X-OAGW-Target-Host` matches no configured endpoint (400).
    UnknownTargetHost(String),
    /// Authentication to the upstream failed (401).
    AuthenticationFailed(String),
    /// Alias or route could not be resolved (404).
    RouteNotFound(String),
    /// A create would duplicate an existing resource (409).
    Conflict(String),
    /// The plugin is still referenced (409).
    PluginInUse(String),
    /// Request body exceeds the hard limit (413).
    PayloadTooLarge(String),
    /// Rate limit exceeded (429).
    RateLimitExceeded {
        detail: String,
        retry_after: Duration,
    },
    /// Referenced credential does not exist (500).
    SecretNotFound(String),
    /// Protocol-level failure talking to the upstream (502).
    ProtocolError(String),
    /// The upstream service failed (502).
    DownstreamError(String),
    /// A stream failed before or during establishment (502).
    StreamAborted(String),
    /// The upstream link is unavailable (503).
    LinkUnavailable(String),
    /// The circuit breaker is open (503).
    CircuitBreakerOpen(String),
    /// The named plugin has no implementation (503).
    PluginNotFound(String),
    /// The connection phase timed out (504).
    ConnectionTimeout(String),
    /// The request timed out (504).
    RequestTimeout(String),
    /// The stream went idle past its budget (504).
    IdleTimeout(String),
}

impl OagwError {
    /// HTTP status for this error.
    #[must_use]
    pub fn status(&self) -> u16 {
        match self {
            Self::ValidationError(_)
            | Self::RouteError(_)
            | Self::MissingTargetHost(_)
            | Self::InvalidTargetHost(_)
            | Self::UnknownTargetHost(_) => 400,
            Self::AuthenticationFailed(_) => 401,
            Self::RouteNotFound(_) => 404,
            Self::Conflict(_) | Self::PluginInUse(_) => 409,
            Self::PayloadTooLarge(_) => 413,
            Self::RateLimitExceeded { .. } => 429,
            Self::SecretNotFound(_) => 500,
            Self::ProtocolError(_) | Self::DownstreamError(_) | Self::StreamAborted(_) => 502,
            Self::LinkUnavailable(_) | Self::CircuitBreakerOpen(_) | Self::PluginNotFound(_) => 503,
            Self::ConnectionTimeout(_) | Self::RequestTimeout(_) | Self::IdleTimeout(_) => 504,
        }
    }

    /// GTS type identifier for this error.
    #[must_use]
    pub fn type_id(&self) -> String {
        match self {
            Self::ValidationError(_) | Self::RouteError(_) => error_type_id("validation.error"),
            Self::MissingTargetHost(_) => error_type_id("routing.missing_target_host"),
            Self::InvalidTargetHost(_) => error_type_id("routing.invalid_target_host"),
            Self::UnknownTargetHost(_) => error_type_id("routing.unknown_target_host"),
            Self::AuthenticationFailed(_) => error_type_id("auth.failed"),
            Self::RouteNotFound(_) => error_type_id("route.not_found"),
            Self::Conflict(_) => error_type_id("conflict"),
            Self::PluginInUse(_) => error_type_id("plugin.in_use"),
            Self::PayloadTooLarge(_) => error_type_id("payload.too_large"),
            Self::RateLimitExceeded { .. } => error_type_id("rate_limit.exceeded"),
            Self::SecretNotFound(_) => error_type_id("secret.not_found"),
            Self::ProtocolError(_) => error_type_id("protocol.error"),
            Self::DownstreamError(_) => error_type_id("downstream.error"),
            Self::StreamAborted(_) => error_type_id("stream.aborted"),
            Self::LinkUnavailable(_) => error_type_id("link.unavailable"),
            Self::CircuitBreakerOpen(_) => error_type_id("circuit_breaker.open"),
            Self::PluginNotFound(_) => error_type_id("plugin.not_found"),
            Self::ConnectionTimeout(_) => error_type_id("timeout.connection"),
            Self::RequestTimeout(_) => error_type_id("timeout.request"),
            Self::IdleTimeout(_) => error_type_id("timeout.idle"),
        }
    }

    /// Human-readable title for this error.
    #[must_use]
    pub fn title(&self) -> &'static str {
        match self {
            Self::ValidationError(_) => "Validation Error",
            Self::RouteError(_) => "Route Error",
            Self::MissingTargetHost(_) => "Missing Target Host",
            Self::InvalidTargetHost(_) => "Invalid Target Host",
            Self::UnknownTargetHost(_) => "Unknown Target Host",
            Self::AuthenticationFailed(_) => "Authentication Failed",
            Self::RouteNotFound(_) => "Route Not Found",
            Self::Conflict(_) => "Conflict",
            Self::PluginInUse(_) => "Plugin In Use",
            Self::PayloadTooLarge(_) => "Payload Too Large",
            Self::RateLimitExceeded { .. } => "Rate Limit Exceeded",
            Self::SecretNotFound(_) => "Secret Not Found",
            Self::ProtocolError(_) => "Protocol Error",
            Self::DownstreamError(_) => "Downstream Error",
            Self::StreamAborted(_) => "Stream Aborted",
            Self::LinkUnavailable(_) => "Link Unavailable",
            Self::CircuitBreakerOpen(_) => "Circuit Breaker Open",
            Self::PluginNotFound(_) => "Plugin Not Found",
            Self::ConnectionTimeout(_) => "Connection Timeout",
            Self::RequestTimeout(_) => "Request Timeout",
            Self::IdleTimeout(_) => "Idle Timeout",
        }
    }

    /// The detail message carried in the problem document.
    #[must_use]
    pub fn detail(&self) -> String {
        match self {
            Self::ValidationError(msg)
            | Self::RouteError(msg)
            | Self::MissingTargetHost(msg)
            | Self::InvalidTargetHost(msg)
            | Self::UnknownTargetHost(msg)
            | Self::AuthenticationFailed(msg)
            | Self::RouteNotFound(msg)
            | Self::Conflict(msg)
            | Self::PluginInUse(msg)
            | Self::PayloadTooLarge(msg)
            | Self::SecretNotFound(msg)
            | Self::ProtocolError(msg)
            | Self::DownstreamError(msg)
            | Self::StreamAborted(msg)
            | Self::LinkUnavailable(msg)
            | Self::CircuitBreakerOpen(msg)
            | Self::PluginNotFound(msg)
            | Self::ConnectionTimeout(msg)
            | Self::RequestTimeout(msg)
            | Self::IdleTimeout(msg) => msg.clone(),
            Self::RateLimitExceeded { detail, .. } => detail.clone(),
        }
    }

    /// Whether the client may retry the same request.
    #[must_use]
    pub fn retriable(&self) -> bool {
        matches!(
            self,
            Self::RateLimitExceeded { .. }
                | Self::LinkUnavailable(_)
                | Self::CircuitBreakerOpen(_)
                | Self::ConnectionTimeout(_)
                | Self::RequestTimeout(_)
                | Self::IdleTimeout(_)
        )
    }

    /// Retry guidance for retriable errors.
    #[must_use]
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RateLimitExceeded { retry_after, .. } => Some(*retry_after),
            Self::LinkUnavailable(_)
            | Self::CircuitBreakerOpen(_)
            | Self::ConnectionTimeout(_)
            | Self::RequestTimeout(_)
            | Self::IdleTimeout(_) => Some(Duration::from_secs(1)),
            _ => None,
        }
    }

    /// Whether the error names a credential secret (and must therefore be
    /// redacted from any log line).
    #[must_use]
    pub fn touches_secret(&self) -> bool {
        matches!(
            self,
            Self::SecretNotFound(_) | Self::AuthenticationFailed(_)
        )
    }
}

impl std::fmt::Display for OagwError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.title(), self.status())
    }
}

impl std::error::Error for OagwError {}

impl From<DomainError> for OagwError {
    fn from(err: DomainError) -> Self {
        match err {
            DomainError::Invalid(msg) => Self::ValidationError(msg),
            DomainError::NotFound { kind, target } => {
                Self::RouteNotFound(format!("no {kind} for {target}"))
            }
            DomainError::Conflict(msg) => Self::Conflict(msg),
            DomainError::Internal(msg) => Self::ProtocolError(msg),
        }
    }
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod tests;
