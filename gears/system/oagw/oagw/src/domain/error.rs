// Created: 2026-08-29 by Constructor Tech
//! Domain error taxonomy.
//!
//! Every variant maps 1:1 onto a row of the normative error table: HTTP status,
//! `~cf.oagw.…` GTS type suffix, title, retriable flag and extension members.
//! The wire encoding (`application/problem+json` + `X-OAGW-Error-Source`) lives
//! in [`crate::api::rest::error`] so the domain layer stays transport-free.

/// Fully qualified GTS type id of the wire error.
///
/// `error_type("validation.error.v1")` →
/// `gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1`
#[must_use]
pub fn error_type(suffix: &str) -> String {
    format!("gts.cf.core.errors.err.v1~cf.oagw.{suffix}")
}

/// Extra members attached to the problem document.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProblemExtras {
    /// Upstream id that the failing request was routed to, when known.
    pub upstream_id: Option<String>,
    /// Upstream host (alias) that the failing request targeted, when known.
    pub host: Option<String>,
    /// Retry guidance in seconds (`rate_limit.exceeded.v1`).
    pub retry_after_seconds: Option<u64>,
    /// Endpoints that would have satisfied `X-OAGW-Target-Host`.
    pub valid_hosts: Option<Vec<String>>,
    /// The alias a target-host error was resolved against.
    pub alias: Option<String>,
    /// Echo of the rejected `X-OAGW-Target-Host` value.
    pub invalid_value: Option<String>,
    /// `{"upstreams": [...], "routes": [...]}` for `plugin.in_use.v1`.
    pub referenced_by: Option<References>,
}

/// Resources that still reference a plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct References {
    /// Upstream ids still binding the plugin.
    pub upstreams: Vec<String>,
    /// Route ids still binding the plugin.
    pub routes: Vec<String>,
}

impl References {
    /// Empty reference set.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            upstreams: Vec::new(),
            routes: Vec::new(),
        }
    }
}

/// Domain error for the outbound API gateway.
///
/// `Debug` is derived; no variant carries credential material.
#[derive(Debug, Clone, PartialEq)]
pub enum OagwError {
    /// 400 — request or resource validation failed.
    Validation(String),
    /// 400 — route specific validation failure (e.g. `path_suffix_mode: disabled`).
    RouteError(String),
    /// 400 — multi-endpoint upstream needs `X-OAGW-Target-Host`.
    MissingTargetHost {
        /// Configured endpoint hosts.
        valid_hosts: Vec<String>,
        /// Alias that was resolved.
        alias: String,
    },
    /// 400 — `X-OAGW-Target-Host` is not a bare host.
    InvalidTargetHost {
        /// The rejected header value.
        invalid_value: String,
    },
    /// 400 — `X-OAGW-Target-Host` is not a configured endpoint.
    UnknownTargetHost {
        /// The rejected header value.
        invalid_value: String,
        /// Configured endpoint hosts.
        valid_hosts: Vec<String>,
    },
    /// 401 — auth plugin could not authenticate the request.
    AuthenticationFailed(String),
    /// 403 — `Origin` is not in the effective CORS allowlist.
    CorsOriginNotAllowed(String),
    /// 403 — method is not in the effective CORS allowlist.
    CorsMethodNotAllowed(String),
    /// 404 — no enabled route matched the request.
    RouteNotFound(String),
    /// 409 — plugin is still referenced by an upstream or route.
    PluginInUse(References),
    /// 413 — request body exceeds the 100 MiB hard limit.
    PayloadTooLarge,
    /// 429 — token bucket exhausted.
    RateLimitExceeded(u64),
    /// 500 — `cred_store` could not resolve a referenced secret.
    SecretNotFound(String),
    /// 502 — upstream returned a transport-level failure.
    DownstreamError(String),
    /// 502 — an SSE / WebSocket upstream leg aborted mid-stream.
    StreamAborted(String),
    /// 502 — protocol level failure (plaintext upstream, bad framing).
    ProtocolError(String),
    /// 503 — upstream link unavailable.
    LinkUnavailable(String),
    /// 503 — circuit breaker is open for the selected endpoint.
    CircuitBreakerOpen,
    /// 503 — plugin id has no registered implementation.
    PluginNotFound(String),
    /// 504 — TCP connect timeout.
    ConnectionTimeout(String),
    /// 504 — request timeout (wall clock budget exceeded).
    RequestTimeout(String),
    /// 504 — idle timeout while streaming.
    IdleTimeout(String),
    /// 500 — unexpected internal failure. Never leaks internals to the wire.
    Internal(String),
}

impl OagwError {
    /// HTTP status the error maps to.
    #[must_use]
    pub fn status(&self) -> u16 {
        match self {
            Self::Validation(_)
            | Self::RouteError(_)
            | Self::MissingTargetHost { .. }
            | Self::InvalidTargetHost { .. }
            | Self::UnknownTargetHost { .. } => 400,
            Self::AuthenticationFailed(_) => 401,
            Self::CorsOriginNotAllowed(_) | Self::CorsMethodNotAllowed(_) => 403,
            Self::RouteNotFound(_) => 404,
            Self::PluginInUse(_) => 409,
            Self::PayloadTooLarge => 413,
            Self::RateLimitExceeded(_) => 429,
            Self::SecretNotFound(_) => 500,
            Self::DownstreamError(_) | Self::StreamAborted(_) | Self::ProtocolError(_) => 502,
            Self::LinkUnavailable(_) | Self::CircuitBreakerOpen | Self::PluginNotFound(_) => 503,
            Self::ConnectionTimeout(_) | Self::RequestTimeout(_) | Self::IdleTimeout(_) => 504,
            Self::Internal(_) => 500,
        }
    }

    /// `~cf.oagw.<suffix>` GTS type suffix.
    #[must_use]
    pub fn type_suffix(&self) -> &'static str {
        match self {
            Self::Validation(_) | Self::RouteError(_) => "validation.error.v1",
            Self::MissingTargetHost { .. } => "routing.missing_target_host.v1",
            Self::InvalidTargetHost { .. } => "routing.invalid_target_host.v1",
            Self::UnknownTargetHost { .. } => "routing.unknown_target_host.v1",
            Self::AuthenticationFailed(_) => "auth.failed.v1",
            Self::CorsOriginNotAllowed(_) => "cors.origin_not_allowed.v1",
            Self::CorsMethodNotAllowed(_) => "cors.method_not_allowed.v1",
            Self::RouteNotFound(_) => "route.not_found.v1",
            Self::PluginInUse(_) => "plugin.in_use.v1",
            Self::PayloadTooLarge => "payload.too_large.v1",
            Self::RateLimitExceeded(_) => "rate_limit.exceeded.v1",
            Self::SecretNotFound(_) => "secret.not_found.v1",
            Self::DownstreamError(_) => "downstream.error.v1",
            Self::StreamAborted(_) => "stream.aborted.v1",
            Self::ProtocolError(_) => "protocol.error.v1",
            Self::LinkUnavailable(_) => "link.unavailable.v1",
            Self::CircuitBreakerOpen => "circuit_breaker.open.v1",
            Self::PluginNotFound(_) => "plugin.not_found.v1",
            Self::ConnectionTimeout(_) => "timeout.connection.v1",
            Self::RequestTimeout(_) => "timeout.request.v1",
            Self::IdleTimeout(_) => "timeout.idle.v1",
            Self::Internal(_) => "internal.error.v1",
        }
    }

    /// Human readable RFC 9457 `title`.
    #[must_use]
    pub fn title(&self) -> &'static str {
        match self {
            Self::Validation(_) => "Validation Error",
            Self::RouteError(_) => "Route Error",
            Self::MissingTargetHost { .. } => "Missing Target Host",
            Self::InvalidTargetHost { .. } => "Invalid Target Host",
            Self::UnknownTargetHost { .. } => "Unknown Target Host",
            Self::AuthenticationFailed(_) => "Authentication Failed",
            Self::CorsOriginNotAllowed(_) => "CORS Origin Not Allowed",
            Self::CorsMethodNotAllowed(_) => "CORS Method Not Allowed",
            Self::RouteNotFound(_) => "Route Not Found",
            Self::PluginInUse(_) => "Plugin In Use",
            Self::PayloadTooLarge => "Payload Too Large",
            Self::RateLimitExceeded(_) => "Rate Limit Exceeded",
            Self::SecretNotFound(_) => "Secret Not Found",
            Self::DownstreamError(_) => "Downstream Error",
            Self::StreamAborted(_) => "Stream Aborted",
            Self::ProtocolError(_) => "Protocol Error",
            Self::LinkUnavailable(_) => "Link Unavailable",
            Self::CircuitBreakerOpen => "Circuit Breaker Open",
            Self::PluginNotFound(_) => "Plugin Not Found",
            Self::ConnectionTimeout(_) => "Connection Timeout",
            Self::RequestTimeout(_) => "Request Timeout",
            Self::IdleTimeout(_) => "Idle Timeout",
            Self::Internal(_) => "Internal Error",
        }
    }

    /// RFC 9457 `detail` message.
    ///
    /// Security: never includes request/response bodies, query parameters,
    /// headers, or credential material — only the values the error table
    /// explicitly allows (identifiers, host names, invalid target-host values).
    #[must_use]
    pub fn detail(&self) -> String {
        match self {
            Self::Validation(msg)
            | Self::RouteError(msg)
            | Self::AuthenticationFailed(msg)
            | Self::CorsOriginNotAllowed(msg)
            | Self::CorsMethodNotAllowed(msg)
            | Self::RouteNotFound(msg)
            | Self::DownstreamError(msg)
            | Self::StreamAborted(msg)
            | Self::ProtocolError(msg)
            | Self::LinkUnavailable(msg)
            | Self::PluginNotFound(msg)
            | Self::ConnectionTimeout(msg)
            | Self::RequestTimeout(msg)
            | Self::IdleTimeout(msg)
            | Self::SecretNotFound(msg)
            | Self::Internal(msg) => msg.clone(),
            Self::MissingTargetHost { valid_hosts, alias } => format!(
                "upstream '{alias}' has multiple endpoints with a common-suffix alias; \
                 the 'x-oagw-target-host' header is required and must be one of: {valid_hosts:?}"
            ),
            Self::InvalidTargetHost { invalid_value } => format!(
                "the 'x-oagw-target-host' header must be a bare host name or IP address, \
                 got '{invalid_value}'"
            ),
            Self::UnknownTargetHost {
                invalid_value,
                valid_hosts,
            } => format!(
                "target host '{invalid_value}' does not match any configured endpoint \
                 (valid hosts: {valid_hosts:?})"
            ),
            Self::PluginInUse(_) => {
                "plugin is still referenced by at least one upstream or route".to_owned()
            }
            Self::PayloadTooLarge => "request body exceeds the 100 MiB limit".to_owned(),
            Self::RateLimitExceeded(retry) => {
                format!("rate limit exceeded; retry after {retry} second(s)")
            }
            Self::CircuitBreakerOpen => {
                "circuit breaker is open for the selected upstream endpoint".to_owned()
            }
        }
    }

    /// Whether the client may safely retry the request.
    #[must_use]
    pub fn retriable(&self) -> bool {
        matches!(
            self,
            Self::RateLimitExceeded(_)
                | Self::DownstreamError(_)
                | Self::LinkUnavailable(_)
                | Self::CircuitBreakerOpen
                | Self::ConnectionTimeout(_)
                | Self::RequestTimeout(_)
                | Self::IdleTimeout(_)
        )
    }

    /// Extension members carried by this error.
    #[must_use]
    pub fn extras(&self) -> ProblemExtras {
        match self {
            Self::MissingTargetHost { valid_hosts, alias } => ProblemExtras {
                valid_hosts: Some(valid_hosts.clone()),
                alias: Some(alias.clone()),
                ..ProblemExtras::default()
            },
            Self::InvalidTargetHost { invalid_value } => ProblemExtras {
                invalid_value: Some(invalid_value.clone()),
                ..ProblemExtras::default()
            },
            Self::UnknownTargetHost {
                invalid_value,
                valid_hosts,
            } => ProblemExtras {
                invalid_value: Some(invalid_value.clone()),
                valid_hosts: Some(valid_hosts.clone()),
                ..ProblemExtras::default()
            },
            Self::PluginInUse(references) => ProblemExtras {
                referenced_by: Some(references.clone()),
                ..ProblemExtras::default()
            },
            Self::RateLimitExceeded(retry) => ProblemExtras {
                retry_after_seconds: Some(*retry),
                ..ProblemExtras::default()
            },
            _ => ProblemExtras::default(),
        }
    }
}

impl std::fmt::Display for OagwError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.status(), self.type_suffix())
    }
}

impl std::error::Error for OagwError {}

impl From<toolkit_http::HttpError> for OagwError {
    fn from(error: toolkit_http::HttpError) -> Self {
        use toolkit_http::HttpError;
        match error {
            HttpError::Timeout(_) => Self::RequestTimeout("upstream request timed out".to_owned()),
            HttpError::DeadlineExceeded(_) => {
                Self::RequestTimeout("upstream deadline exceeded".to_owned())
            }
            HttpError::Tls(_) => Self::LinkUnavailable("upstream TLS handshake failed".to_owned()),
            HttpError::InsecureTransport => Self::ProtocolError(
                "upstream scheme is not allowed by the transport policy".to_owned(),
            ),
            HttpError::InvalidScheme { .. } | HttpError::InvalidUri { .. } => {
                Self::ProtocolError("upstream target is not a reachable URL".to_owned())
            }
            HttpError::Overloaded | HttpError::ServiceClosed => {
                Self::LinkUnavailable("outbound transport is saturated".to_owned())
            }
            // Every remaining transport failure collapses into one surface so
            // no internal address, URL or header value reaches the caller.
            HttpError::RequestBuild(_)
            | HttpError::InvalidHeaderName(_)
            | HttpError::InvalidHeaderValue(_)
            | HttpError::Transport(_)
            | HttpError::BodyTooLarge { .. }
            | HttpError::HttpStatus { .. }
            | HttpError::Json(_)
            | HttpError::FormEncode(_)
            | _ => Self::DownstreamError("upstream transport failure".to_owned()),
        }
    }
}
