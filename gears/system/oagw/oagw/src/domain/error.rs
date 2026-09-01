//! Domain error types for the OAGW gear.
//!
//! Two error enum families live here:
//!
//! * [`DomainError`] — control-plane failures (management CRUD). Mapped to
//!   RFC 9457 problems by [`crate::api::rest::error`].
//! * [`ProxyError`] — data-plane failures, carrying the request context
//!   (`upstream_id`, `host`, `path`) demanded of every gateway error by the
//!   DESIGN error table.

use std::time::Duration;

use thiserror::Error;

/// Control-plane domain error.
#[derive(Debug, Error)]
pub enum DomainError {
    /// The referenced resource does not exist in the calling tenant's scope.
    #[error("resource not found")]
    NotFound,
    /// A uniqueness or lifecycle constraint was violated.
    #[error("conflict: {detail}")]
    Conflict { detail: String },
    /// The request payload failed validation.
    #[error("validation error: {detail}")]
    Validation { detail: String },
    /// The caller lacks a management permission for this operation.
    #[error("access denied: {detail}")]
    AccessDenied { detail: String },
    /// A plugin is still referenced by an upstream or route.
    #[error("plugin in use")]
    PluginInUse,
    /// Any internal failure. `diagnostic` must never contain secrets.
    #[error("internal error")]
    Internal { diagnostic: String },
}

impl DomainError {
    /// Create an internal error from an arbitrary message.
    #[must_use]
    pub fn internal(diagnostic: impl Into<String>) -> Self {
        Self::Internal {
            diagnostic: diagnostic.into(),
        }
    }

    /// Create a validation error.
    #[must_use]
    pub fn validation(detail: impl Into<String>) -> Self {
        Self::Validation {
            detail: detail.into(),
        }
    }
}

/// Shared context fields for data-plane errors (DESIGN "extension fields").
#[derive(Debug, Clone, Default)]
pub struct ProxyContext {
    /// GTS instance id of the resolved upstream, when known.
    pub upstream_id: Option<String>,
    /// Target host, when known.
    pub host: Option<String>,
    /// The proxy path suffix, when known.
    pub path: Option<String>,
}

/// Data-plane (proxy) error.
///
/// Every variant maps to exactly one GTS error instance id — see the table in
/// `crate::api::rest::error::OagwProblem::from_proxy_error`.
#[derive(Debug, Error)]
pub enum ProxyError {
    /// 404 — no upstream resolved for the alias or no route matched.
    #[error("route not found")]
    RouteNotFound {
        /// Request context extension fields.
        context: ProxyContext,
    },
    /// 400 — general validation failure (body, headers, method, query).
    #[error("validation error: {detail}")]
    Validation {
        /// Extension fields.
        context: ProxyContext,
        /// Human-readable detail.
        detail: String,
    },
    /// 400 — multi-endpoint common-suffix pool without `X-OAGW-Target-Host`.
    #[error("X-OAGW-Target-Host is required for common-suffix pools")]
    MissingTargetHost {
        /// Extension fields.
        context: ProxyContext,
        /// Hosts that would have been valid.
        valid_hosts: Vec<String>,
    },
    /// 400 — `X-OAGW-Target-Host` is not a plain hostname or IP.
    #[error("invalid X-OAGW-Target-Host: {detail}")]
    InvalidTargetHost {
        /// Extension fields.
        context: ProxyContext,
        /// Parse failure detail.
        detail: String,
    },
    /// 400 — `X-OAGW-Target-Host` does not match any pool endpoint.
    #[error("unknown X-OAGW-Target-Host")]
    UnknownTargetHost {
        /// Extension fields.
        context: ProxyContext,
        /// Hosts that would have been valid.
        valid_hosts: Vec<String>,
    },
    /// 401 — an auth plugin could not obtain credentials for the upstream.
    #[error("authentication to upstream failed: {detail}")]
    AuthenticationFailed {
        /// Extension fields.
        context: ProxyContext,
        /// Plugin failure detail.
        detail: String,
    },
    /// 403 — CORS origin rejected on an actual request.
    #[error("cors origin not allowed: {origin}")]
    CorsOriginNotAllowed {
        /// Extension fields.
        context: ProxyContext,
        /// The offending origin.
        origin: String,
    },
    /// 403 — CORS method rejected on an actual request.
    #[error("cors method not allowed: {method}")]
    CorsMethodNotAllowed {
        /// Extension fields.
        context: ProxyContext,
        /// The offending method.
        method: String,
    },
    /// 413 — request body exceeded the 100 MB hard limit.
    #[error("payload too large")]
    PayloadTooLarge {
        /// Extension fields.
        context: ProxyContext,
    },
    /// 429 — rate limit exceeded.
    #[error("rate limit exceeded")]
    RateLimitExceeded {
        /// Extension fields.
        context: ProxyContext,
        /// Seconds until the bucket refills (for `Retry-After`).
        retry_after: Duration,
        /// Effective bucket capacity (for `X-RateLimit-Limit`).
        limit: u64,
        /// Tokens remaining after the rejected deduction (for
        /// `X-RateLimit-Remaining`).
        remaining: u64,
        /// Unix epoch seconds when the bucket resets (for `X-RateLimit-Reset`
        /// — `now + retry_after`).
        reset: u64,
    },
    /// 500 — a `cred://` secret referenced by an auth plugin is missing.
    #[error("referenced secret not found: {reference}")]
    SecretNotFound {
        /// Extension fields.
        context: ProxyContext,
        /// The unresolved `cred://` reference (never the material).
        reference: String,
    },
    /// 502 — protocol-level failure (malformed response, upgrade failure).
    #[error("protocol error: {detail}")]
    ProtocolError {
        /// Extension fields.
        context: ProxyContext,
        /// Protocol failure detail.
        detail: String,
    },
    /// 502 — generic upstream error.
    #[error("downstream error: {detail}")]
    DownstreamError {
        /// Extension fields.
        context: ProxyContext,
        /// Failure detail.
        detail: String,
    },
    /// 502 — a stream was aborted before completion.
    #[error("stream aborted: {detail}")]
    StreamAborted {
        /// Extension fields.
        context: ProxyContext,
        /// Abort detail.
        detail: String,
    },
    /// 503 — the selected upstream is disabled for this caller.
    #[error("upstream link unavailable")]
    LinkUnavailable {
        /// Extension fields.
        context: ProxyContext,
    },
    /// 503 — a bound plugin identifier is not resolvable in any registry.
    #[error("plugin not found: {plugin_ref}")]
    PluginNotFound {
        /// Extension fields.
        context: ProxyContext,
        /// The unresolvable plugin reference.
        plugin_ref: String,
    },
    /// 504 — the upstream connection phase timed out.
    #[error("connection timeout")]
    ConnectionTimeout {
        /// Extension fields.
        context: ProxyContext,
    },
    /// 504 — the total request budget timed out.
    #[error("request timeout after {timeout_secs}s")]
    RequestTimeout {
        /// Extension fields.
        context: ProxyContext,
        /// Configured proxy timeout in seconds.
        timeout_secs: u64,
    },
    /// 504 — idle timeout while streaming a response body.
    #[error("idle timeout")]
    IdleTimeout {
        /// Extension fields.
        context: ProxyContext,
    },
    /// 400 — gRPC proxy path requested but not implemented (Phase 3).
    #[error("grpc proxying is not implemented")]
    GrpcNotImplemented {
        /// Extension fields.
        context: ProxyContext,
    },
    /// 500 — internal data-plane failure. Never leak material.
    #[error("internal data plane error: {diagnostic}")]
    Internal {
        /// Extension fields.
        context: ProxyContext,
        /// Diagnostic (redacted).
        diagnostic: String,
    },
}

impl ProxyError {
    /// Attach upstream/host/path context when present.
    #[must_use]
    pub fn with_context(mut self, context: ProxyContext) -> Self {
        match &mut self {
            Self::RouteNotFound { context: c }
            | Self::Validation { context: c, .. }
            | Self::MissingTargetHost { context: c, .. }
            | Self::InvalidTargetHost { context: c, .. }
            | Self::UnknownTargetHost { context: c, .. }
            | Self::AuthenticationFailed { context: c, .. }
            | Self::CorsOriginNotAllowed { context: c, .. }
            | Self::CorsMethodNotAllowed { context: c, .. }
            | Self::PayloadTooLarge { context: c }
            | Self::RateLimitExceeded { context: c, .. }
            | Self::SecretNotFound { context: c, .. }
            | Self::ProtocolError { context: c, .. }
            | Self::DownstreamError { context: c, .. }
            | Self::StreamAborted { context: c, .. }
            | Self::LinkUnavailable { context: c }
            | Self::PluginNotFound { context: c, .. }
            | Self::ConnectionTimeout { context: c }
            | Self::RequestTimeout { context: c, .. }
            | Self::IdleTimeout { context: c }
            | Self::GrpcNotImplemented { context: c }
            | Self::Internal { context: c, .. } => {
                if c.upstream_id.is_none() {
                    c.upstream_id = context.upstream_id;
                }
                if c.host.is_none() {
                    c.host = context.host;
                }
                if c.path.is_none() {
                    c.path = context.path;
                }
            }
        }
        self
    }

    /// Convenience constructor for a validation error.
    #[must_use]
    pub fn validation(detail: impl Into<String>) -> Self {
        Self::Validation {
            context: ProxyContext::default(),
            detail: detail.into(),
        }
    }

    /// Convenience constructor for an internal error.
    #[must_use]
    pub fn internal(context: ProxyContext, diagnostic: impl Into<String>) -> Self {
        Self::Internal {
            context,
            diagnostic: diagnostic.into(),
        }
    }
}
