//! The OAGW wire error envelope and the `DomainError` mapping.
//!
//! `DESIGN` §3.3 mandates that every gateway error be an RFC 9457 problem
//! whose `type` is the GTS instance id tabulated for the failure, and that the
//! response carry `X-OAGW-Error-Source: gateway` ([`ADR`-0007]). The platform
//! `CanonicalError` builders are `pub(crate)` to `toolkit-canonical-errors`
//! and hard-code the `cf.core.err.*` category ids, so the `cf.oagw.*` ids are
//! unreachable through it; this module therefore owns its own problem type
//! with the mandated extensions (`upstream_id`, `alias`, `host`, `path`,
//! `plugin_id`, `referenced_by`, `valid_hosts`, `invalid_value`,
//! `retry_after_seconds`, `trace_id`).
//!
//! The rarely populated extension fields live behind a [`Box`] and are
//! flattened into the top level of the body, so a problem stays cheap to move
//! through the handlers that never add one.

use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::domain::error::{DomainError, OagwErrorType};
use crate::domain::model::canonical_types;

/// Name of the header that distinguishes gateway from upstream errors.
pub const ERROR_SOURCE_HEADER: &str = "X-OAGW-Error-Source";

/// Value of [`ERROR_SOURCE_HEADER`] for errors produced by the gateway.
pub const ERROR_SOURCE_GATEWAY: &str = "gateway";

/// Value of [`ERROR_SOURCE_HEADER`] for passthrough upstream errors.
pub const ERROR_SOURCE_UPSTREAM: &str = "upstream";

/// Media type of every gateway error body.
pub const PROBLEM_JSON: &str = "application/problem+json";

/// The only `detail` an internal error ever carries on the wire: the
/// diagnostic behind it is logged, never surfaced (`ADR`-0007).
pub const INTERNAL_DETAIL: &str = "an internal error occurred";

/// Resources that still reference a plugin, as GTS ids.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReferencedBy {
    /// Upstream GTS ids binding the plugin.
    pub upstreams: Vec<String>,
    /// Route GTS ids binding the plugin.
    pub routes: Vec<String>,
}

/// RFC 9457 extension members of an OAGW problem, serialized at the top level
/// of the body.
///
/// Every field is absent for the common management errors, which is why they
/// are boxed out of [`OagwProblem`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ProblemExtensions {
    /// Request URI path of the occurrence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    /// Distributed-tracing correlation id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// Target upstream, as a GTS id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_id: Option<String>,
    /// Routing key of the upstream the request targeted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Endpoint host the request would have reached.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Request path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Plugin GTS id, for plugin-scoped failures.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin_id: Option<String>,
    /// Who still references a plugin.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub referenced_by: Option<ReferencedBy>,
    /// Endpoint hosts the caller may name in `X-OAGW-Target-Host`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_hosts: Option<Vec<String>>,
    /// The rejected value, for malformed-header and malformed-payload errors.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invalid_value: Option<String>,
    /// Retry guidance, in seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u64>,
}

impl ProblemExtensions {
    /// `true` when the problem carries no extension member at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// An RFC 9457 problem carrying the OAGW `type` and extension fields.
///
/// Deliberately *not* `toolkit_canonical_errors::Problem`: that type's
/// mandatory `context` field would make the platform
/// `canonical_error_middleware` rewrite the body and drop the extensions the
/// error table requires.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OagwProblem {
    /// GTS instance id of the error identity.
    #[serde(rename = "type")]
    kind: &'static str,
    /// Human-readable summary.
    title: &'static str,
    /// HTTP status.
    status: u16,
    /// Occurrence-specific explanation.
    detail: String,
    /// RFC 9457 extension members, flattened into the body's top level.
    #[serde(flatten)]
    extensions: Box<ProblemExtensions>,
}

impl OagwProblem {
    /// Build a problem for an OAGW error identity.
    #[must_use]
    pub fn new(kind: OagwErrorType, detail: impl Into<String>) -> Self {
        Self {
            kind: kind.gts_id(),
            title: kind.title(),
            status: kind.status(),
            detail: detail.into(),
            extensions: Box::default(),
        }
    }

    /// Build a problem for a canonical (non-OAGW-specific) identity.
    #[must_use]
    pub fn canonical(
        kind: &'static str,
        title: &'static str,
        status: u16,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            title,
            status,
            detail: detail.into(),
            extensions: Box::default(),
        }
    }

    /// Stamp the request path into `instance`.
    #[must_use]
    pub fn with_instance(mut self, instance: impl Into<String>) -> Self {
        self.extensions.instance = Some(instance.into());
        self
    }

    /// Stamp a tracing correlation id.
    #[must_use]
    pub fn with_trace_id(mut self, trace_id: impl Into<String>) -> Self {
        self.extensions.trace_id = Some(trace_id.into());
        self
    }

    /// Stamp the routing key of the upstream the request targeted.
    #[must_use]
    pub fn with_alias(mut self, alias: impl Into<String>) -> Self {
        self.extensions.alias = Some(alias.into());
        self
    }

    /// Stamp the GTS id of the target upstream.
    #[must_use]
    pub fn with_upstream_id(mut self, upstream_id: impl Into<String>) -> Self {
        self.extensions.upstream_id = Some(upstream_id.into());
        self
    }

    /// Stamp the endpoint host the request would have reached.
    #[must_use]
    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.extensions.host = Some(host.into());
        self
    }

    /// Stamp the request path.
    #[must_use]
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.extensions.path = Some(path.into());
        self
    }

    /// Stamp the plugin GTS id of a plugin-scoped failure.
    #[must_use]
    pub fn with_plugin_id(mut self, plugin_id: impl Into<String>) -> Self {
        self.extensions.plugin_id = Some(plugin_id.into());
        self
    }

    /// Stamp who still references a plugin.
    #[must_use]
    pub fn with_referenced_by(mut self, referenced_by: ReferencedBy) -> Self {
        self.extensions.referenced_by = Some(referenced_by);
        self
    }

    /// Stamp the endpoint hosts the caller may name in `X-OAGW-Target-Host`.
    #[must_use]
    pub fn with_valid_hosts(mut self, valid_hosts: Vec<String>) -> Self {
        self.extensions.valid_hosts = Some(valid_hosts);
        self
    }

    /// Stamp the rejected value.
    #[must_use]
    pub fn with_invalid_value(mut self, invalid_value: impl Into<String>) -> Self {
        self.extensions.invalid_value = Some(invalid_value.into());
        self
    }

    /// Stamp retry guidance, in seconds.
    #[must_use]
    pub fn with_retry_after(mut self, seconds: u64) -> Self {
        self.extensions.retry_after_seconds = Some(seconds);
        self
    }

    /// The error identity (`type`) this problem carries.
    #[must_use]
    pub fn kind(&self) -> &str {
        self.kind
    }

    /// The `title` member.
    #[must_use]
    pub fn title(&self) -> &str {
        self.title
    }

    /// The `status` member.
    #[must_use]
    pub const fn status(&self) -> u16 {
        self.status
    }

    /// The `detail` member.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }

    /// The RFC 9457 extension members.
    #[must_use]
    pub const fn extensions(&self) -> &ProblemExtensions {
        &self.extensions
    }

    /// The error identity this problem projects, when it is an OAGW one.
    #[must_use]
    pub fn oagw_type(&self) -> Option<OagwErrorType> {
        let all = [
            OagwErrorType::Validation,
            OagwErrorType::MissingTargetHost,
            OagwErrorType::InvalidTargetHost,
            OagwErrorType::UnknownTargetHost,
            OagwErrorType::AuthFailed,
            OagwErrorType::RouteNotFound,
            OagwErrorType::PluginInUse,
            OagwErrorType::PayloadTooLarge,
            OagwErrorType::RateLimitExceeded,
            OagwErrorType::SecretNotFound,
            OagwErrorType::LinkUnavailable,
            OagwErrorType::ProtocolError,
            OagwErrorType::DownstreamError,
            OagwErrorType::StreamAborted,
            OagwErrorType::CircuitBreakerOpen,
            OagwErrorType::PluginNotFound,
            OagwErrorType::ConnectionTimeout,
            OagwErrorType::RequestTimeout,
            OagwErrorType::IdleTimeout,
        ];
        all.into_iter()
            .find(|candidate| candidate.gts_id() == self.kind)
    }
}

/// Result alias for handlers that speak the OAGW envelope.
pub type ApiResult<T> = Result<T, OagwProblem>;

impl From<DomainError> for OagwProblem {
    fn from(error: DomainError) -> Self {
        match error {
            DomainError::Validation { detail } => {
                OagwProblem::new(OagwErrorType::Validation, detail)
            }
            DomainError::MissingTargetHost { alias, valid_hosts } => OagwProblem::new(
                OagwErrorType::MissingTargetHost,
                format!(
                    "X-OAGW-Target-Host is required to select an endpoint of upstream \
                     '{alias}'; configured hosts: {}",
                    valid_hosts.join(", ")
                ),
            )
            .with_alias(alias)
            .with_valid_hosts(valid_hosts),
            DomainError::InvalidTargetHost { invalid_value } => OagwProblem::new(
                OagwErrorType::InvalidTargetHost,
                format!(
                    "X-OAGW-Target-Host must be a bare hostname or IP with no port, \
                     path or special characters: '{invalid_value}'"
                ),
            )
            .with_invalid_value(invalid_value),
            DomainError::UnknownTargetHost {
                invalid_value,
                valid_hosts,
            } => OagwProblem::new(
                OagwErrorType::UnknownTargetHost,
                format!(
                    "X-OAGW-Target-Host '{invalid_value}' does not match any configured \
                     endpoint; valid hosts: {}",
                    valid_hosts.join(", ")
                ),
            )
            .with_invalid_value(invalid_value)
            .with_valid_hosts(valid_hosts),
            DomainError::AuthenticationFailed { detail } => {
                OagwProblem::new(OagwErrorType::AuthFailed, detail)
            }
            DomainError::RouteNotFound { alias, path } => {
                // An empty path is the alias itself being unknown: nothing in
                // the tenant chain defines it, so there is no route to speak of
                // (`DESIGN` §3.3, the `404` of a routing miss).
                let detail = if path.is_empty() {
                    format!("no upstream of the tenant chain defines the alias '{alias}'")
                } else {
                    format!("no route of upstream '{alias}' matches '{path}'")
                };
                let problem =
                    OagwProblem::new(OagwErrorType::RouteNotFound, detail).with_alias(alias);
                if path.is_empty() {
                    problem
                } else {
                    problem.with_path(path)
                }
            }
            DomainError::PluginInUse {
                plugin_id,
                upstreams,
                routes,
            } => {
                let referenced_by = ReferencedBy { upstreams, routes };
                OagwProblem::new(
                    OagwErrorType::PluginInUse,
                    format!(
                        "plugin is referenced by {} upstream(s) and {} route(s)",
                        referenced_by.upstreams.len(),
                        referenced_by.routes.len()
                    ),
                )
                .with_plugin_id(plugin_id)
                .with_referenced_by(referenced_by)
            }
            DomainError::PayloadTooLarge { limit_bytes } => OagwProblem::new(
                OagwErrorType::PayloadTooLarge,
                format!("request payload exceeds the limit of {limit_bytes} bytes"),
            ),
            DomainError::RateLimitExceeded {
                detail,
                retry_after,
            } => with_retry_after(
                OagwProblem::new(OagwErrorType::RateLimitExceeded, detail),
                retry_after,
            ),
            DomainError::SecretNotFound { detail } => {
                OagwProblem::new(OagwErrorType::SecretNotFound, detail)
            }
            DomainError::LinkUnavailable {
                detail,
                retry_after,
            } => with_retry_after(
                OagwProblem::new(OagwErrorType::LinkUnavailable, detail),
                retry_after,
            ),
            DomainError::ProtocolError { detail, trace_id } => {
                let problem = OagwProblem::new(OagwErrorType::ProtocolError, detail);
                match trace_id {
                    Some(trace_id) => problem.with_trace_id(trace_id),
                    None => problem,
                }
            }
            DomainError::DownstreamError { detail } => {
                OagwProblem::new(OagwErrorType::DownstreamError, detail)
            }
            DomainError::StreamAborted { detail } => {
                OagwProblem::new(OagwErrorType::StreamAborted, detail)
            }
            DomainError::CircuitBreakerOpen { alias, retry_after } => OagwProblem::new(
                OagwErrorType::CircuitBreakerOpen,
                format!("circuit breaker for upstream '{alias}' is open"),
            )
            .with_alias(alias)
            .with_retry_after(retry_after),
            DomainError::PluginNotFound { plugin_id } => OagwProblem::new(
                OagwErrorType::PluginNotFound,
                format!("no plugin is registered under '{plugin_id}'"),
            )
            .with_plugin_id(plugin_id),
            DomainError::ConnectionTimeout { alias, limit_secs } => OagwProblem::new(
                OagwErrorType::ConnectionTimeout,
                format!("connecting to upstream '{alias}' exceeded {limit_secs}s"),
            )
            .with_alias(alias)
            .with_retry_after(limit_secs),
            DomainError::RequestTimeout { limit_secs } => OagwProblem::new(
                OagwErrorType::RequestTimeout,
                format!("upstream exchange exceeded {limit_secs}s"),
            )
            .with_retry_after(limit_secs),
            DomainError::IdleTimeout { limit_secs } => OagwProblem::new(
                OagwErrorType::IdleTimeout,
                format!("upstream stream idle past {limit_secs}s"),
            )
            .with_retry_after(limit_secs),
            DomainError::NotFound { resource } => OagwProblem::canonical(
                canonical_types::NOT_FOUND,
                "Not Found",
                404,
                format!("{resource} not found"),
            ),
            DomainError::Conflict { detail } => {
                OagwProblem::canonical(canonical_types::ALREADY_EXISTS, "Conflict", 409, detail)
            }
            DomainError::AccessDenied { detail } => {
                OagwProblem::canonical(canonical_types::PERMISSION_DENIED, "Forbidden", 403, detail)
            }
            DomainError::InvalidArgument { detail } => OagwProblem::canonical(
                canonical_types::INVALID_ARGUMENT,
                "Invalid Argument",
                400,
                detail,
            ),
            DomainError::ServiceUnavailable { detail, .. } => OagwProblem::canonical(
                canonical_types::SERVICE_UNAVAILABLE,
                "Service Unavailable",
                503,
                detail,
            ),
            DomainError::Internal { diagnostic, .. } => {
                // The diagnostic names internal machinery (a storage backend, a
                // codec); it is logged, never echoed to the caller.
                tracing::error!(diagnostic, "internal error");
                OagwProblem::canonical(
                    canonical_types::INTERNAL,
                    "Internal Error",
                    500,
                    INTERNAL_DETAIL,
                )
            }
        }
    }
}

/// Stamp [`ProblemExtensions::retry_after_seconds`] from an optional duration.
fn with_retry_after(
    mut problem: OagwProblem,
    retry_after: Option<std::time::Duration>,
) -> OagwProblem {
    if let Some(duration) = retry_after {
        // Round up: advising a client to retry a second early is worse than
        // advising it to retry a second late, and a sub-second budget must
        // still yield a usable `Retry-After` of at least one second.
        let seconds = duration.as_millis().div_ceil(1000);
        let seconds = u64::try_from(seconds).unwrap_or(u64::MAX).max(1);
        problem = problem.with_retry_after(seconds);
    }
    problem
}

impl IntoResponse for OagwProblem {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut builder = axum::response::Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, PROBLEM_JSON)
            .header(
                HEADER_X_OAGW_ERROR_SOURCE,
                HeaderValue::from_static(ERROR_SOURCE_GATEWAY),
            );
        if let Some(retry_after) = self.extensions.retry_after_seconds {
            builder = builder.header(header::RETRY_AFTER, retry_after.to_string());
        }
        let body = serde_json::to_vec(&self)
            .unwrap_or_else(|_| br#"{"title":"Internal Error","status":500}"#.to_vec());
        builder
            .body(axum::body::Body::from(body))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
    }
}

/// Stamp `X-OAGW-Error-Source: gateway` onto any response.
///
/// Errors carry the header through [`OagwProblem`]'s [`IntoResponse`]; the
/// success paths use this helper so that *every* gateway response declares
/// where it came from (`ADR`-0007).
#[must_use]
pub fn with_error_source(mut response: Response) -> Response {
    response.headers_mut().insert(
        HEADER_X_OAGW_ERROR_SOURCE,
        HeaderValue::from_static(ERROR_SOURCE_GATEWAY),
    );
    response
}

/// `X-OAGW-Error-Source`, as a [`HeaderName`].
pub const HEADER_X_OAGW_ERROR_SOURCE: HeaderName = HeaderName::from_static("x-oagw-error-source");

/// `X-OAGW-Target-Host`, as a [`HeaderName`].
pub const HEADER_X_OAGW_TARGET_HOST: HeaderName = HeaderName::from_static("x-oagw-target-host");

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "error_tests.rs"]
mod tests;
