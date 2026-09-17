//! Domain error type carrying the contract error catalog.
//!
//! Every gateway error maps to one row of the `DESIGN.md` §3.3 error table:
//! an HTTP status, a GTS instance id under
//! `gts.cf.core.errors.err.v1~…`, a title and a retriable flag. The transport
//! layer renders these into RFC 9457 `application/problem+json` bodies with
//! `X-OAGW-Error-Source: gateway`.

/// Root of every gateway error body.
pub const ERROR_TYPE_BASE: &str = "gts.cf.core.errors.err.v1~";

/// Catalogued error kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// General request validation failure.
    Validation,
    /// `X-OAGW-Target-Host` required but missing.
    MissingTargetHost,
    /// `X-OAGW-Target-Host` format invalid.
    InvalidTargetHost,
    /// `X-OAGW-Target-Host` does not match a configured endpoint.
    UnknownTargetHost,
    /// Authentication to the upstream failed.
    AuthenticationFailed,
    /// No matching route found.
    RouteNotFound,
    /// Management resource not found.
    ResourceNotFound,
    /// Plugin still referenced by an upstream or route.
    PluginInUse,
    /// Alias or match-rule uniqueness conflict.
    ResourceConflict,
    /// Request payload exceeds the hard limit.
    PayloadTooLarge,
    /// Rate limit exceeded.
    RateLimitExceeded,
    /// Referenced secret is missing or inaccessible.
    SecretNotFound,
    /// Protocol-level error talking to the upstream.
    ProtocolError,
    /// Upstream service returned an error.
    DownstreamError,
    /// Streaming connection aborted.
    StreamAborted,
    /// Upstream link unavailable.
    LinkUnavailable,
    /// Circuit breaker open.
    CircuitBreakerOpen,
    /// Plugin not found / not executable.
    PluginNotFound,
    /// Connection to the upstream timed out.
    ConnectionTimeout,
    /// Upstream request timed out.
    RequestTimeout,
    /// Idle stream timeout.
    IdleTimeout,
    /// Cross-origin request from an origin that is not configured.
    CorsOriginNotAllowed,
    /// Cross-origin request with a method that is not allowed.
    CorsMethodNotAllowed,
}

impl ErrorKind {
    /// HTTP status for this error kind.
    #[must_use]
    pub const fn http_status(self) -> u16 {
        match self {
            Self::Validation
            | Self::MissingTargetHost
            | Self::InvalidTargetHost
            | Self::UnknownTargetHost => 400,
            Self::AuthenticationFailed => 401,
            Self::RouteNotFound | Self::ResourceNotFound => 404,
            Self::PluginInUse | Self::ResourceConflict => 409,
            Self::PayloadTooLarge => 413,
            Self::RateLimitExceeded => 429,
            Self::SecretNotFound => 500,
            Self::ProtocolError | Self::DownstreamError | Self::StreamAborted => 502,
            Self::LinkUnavailable | Self::CircuitBreakerOpen | Self::PluginNotFound => 503,
            Self::ConnectionTimeout | Self::RequestTimeout | Self::IdleTimeout => 504,
            Self::CorsOriginNotAllowed | Self::CorsMethodNotAllowed => 403,
        }
    }

    /// GTS instance id (after `gts.cf.core.errors.err.v1~`).
    #[must_use]
    pub const fn gts_instance(self) -> &'static str {
        match self {
            Self::Validation => "cf.oagw.validation.error.v1",
            Self::MissingTargetHost => "cf.oagw.routing.missing_target_host.v1",
            Self::InvalidTargetHost => "cf.oagw.routing.invalid_target_host.v1",
            Self::UnknownTargetHost => "cf.oagw.routing.unknown_target_host.v1",
            Self::AuthenticationFailed => "cf.oagw.auth.failed.v1",
            Self::RouteNotFound => "cf.oagw.route.not_found.v1",
            Self::ResourceNotFound => "cf.oagw.resource.not_found.v1",
            Self::PluginInUse => "cf.oagw.plugin.in_use.v1",
            Self::ResourceConflict => "cf.oagw.resource.conflict.v1",
            Self::PayloadTooLarge => "cf.oagw.payload.too_large.v1",
            Self::RateLimitExceeded => "cf.oagw.rate_limit.exceeded.v1",
            Self::SecretNotFound => "cf.oagw.secret.not_found.v1",
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

    /// Full GTS type identifier for the error body.
    #[must_use]
    pub fn gts_type(self) -> String {
        format!("{ERROR_TYPE_BASE}{}", self.gts_instance())
    }

    /// RFC 9457 `title` for this error kind.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Validation => "Validation Error",
            Self::MissingTargetHost => "Missing Target Host",
            Self::InvalidTargetHost => "Invalid Target Host",
            Self::UnknownTargetHost => "Unknown Target Host",
            Self::AuthenticationFailed => "Authentication Failed",
            Self::RouteNotFound => "Route Not Found",
            Self::ResourceNotFound => "Resource Not Found",
            Self::PluginInUse => "Plugin In Use",
            Self::ResourceConflict => "Resource Conflict",
            Self::PayloadTooLarge => "Payload Too Large",
            Self::RateLimitExceeded => "Rate Limit Exceeded",
            Self::SecretNotFound => "Secret Not Found",
            Self::ProtocolError => "Protocol Error",
            Self::DownstreamError => "Downstream Error",
            Self::StreamAborted => "Stream Aborted",
            Self::LinkUnavailable => "Link Unavailable",
            Self::CircuitBreakerOpen => "Circuit Breaker Open",
            Self::PluginNotFound => "Plugin Not Found",
            Self::ConnectionTimeout => "Connection Timeout",
            Self::RequestTimeout => "Request Timeout",
            Self::IdleTimeout => "Idle Timeout",
            Self::CorsOriginNotAllowed => "CORS Origin Not Allowed",
            Self::CorsMethodNotAllowed => "CORS Method Not Allowed",
        }
    }

    /// Whether a client may retry the request.
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

/// Remaining rate-limit budget attached to a rejection (ADR-0003).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RateLimitBudget {
    /// Effective bucket capacity, the `X-RateLimit-Limit` value.
    pub limit: u32,
    /// Tokens left after the rejected request.
    pub remaining: u32,
    /// Epoch seconds at which the bucket is replenished.
    pub reset_at: u64,
}

/// RFC 9457 extension fields (ADR-0007).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ErrorExtensions {
    /// Upstream GTS identifier involved in the failure.
    pub upstream_id: Option<String>,
    /// Upstream host that was targeted.
    pub host: Option<String>,
    /// Request path that was being served.
    pub path: Option<String>,
    /// Retry guidance in seconds.
    pub retry_after_seconds: Option<u64>,
    /// Distributed trace correlation id.
    pub trace_id: Option<String>,
    /// Configured endpoint hosts (routing errors).
    pub valid_hosts: Vec<String>,
    /// Rejected value (routing errors).
    pub invalid_value: Option<String>,
    /// Alias that was being resolved.
    pub alias: Option<String>,
    /// Remaining budget, attached to a rate-limit rejection.
    pub rate_limit: Option<RateLimitBudget>,
}

/// A gateway or management error with its contract metadata attached.
#[derive(Debug, Clone)]
pub struct DomainError {
    /// Catalogued kind driving status, GTS type and title.
    pub kind: ErrorKind,
    /// Human-readable explanation of this occurrence.
    pub detail: String,
    /// RFC 9457 extension fields.
    ///
    /// Boxed so that `Result<_, DomainError>` stays small: the extension map is
    /// eight optional fields and is only populated on routing errors.
    pub extensions: Box<ErrorExtensions>,
}

impl DomainError {
    /// Build an error for `kind` with `detail` and empty extensions.
    #[must_use]
    pub fn new(kind: ErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
            extensions: Box::default(),
        }
    }

    /// Attach RFC 9457 extension fields.
    #[must_use]
    pub fn with_extensions(mut self, extensions: ErrorExtensions) -> Self {
        self.extensions = Box::new(extensions);
        self
    }

    /// 400 `cf.oagw.validation.error.v1`.
    #[must_use]
    pub fn validation(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::Validation, detail)
    }

    /// 404 `cf.oagw.resource.not_found.v1`.
    #[must_use]
    pub fn resource_not_found(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::ResourceNotFound, detail)
    }

    /// 404 `cf.oagw.route.not_found.v1`.
    #[must_use]
    pub fn route_not_found(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::RouteNotFound, detail)
    }

    /// 409 `cf.oagw.resource.conflict.v1`.
    #[must_use]
    pub fn conflict(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::ResourceConflict, detail)
    }

    /// 409 `cf.oagw.plugin.in_use.v1`.
    #[must_use]
    pub fn plugin_in_use(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::PluginInUse, detail)
    }

    /// 413 `cf.oagw.payload.too_large.v1`.
    #[must_use]
    pub fn payload_too_large(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::PayloadTooLarge, detail)
    }

    /// 429 `cf.oagw.rate_limit.exceeded.v1`.
    #[must_use]
    pub fn rate_limit_exceeded(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::RateLimitExceeded, detail)
    }

    /// 503 `cf.oagw.link.unavailable.v1`.
    #[must_use]
    pub fn link_unavailable(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::LinkUnavailable, detail)
    }

    /// 502 `cf.oagw.protocol.error.v1`.
    #[must_use]
    pub fn protocol_error(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::ProtocolError, detail)
    }

    /// 502 `cf.oagw.downstream.error.v1`.
    #[must_use]
    pub fn downstream_error(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::DownstreamError, detail)
    }

    /// 504 `cf.oagw.timeout.request.v1`.
    #[must_use]
    pub fn request_timeout(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::RequestTimeout, detail)
    }

    /// 503 `cf.oagw.plugin.not_found.v1`.
    #[must_use]
    pub fn plugin_not_found(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::PluginNotFound, detail)
    }

    /// 401 `cf.oagw.auth.failed.v1`.
    #[must_use]
    pub fn auth_failed(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::AuthenticationFailed, detail)
    }

    /// 500 `cf.oagw.secret.not_found.v1`.
    #[must_use]
    pub fn secret_not_found(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::SecretNotFound, detail)
    }

    /// 403 `cf.oagw.cors.origin_not_allowed.v1`.
    #[must_use]
    pub fn cors_origin_not_allowed(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::CorsOriginNotAllowed, detail)
    }

    /// 403 `cf.oagw.cors.method_not_allowed.v1`.
    #[must_use]
    pub fn cors_method_not_allowed(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::CorsMethodNotAllowed, detail)
    }
}

impl std::fmt::Display for DomainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} ({}): {}",
            self.kind.title(),
            self.kind.http_status(),
            self.detail
        )
    }
}

impl std::error::Error for DomainError {}

#[cfg(test)]
#[path = "error_tests.rs"]
mod error_tests;
