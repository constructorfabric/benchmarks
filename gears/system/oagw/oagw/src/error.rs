// Created: 2026-08-31 by Constructor Tech
//! Gateway error surface (DESIGN §3.3 error table + ADR-0007).
//!
//! Every error renders as an RFC 9457 `application/problem+json` document:
//!
//! ```json
//! {
//!   "type": "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
//!   "title": "Validation Error",
//!   "status": 400,
//!   "detail": "alias is required for IP-based endpoints",
//!   "instance": "/oagw/v1/upstreams",
//!   "trace_id": "01J..."
//! }
//! ```
//!
//! Extension members (`upstream_id`, `alias`, `host`, `path`,
//! `retry_after_seconds`, `trace_id`, `instance`, `plugin_id`,
//! `referenced_by`, …) are omitted when unset. The response always carries
//! `X-OAGW-Error-Source: gateway` (ADR-0007).
//!
//! Note: the body intentionally has **no** `context` member — the toolkit's
//! canonical-error middleware only rewrites bodies it can parse into a
//! canonical `Problem`, and OAGW owns its wire shape.

use std::fmt;

use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use uuid::Uuid;

/// Header distinguishing gateway-generated from upstream-passthrough errors.
pub const ERROR_SOURCE_HEADER: &str = "x-oagw-error-source";
/// Value of [`ERROR_SOURCE_HEADER`] for errors OAGW generated itself.
pub const ERROR_SOURCE_GATEWAY: &str = "gateway";
/// Value of [`ERROR_SOURCE_HEADER`] for errors passed through from an upstream.
pub const ERROR_SOURCE_UPSTREAM: &str = "upstream";

const PROBLEM_JSON: &str = "application/problem+json";

/// GTS prefix shared by every OAGW error type id (DESIGN §3.3).
const ERR_PREFIX: &str = "gts.cf.core.errors.err.v1~cf.oagw.";

/// Resource kinds addressable through the management API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    /// Upstream record.
    Upstream,
    /// Route record.
    Route,
    /// Plugin record.
    Plugin,
}

impl ResourceKind {
    /// Human-readable resource label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            ResourceKind::Upstream => "Upstream",
            ResourceKind::Route => "Route",
            ResourceKind::Plugin => "Plugin",
        }
    }
}

impl fmt::Display for ResourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Resources that reference a plugin, as reported by `plugin.in_use`.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct ReferencedBy {
    /// Upstream ids referencing the plugin.
    pub upstreams: Vec<String>,
    /// Route ids referencing the plugin.
    pub routes: Vec<String>,
}

impl ReferencedBy {
    /// Total number of referencing resources.
    #[must_use]
    pub fn total(&self) -> usize {
        self.upstreams.len() + self.routes.len()
    }
}

/// State of the token bucket a request was scored against (ADR-0003).
///
/// Carried by a `rate_limit.exceeded.v1` problem so the rendered response can
/// carry the standard `X-RateLimit-*` headers as well as the `Retry-After` one.
/// It is deliberately **not** a problem body member: the quota is transport
/// metadata, and RFC 6585 clients read it from the headers.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RateLimitSnapshot {
    /// Effective sustained rate per window.
    pub limit: u64,
    /// Tokens left in the bucket, floored.
    pub remaining: u64,
    /// Epoch seconds at which the bucket is full again.
    pub reset: u64,
}

/// Optional RFC 9457 extension members carried by an OAGW error.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct ProblemExtensions {
    /// Upstream the failure relates to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_id: Option<String>,
    /// Upstream alias the failure relates to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Host the failure relates to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Request path the failure relates to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Retry guidance (mirrored onto the `Retry-After` header).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u64>,
    /// Distributed tracing correlation id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// URI reference identifying this occurrence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    /// Plugin the failure relates to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin_id: Option<String>,
    /// Resources that reference [`ProblemExtensions::plugin_id`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub referenced_by: Option<ReferencedBy>,
    /// Endpoint hosts that would satisfy the request (ADR-0007).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_hosts: Option<Vec<String>>,
    /// Rejected value (ADR-0007).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invalid_value: Option<String>,
    /// Machine-readable code of a plugin rejection (ADR-0009).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// Header whose absence rejected a phase (ADR-0009 `required_headers`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub missing_header: Option<String>,
    /// Quota state of a rate-limited request, rendered as headers only
    /// (ADR-0003).
    #[serde(skip)]
    pub rate_limit: Option<RateLimitSnapshot>,
    /// CORS headers the answer to this failure must carry, rendered as headers
    /// only (ADR-0004). A refused origin carries the `Vary` alone and never an
    /// allow-origin, because naming it would tell the browser the opposite of
    /// what the answer says.
    #[serde(skip)]
    pub cors_headers: Vec<(String, String)>,
}

/// Every row of the DESIGN §3.3 error table, plus the management-only
/// `NotFound` family used by the CRUD endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum OagwErrorKind {
    /// General route validation error (400).
    #[error("validation error")]
    Validation,
    /// `X-OAGW-Target-Host` required but absent (400).
    #[error("missing target host")]
    MissingTargetHost,
    /// `X-OAGW-Target-Host` format invalid (400).
    #[error("invalid target host")]
    InvalidTargetHost,
    /// `X-OAGW-Target-Host` matches no configured endpoint (400).
    #[error("unknown target host")]
    UnknownTargetHost,
    /// Authentication to the upstream failed (401).
    #[error("authentication failed")]
    AuthenticationFailed,
    /// Referenced secret is unavailable (500).
    #[error("secret not found")]
    SecretNotFound,
    /// Protocol-level failure (502).
    #[error("protocol error")]
    ProtocolError,
    /// Upstream returned an error (502).
    #[error("downstream error")]
    DownstreamError,
    /// Streaming connection aborted (502).
    #[error("stream aborted")]
    StreamAborted,
    /// Upstream link unavailable (503).
    #[error("link unavailable")]
    LinkUnavailable,
    /// Circuit breaker is open (503).
    #[error("circuit breaker open")]
    CircuitBreakerOpen,
    /// Referenced plugin cannot be resolved (503).
    ///
    /// Shares the `plugin.not_found.v1` GTS id with the 404
    /// [`OagwErrorKind::NotFound`] case of a plugin; the HTTP status
    /// distinguishes "the record is gone" from "this tenant cannot bind it".
    #[error("plugin not found")]
    PluginNotFound,
    /// Connection to the upstream timed out (504).
    #[error("connection timeout")]
    ConnectionTimeout,
    /// Upstream request timed out (504).
    #[error("request timeout")]
    RequestTimeout,
    /// Idle stream timed out (504).
    #[error("idle timeout")]
    IdleTimeout,
    /// Rate limit exceeded (429).
    #[error("rate limit exceeded")]
    RateLimitExceeded,
    /// Cross-origin request from an origin the upstream does not allow (403).
    ///
    /// ADR-0004 "Error Responses" defines the two `cors.*` GTS ids itself;
    /// DESIGN §3.3 has no `cors.*` rows, so the ADR is the authority here.
    #[error("cors origin not allowed")]
    CorsOriginNotAllowed,
    /// Cross-origin request with a method the upstream does not allow (403).
    ///
    /// Same ADR-0004 provenance as [`OagwErrorKind::CorsOriginNotAllowed`].
    #[error("cors method not allowed")]
    CorsMethodNotAllowed,
    /// Request payload exceeds the configured limit (413).
    #[error("payload too large")]
    PayloadTooLarge,
    /// Plugin is still referenced by an upstream or route (409).
    #[error("plugin in use")]
    PluginInUse,
    /// Alias already taken within the tenant (409).
    #[error("alias conflict")]
    AliasConflict,
    /// Route match rule duplicates an existing route (409).
    #[error("route conflict")]
    RouteConflict,
    /// Plugin name already taken within the tenant (409).
    #[error("plugin conflict")]
    PluginConflict,
    /// Management resource does not exist (or is invisible to the caller).
    ///
    /// For a plugin this kind and [`OagwErrorKind::PluginNotFound`] share the
    /// `plugin.not_found.v1` GTS id (DESIGN §3.3): a plugin absent as a CRUD
    /// resource is 404, a plugin a binding cannot resolve is 503. The HTTP
    /// status is the discriminator — the slug must stay identical because the
    /// wire contract defines a single problem type per table row.
    #[error("resource not found")]
    NotFound,
    /// Unexpected control-plane failure (500).
    #[error("internal error")]
    Internal,
}

impl OagwErrorKind {
    /// HTTP status for this error kind (DESIGN §3.3).
    #[must_use]
    pub fn status(self) -> u16 {
        match self {
            OagwErrorKind::Validation
            | OagwErrorKind::MissingTargetHost
            | OagwErrorKind::InvalidTargetHost
            | OagwErrorKind::UnknownTargetHost => 400,
            OagwErrorKind::AuthenticationFailed => 401,
            OagwErrorKind::NotFound => 404,
            OagwErrorKind::AliasConflict
            | OagwErrorKind::RouteConflict
            | OagwErrorKind::PluginConflict
            | OagwErrorKind::PluginInUse => 409,
            OagwErrorKind::PayloadTooLarge => 413,
            OagwErrorKind::RateLimitExceeded => 429,
            OagwErrorKind::CorsOriginNotAllowed | OagwErrorKind::CorsMethodNotAllowed => 403,
            OagwErrorKind::SecretNotFound | OagwErrorKind::Internal => 500,
            OagwErrorKind::ProtocolError
            | OagwErrorKind::DownstreamError
            | OagwErrorKind::StreamAborted => 502,
            OagwErrorKind::LinkUnavailable
            | OagwErrorKind::CircuitBreakerOpen
            | OagwErrorKind::PluginNotFound => 503,
            OagwErrorKind::ConnectionTimeout
            | OagwErrorKind::RequestTimeout
            | OagwErrorKind::IdleTimeout => 504,
        }
    }

    /// RFC 9457 `title` for this error kind.
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            OagwErrorKind::Validation
            | OagwErrorKind::MissingTargetHost
            | OagwErrorKind::InvalidTargetHost
            | OagwErrorKind::UnknownTargetHost => "Validation Error",
            OagwErrorKind::AuthenticationFailed => "Authentication Failed",
            OagwErrorKind::NotFound => "Not Found",
            OagwErrorKind::AliasConflict
            | OagwErrorKind::RouteConflict
            | OagwErrorKind::PluginConflict => "Conflict",
            OagwErrorKind::PluginInUse => "Plugin In Use",
            OagwErrorKind::PayloadTooLarge => "Payload Too Large",
            OagwErrorKind::RateLimitExceeded => "Rate Limit Exceeded",
            OagwErrorKind::CorsOriginNotAllowed => "CORS Origin Not Allowed",
            OagwErrorKind::CorsMethodNotAllowed => "CORS Method Not Allowed",
            OagwErrorKind::SecretNotFound => "Secret Not Found",
            OagwErrorKind::Internal => "Internal Error",
            OagwErrorKind::ProtocolError => "Protocol Error",
            OagwErrorKind::DownstreamError => "Downstream Error",
            OagwErrorKind::StreamAborted => "Stream Aborted",
            OagwErrorKind::LinkUnavailable => "Link Unavailable",
            OagwErrorKind::CircuitBreakerOpen => "Circuit Breaker Open",
            OagwErrorKind::PluginNotFound => "Plugin Not Found",
            OagwErrorKind::ConnectionTimeout => "Connection Timeout",
            OagwErrorKind::RequestTimeout => "Request Timeout",
            OagwErrorKind::IdleTimeout => "Idle Timeout",
        }
    }

    /// GTS type id for this error kind (DESIGN §3.3).
    ///
    /// `NotFound` is parameterised by the missing resource kind. A plugin
    /// appears twice in the table (404 resource-absent, 503 reference-
    /// unresolved) and both rows share the `plugin.not_found.v1` slug; the
    /// HTTP status is the discriminator.
    #[must_use]
    pub fn gts_type(self, kind: ResourceKind) -> String {
        let slug = match self {
            OagwErrorKind::Validation => "validation.error.v1",
            OagwErrorKind::MissingTargetHost => "routing.missing_target_host.v1",
            OagwErrorKind::InvalidTargetHost => "routing.invalid_target_host.v1",
            OagwErrorKind::UnknownTargetHost => "routing.unknown_target_host.v1",
            OagwErrorKind::AuthenticationFailed => "auth.failed.v1",
            OagwErrorKind::NotFound => match kind {
                ResourceKind::Upstream => "upstream.not_found.v1",
                ResourceKind::Route => "route.not_found.v1",
                ResourceKind::Plugin => "plugin.not_found.v1",
            },
            OagwErrorKind::AliasConflict => "alias.conflict.v1",
            OagwErrorKind::RouteConflict => "route.conflict.v1",
            OagwErrorKind::PluginConflict => "plugin.conflict.v1",
            OagwErrorKind::PluginInUse => "plugin.in_use.v1",
            OagwErrorKind::PayloadTooLarge => "payload.too_large.v1",
            OagwErrorKind::RateLimitExceeded => "rate_limit.exceeded.v1",
            // ADR-0004 "Error Responses" fixes both slugs; the DESIGN §3.3
            // table has no cors row for them.
            OagwErrorKind::CorsOriginNotAllowed => "cors.origin_not_allowed.v1",
            OagwErrorKind::CorsMethodNotAllowed => "cors.method_not_allowed.v1",
            OagwErrorKind::SecretNotFound => "secret.not_found.v1",
            OagwErrorKind::Internal => "internal.error.v1",
            OagwErrorKind::ProtocolError => "protocol.error.v1",
            OagwErrorKind::DownstreamError => "downstream.error.v1",
            OagwErrorKind::StreamAborted => "stream.aborted.v1",
            OagwErrorKind::LinkUnavailable => "link.unavailable.v1",
            OagwErrorKind::CircuitBreakerOpen => "circuit_breaker.open.v1",
            OagwErrorKind::PluginNotFound => "plugin.not_found.v1",
            OagwErrorKind::ConnectionTimeout => "timeout.connection.v1",
            OagwErrorKind::RequestTimeout => "timeout.request.v1",
            OagwErrorKind::IdleTimeout => "timeout.idle.v1",
        };
        format!("{ERR_PREFIX}{slug}")
    }
}

/// A gateway error: [`OagwErrorKind`] plus its human detail and extensions.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{detail}")]
pub struct OagwError {
    kind: OagwErrorKind,
    /// Resource kind, only meaningful for [`OagwErrorKind::NotFound`].
    resource: ResourceKind,
    detail: String,
    ext: Box<ProblemExtensions>,
}

impl OagwError {
    /// Build an error for `kind` with `detail`.
    #[must_use]
    pub fn new(kind: OagwErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            resource: ResourceKind::Upstream,
            detail: detail.into(),
            ext: Box::default(),
        }
    }

    /// 400 validation failure.
    #[must_use]
    pub fn validation(detail: impl Into<String>) -> Self {
        Self::new(OagwErrorKind::Validation, detail)
    }

    /// Management 404 for `kind` resources.
    #[must_use]
    pub fn not_found(kind: ResourceKind, id: Uuid) -> Self {
        let detail = format!(
            "{} resource '{id}' was not found for the calling tenant",
            kind.label()
        );
        let mut err = Self::new(OagwErrorKind::NotFound, detail);
        err.resource = kind;
        err
    }

    /// Report this 404 as a missing `kind`.
    ///
    /// The data plane has a single 404 contract (DESIGN §3.3
    /// `route.not_found.v1`), whatever lookup failed to produce it.
    #[must_use]
    pub const fn with_resource(mut self, kind: ResourceKind) -> Self {
        self.resource = kind;
        self
    }

    /// 409 `plugin.in_use` (ADR-0001 "Plugin Deletion Behavior").
    #[must_use]
    pub fn plugin_in_use(plugin_id: Uuid, referenced_by: ReferencedBy) -> Self {
        let detail = format!(
            "Plugin is referenced by {} upstream(s) and {} route(s)",
            referenced_by.upstreams.len(),
            referenced_by.routes.len()
        );
        let mut err = Self::new(OagwErrorKind::PluginInUse, detail);
        err.ext.plugin_id = Some(plugin_id.to_string());
        err.ext.referenced_by = Some(referenced_by);
        err
    }

    /// 409 alias conflict within the calling tenant.
    #[must_use]
    pub fn alias_conflict(alias: &str, existing_id: Uuid) -> Self {
        let detail = format!("An upstream with alias '{alias}' already exists");
        let mut err = Self::new(OagwErrorKind::AliasConflict, detail);
        err.ext.alias = Some(alias.to_owned());
        err.ext.upstream_id = Some(existing_id.to_string());
        err
    }

    /// 409 duplicate route match rule within the upstream.
    #[must_use]
    pub fn route_conflict(detail: impl Into<String>, upstream_id: Uuid) -> Self {
        let mut err = Self::new(OagwErrorKind::RouteConflict, detail);
        err.ext.upstream_id = Some(upstream_id.to_string());
        err
    }

    /// 409 duplicate plugin name within the calling tenant.
    #[must_use]
    pub fn plugin_conflict(name: &str, existing_id: Uuid) -> Self {
        let detail = format!("A plugin named '{name}' already exists");
        let mut err = Self::new(OagwErrorKind::PluginConflict, detail);
        err.ext.plugin_id = Some(existing_id.to_string());
        err
    }

    /// 413 payload too large.
    #[must_use]
    pub fn payload_too_large(limit: u64, actual: u64) -> Self {
        Self::new(
            OagwErrorKind::PayloadTooLarge,
            format!("Request body of {actual} bytes exceeds the limit of {limit} bytes"),
        )
    }

    /// Attach an extension member.
    #[must_use]
    pub fn with_extension(mut self, apply: impl FnOnce(&mut ProblemExtensions)) -> Self {
        apply(&mut self.ext);
        self
    }

    /// Attach the CORS headers the answer to this failure must carry.
    ///
    /// The first set wins: a CORS refusal already carries the `Vary`-only
    /// answer the ADR asks for, and a later attach of the permissive set of an
    /// *allowed* origin would tell the browser the opposite of what happened.
    #[must_use]
    pub fn with_cors_headers(mut self, headers: &HeaderMap) -> Self {
        if self.ext.cors_headers.is_empty() && !headers.is_empty() {
            self.ext.cors_headers = headers
                .iter()
                .filter_map(|(name, value)| {
                    let value = value.to_str().ok()?;
                    Some((name.as_str().to_owned(), value.to_owned()))
                })
                .collect();
        }
        self
    }

    /// Kind of this error.
    #[must_use]
    pub const fn kind(&self) -> &OagwErrorKind {
        &self.kind
    }

    /// HTTP status code.
    #[must_use]
    pub fn status(&self) -> u16 {
        self.kind.status()
    }

    /// GTS type id (DESIGN §3.3).
    #[must_use]
    pub fn gts_type(&self) -> String {
        self.kind.gts_type(self.resource)
    }

    /// RFC 9457 `title`.
    #[must_use]
    pub fn title(&self) -> &'static str {
        self.kind.title()
    }

    /// Human-readable detail.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }

    /// Extension members.
    #[must_use]
    pub const fn extensions(&self) -> &ProblemExtensions {
        &self.ext
    }

    /// RFC 9457 document for this error.
    #[must_use]
    pub fn problem(&self) -> ProblemBody {
        ProblemBody {
            problem_type: self.gts_type(),
            title: self.title(),
            status: self.status(),
            detail: self.detail.clone(),
            extensions: (*self.ext).clone(),
        }
    }

    /// Whether the error is retryable per DESIGN §3.3.
    #[must_use]
    pub const fn retryable(&self) -> bool {
        matches!(
            self.kind,
            OagwErrorKind::RateLimitExceeded
                | OagwErrorKind::LinkUnavailable
                | OagwErrorKind::CircuitBreakerOpen
                | OagwErrorKind::ConnectionTimeout
                | OagwErrorKind::RequestTimeout
                | OagwErrorKind::IdleTimeout
        )
    }
}

/// RFC 9457 `application/problem+json` document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProblemBody {
    /// GTS type identifier.
    #[serde(rename = "type")]
    pub problem_type: String,
    /// Human-readable summary.
    pub title: &'static str,
    /// HTTP status code.
    pub status: u16,
    /// Human-readable explanation for this occurrence.
    pub detail: String,
    /// OAGW extension members.
    #[serde(flatten)]
    pub extensions: ProblemExtensions,
}

impl IntoResponse for OagwError {
    fn into_response(self) -> Response {
        let body = self.problem();
        let status = StatusCode::from_u16(body.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let payload = match serde_json::to_vec(&body) {
            Ok(bytes) => bytes,
            Err(err) => {
                tracing::error!(error = %err, error_type = %body.problem_type, "problem rendering failed");
                Vec::new()
            }
        };

        let mut response = (status, payload).into_response();
        response
            .headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static(PROBLEM_JSON));
        response.headers_mut().insert(
            HeaderName::from_static(ERROR_SOURCE_HEADER),
            HeaderValue::from_static(ERROR_SOURCE_GATEWAY),
        );
        if let Some(retry_after) = self.ext.retry_after_seconds {
            response.headers_mut().insert(
                HeaderName::from_static("retry-after"),
                HeaderValue::from(retry_after),
            );
        }
        for (name, value) in &self.ext.cors_headers {
            if let Ok(name) = HeaderName::from_bytes(name.as_bytes())
                && let Ok(value) = HeaderValue::from_str(value)
            {
                response.headers_mut().insert(name, value);
            }
        }
        if let Some(snapshot) = self.ext.rate_limit {
            let headers = [
                ("x-ratelimit-limit", snapshot.limit),
                ("x-ratelimit-remaining", snapshot.remaining),
                ("x-ratelimit-reset", snapshot.reset),
            ];
            for (name, value) in headers {
                response
                    .headers_mut()
                    .insert(HeaderName::from_static(name), HeaderValue::from(value));
            }
        }
        response
    }
}

/// Map an alias decision rejection onto the validation problem (400).
impl From<crate::domain::alias::AliasRejection> for OagwError {
    fn from(rejection: crate::domain::alias::AliasRejection) -> Self {
        let alias = match &rejection {
            crate::domain::alias::AliasRejection::ChangeRejected { existing, .. } => {
                Some(existing.clone())
            }
            _ => None,
        };
        let detail = rejection.detail();
        let error = OagwError::validation(detail);
        match alias {
            Some(alias) => error.with_extension(|ext| ext.alias = Some(alias)),
            None => error,
        }
    }
}

/// Convenience alias for handlers returning an OAGW error.
pub type OagwResult<T> = Result<T, OagwError>;

#[cfg(test)]
mod tests {
    use super::{OagwError, OagwErrorKind, ResourceKind};

    /// One GTS id, two HTTP statuses: the slug is shared by the 404
    /// resource-absent case and the 503 reference-unresolved case, so only the
    /// status can tell them apart (DESIGN §3.3).
    #[test]
    fn the_plugin_not_found_slug_is_shared_by_both_statuses() {
        let absent = OagwError::not_found(ResourceKind::Plugin, uuid::Uuid::new_v4());
        let unresolved = OagwError::new(OagwErrorKind::PluginNotFound, "reference");
        assert_eq!(absent.gts_type(), unresolved.gts_type());
        assert_eq!(absent.status(), 404);
        assert_eq!(unresolved.status(), 503);
        assert!(absent.gts_type().ends_with("plugin.not_found.v1"));
    }
}
