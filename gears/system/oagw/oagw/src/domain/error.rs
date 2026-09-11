//! `DomainError` — the single error taxonomy carried across both planes and
//! across the repository boundary (DoD `cpt-cf-oagw-dod-gear-foundation-domain-error`).
//!
//! **Scope of this entry.** The taxonomy and its context fields only. The
//! mapping from a variant onto the HTTP status, the GTS error type, the
//! retriable flag and the extension-field set of the DESIGN §3.3 table is
//! owned by `api/rest/error.rs` (entry 2.5), which is the *single* mapping
//! layer; no variant here renders an HTTP body, and no endpoint maps an error
//! locally.
//!
//! Two repository-boundary variants (`NotFound`, `Conflict`) are added to the
//! DESIGN §3.3 table: the table is a *proxy-path* contract, while
//! `cpt-cf-oagw-algo-gear-foundation-repo-scope` requires the store to
//! distinguish not-found for a foreign-tenant or missing key from conflict for
//! a uniqueness violation. They map onto the same `404` / `409` statuses as
//! `RouteNotFound` and `PluginInUse` respectively.
//!
//! # Credential isolation
//!
//! No variant may ever echo a rejected credential value. Validation callers
//! name the offending *field* and never interpolate the offending *value*
//! (see [`crate::config`]); the `detail: String` fields below therefore carry
//! caller-curated text only. [`DomainError::redacted_detail`] is the single
//! place a `detail` is surfaced from, and it is what entry 2.5 serializes.

/// Plugin-in-use reference set, carried only by
/// `gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1` (409).
///
/// `Serialize` is the only wire concern this type carries: the reference
/// identifiers are the resource instance identifiers the scan found, and
/// entry 2.5 emits them as the `referenced_by` problem-details member in the
/// shape the plugin delete flow requires.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReferencedBy {
    /// Upstreams whose `plugins.items[]` or `auth.type` reference the plugin.
    pub upstreams: Vec<String>,
    /// Routes whose `plugins.items[]` reference the plugin.
    pub routes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum DomainError {
    /// 400 `validation.error.v1`. The canonical write-path rejection; `detail`
    /// names the offending field but never echoes a rejected credential value.
    #[error("validation failed: {detail}")]
    ValidationError {
        detail: String,
        path: Option<String>,
        trace_id: Option<String>,
    },

    /// 400 `validation.error.v1` — a **distinct** variant carrying route
    /// context (failing method, path prefix, or match rule); it maps onto the
    /// same GTS type and status as [`DomainError::ValidationError`].
    #[error("route rejected: {detail}")]
    RouteError {
        detail: String,
        method: Option<String>,
        path_prefix: Option<String>,
        match_rule: Option<String>,
    },

    /// 400 `routing.missing_target_host.v1` — `X-OAGW-Target-Host` required
    /// for a multi-endpoint upstream with a common-suffix alias, absent.
    #[error("missing target host header")]
    MissingTargetHost {
        upstream_id: Option<String>,
        alias: Option<String>,
        valid_hosts: Vec<String>,
        trace_id: Option<String>,
    },

    /// 400 `routing.invalid_target_host.v1` — `X-OAGW-Target-Host` present but
    /// malformed. `invalid_value` is the truncated, control-character-free
    /// echo of the request header value only.
    #[error("invalid target host header")]
    InvalidTargetHost {
        upstream_id: Option<String>,
        invalid_value: String,
        trace_id: Option<String>,
    },

    /// 400 `routing.unknown_target_host.v1` — a well-formed target host the
    /// upstream pool does not contain.
    #[error("unknown target host")]
    UnknownTargetHost {
        upstream_id: Option<String>,
        invalid_value: String,
        valid_hosts: Vec<String>,
        trace_id: Option<String>,
    },

    /// 401 `auth.failed.v1` — used for both inbound bearer authentication
    /// failures and the outbound upstream-auth row of the DESIGN table.
    #[error("authentication failed")]
    AuthenticationFailed {
        upstream_id: Option<String>,
        host: Option<String>,
        path: Option<String>,
        trace_id: Option<String>,
    },

    /// 404 `route.not_found.v1` — no enabled route matched on the proxy path.
    #[error("route not found")]
    RouteNotFound {
        path: Option<String>,
        trace_id: Option<String>,
    },

    /// 404 `route.not_found.v1` — repository-level not-found for a
    /// missing **or foreign-tenant** key; existence is never disclosed.
    #[error("not found: {resource_type}")]
    NotFound { resource_type: &'static str },

    /// 409 `plugin.in_use.v1` — a uniqueness violation, or a delete of a
    /// record that is still referenced.
    #[error("conflict: {detail}")]
    Conflict {
        detail: String,
        referenced_by: Option<ReferencedBy>,
    },

    /// 409 `plugin.in_use.v1` — typed shape of the plugin delete-in-use
    /// outcome; `referenced_by` is the only carrier of that member.
    #[error("plugin is in use")]
    PluginInUse { referenced_by: ReferencedBy },

    /// 413 `payload.too_large.v1` — never carries `Retry-After`. `limit_bytes`
    /// is the configured limit the `detail` states; no request body content is
    /// ever carried.
    #[error("payload too large")]
    PayloadTooLarge {
        path: Option<String>,
        trace_id: Option<String>,
        upstream_id: Option<String>,
        limit_bytes: Option<u64>,
    },

    /// 429 `rate_limit.exceeded.v1` — retriable; `retry_after_seconds` and
    /// the `Retry-After` header are emitted together.
    #[error("rate limit exceeded")]
    RateLimitExceeded {
        upstream_id: Option<String>,
        host: Option<String>,
        retry_after_seconds: Option<u64>,
        trace_id: Option<String>,
    },

    /// 500 `secret.not_found.v1` — a `cred://` reference could not be
    /// resolved; no secret material ever reaches the rendered body.
    #[error("secret not found")]
    SecretNotFound {
        path: Option<String>,
        trace_id: Option<String>,
    },

    /// 502 `protocol.error.v1`
    #[error("protocol error")]
    ProtocolError {
        upstream_id: Option<String>,
        host: Option<String>,
        path: Option<String>,
        trace_id: Option<String>,
    },

    /// 502 `downstream.error.v1` — retriable *per occurrence*, decided by the
    /// producer of the error, which is why the flag travels on the variant.
    #[error("downstream error")]
    DownstreamError {
        upstream_id: Option<String>,
        host: Option<String>,
        path: Option<String>,
        trace_id: Option<String>,
        retriable: bool,
    },

    /// 502 `stream.aborted.v1` — after headers were sent this is attribution
    /// only: no status or body change.
    #[error("stream aborted")]
    StreamAborted {
        upstream_id: Option<String>,
        host: Option<String>,
        path: Option<String>,
        trace_id: Option<String>,
    },

    /// 503 `link.unavailable.v1` — also the outcome for a disabled upstream.
    /// Retriable, but carries neither `retry_after_seconds` nor `Retry-After`.
    #[error("upstream link unavailable")]
    LinkUnavailable {
        upstream_id: Option<String>,
        host: Option<String>,
        path: Option<String>,
        trace_id: Option<String>,
    },

    /// 503 `circuit_breaker.open.v1` — **declared, not implemented** (graded
    /// deviation 9); only its metric names are in scope.
    #[error("circuit breaker open")]
    CircuitBreakerOpen {
        upstream_id: Option<String>,
        host: Option<String>,
        trace_id: Option<String>,
    },

    /// 503 `plugin.not_found.v1` — a composed chain reference that does not
    /// resolve before execution.
    #[error("plugin not found: {plugin_ref}")]
    PluginNotFound { plugin_ref: String },

    /// 504 `timeout.connection.v1` — guidance is `proxy_timeout_secs`.
    #[error("connection timeout")]
    ConnectionTimeout {
        upstream_id: Option<String>,
        host: Option<String>,
        guidance_secs: Option<u64>,
        trace_id: Option<String>,
    },

    /// 504 `timeout.request.v1`
    #[error("request timeout")]
    RequestTimeout {
        upstream_id: Option<String>,
        host: Option<String>,
        guidance_secs: Option<u64>,
        trace_id: Option<String>,
    },

    /// 504 `timeout.idle.v1` — the only timeout an `sse`/`ws`/`wt` session is
    /// bounded by.
    #[error("idle timeout")]
    IdleTimeout {
        upstream_id: Option<String>,
        host: Option<String>,
        guidance_secs: Option<u64>,
        trace_id: Option<String>,
    },

    /// CORS origin rejection (entry 2.8); 403, non-retriable, `Vary: Origin`.
    #[error("CORS origin not allowed")]
    CorsOriginNotAllowed {
        path: Option<String>,
        trace_id: Option<String>,
    },

    /// CORS method rejection (entry 2.8); 403, non-retriable, `Vary: Origin`.
    #[error("CORS method not allowed")]
    CorsMethodNotAllowed {
        path: Option<String>,
        trace_id: Option<String>,
    },

    /// `CorsError::InvalidConfig` of ADR 0004, re-checked on the merged
    /// effective configuration (wildcard origin with credentials).
    #[error("invalid CORS configuration: {0}")]
    CorsInvalidConfig(String),

    /// A plugin failed internally (ADR 0008). Never carries credential
    /// material.
    #[error("plugin failure: {0}")]
    PluginInternal(String),

    /// An invariant violation that is not attributable to the caller.
    #[error("internal error: {0}")]
    Internal(String),
}

impl DomainError {
    /// A validation rejection naming `field` and never echoing `value`.
    ///
    /// `cpt-cf-oagw-algo-gear-foundation-credential-boundary` step 3: the
    /// rejection "names the field but never echoes the rejected value".
    #[must_use]
    pub fn field_rejection(field: &str, reason: &str) -> Self {
        Self::ValidationError {
            detail: format!("field `{field}` rejected: {reason}"),
            path: None,
            trace_id: None,
        }
    }

    /// The curated text entry 2.5 renders as problem+json `detail`.
    ///
    /// Returns `None` for variants that carry no `detail` of their own; those
    /// fall back to the mapped `title` of the GTS type. Credential material
    /// can never reach this string: no variant stores a credential value, and
    /// the `invalid_value` echo is bounded by
    /// [`crate::domain::validation::bound_invalid_value`].
    #[must_use]
    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::ValidationError { detail, .. }
            | Self::RouteError { detail, .. }
            | Self::Conflict { detail, .. }
            | Self::CorsInvalidConfig(detail)
            | Self::PluginInternal(detail)
            | Self::Internal(detail) => Some(detail),
            _ => None,
        }
    }

    /// Whether the variant is a not-found outcome (foreign-tenant keys and
    /// missing keys are indistinguishable at this boundary).
    #[must_use]
    pub const fn is_not_found(&self) -> bool {
        matches!(self, Self::NotFound { .. } | Self::RouteNotFound { .. })
    }

    /// Whether the variant is a conflict (uniqueness violation or in-use).
    #[must_use]
    pub const fn is_conflict(&self) -> bool {
        matches!(self, Self::Conflict { .. } | Self::PluginInUse { .. })
    }
}

/// A credential-bearing value is never stored in, logged from, or rendered
/// from a `DomainError`. This compile-time guard documents the invariant that
/// the error text is the only free-form surface a rejection reaches.
#[must_use]
pub fn redacted_detail(error: &DomainError) -> Option<&str> {
    error.detail()
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod tests;
