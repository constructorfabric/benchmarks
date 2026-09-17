//! Domain error types for the OAGW gear and their canonical mapping.
//!
//! Every domain failure is authored as a typed variant here and mapped to
//! the platform [`CanonicalError`] (AIP-193 ladder, ADR 0005) through the
//! single `From<DomainError> for CanonicalError` implementation, reusing the
//! `#[resource_error]` resource markers for the OAGW GTS type ids.

use toolkit_canonical_errors::{CanonicalError, resource_error};
use toolkit_macros::domain_model;

/// Resource marker for the OAGW upstream type (`gts.cf.core.oagw.upstream.v1~`).
#[resource_error(gts_id!("cf.core.oagw.upstream.v1~"))]
pub(crate) struct UpstreamResource;

/// Resource marker for the OAGW route type (`gts.cf.core.oagw.route.v1~`).
#[resource_error(gts_id!("cf.core.oagw.route.v1~"))]
pub(crate) struct RouteResource;

/// Resource marker for the OAGW plugin type (`gts.cf.core.oagw.plugin.v1~`).
#[resource_error(gts_id!("cf.core.oagw.plugin.v1~"))]
pub(crate) struct PluginResource;

/// Resource marker for the OAGW proxy type (`gts.cf.core.oagw.proxy.v1~`).
#[resource_error(gts_id!("cf.core.oagw.proxy.v1~"))]
pub(crate) struct ProxyResource;

/// Domain-level errors for the OAGW control plane.
#[domain_model]
#[derive(Debug, thiserror::Error)]
pub enum DomainError {
    /// A referenced upstream does not exist.
    #[error("upstream `{0}` not found")]
    UpstreamNotFound(String),
    /// A referenced upstream alias is already bound to a different entity.
    #[error("upstream alias `{0}` already exists")]
    UpstreamAliasConflict(String),
    /// A referenced route does not exist.
    #[error("route `{0}` not found")]
    RouteNotFound(String),
    /// A referenced route alias is already bound to a different entity.
    #[error("route alias `{0}` already exists")]
    RouteAliasConflict(String),
    /// The route references an upstream that is disabled or missing.
    #[error("route `{0}` references a disabled or unknown upstream `{1}`")]
    RouteDisabledUpstream(String, String),
    /// A referenced plugin does not exist.
    #[error("plugin `{0}` not found")]
    PluginNotFound(String),
    /// A referenced plugin alias is already bound to a different entity.
    #[error("plugin alias `{0}` already exists")]
    PluginAliasConflict(String),
    /// A plugin cannot be unbound while it is still bound to live routes.
    #[error("plugin `{0}` is still bound and cannot be removed")]
    PluginStillBound(String),
    /// The same plugin was bound twice to the same target (bind-once rule).
    #[error("plugin `{0}` is already bound to this target")]
    PluginInUse(String),
    /// A route cannot be deleted while plugins are still bound to it.
    #[error("route `{0}` still has plugins bound and cannot be removed")]
    RouteStillBound(String),
    /// An upstream cannot be deleted while plugins are still bound to it.
    #[error("upstream `{0}` still has plugins bound and cannot be removed")]
    UpstreamStillBound(String),
    /// The request body failed schema validation.
    #[error("validation failed: {detail}")]
    Validation { detail: String },
    /// The caller's tenant is not allowed to touch the entity.
    #[error("tenant scope violation for resource `{0}`")]
    TenantScope(String),
    /// The proxy alias does not resolve to a route.
    #[error("proxy alias `{0}` not found")]
    ProxyAliasNotFound(String),
    /// The upstream transport is not permitted by config.
    #[error("upstream transport for `{0}` not permitted: {1}")]
    TransportNotPermitted(String, String),
    /// Guard execution produced a client-facing rejection.
    #[error("request rejected by guard: {0}")]
    GuardRejected(String),
    /// Auth plugin executed but could not satisfy the request.
    #[error("authentication failed: {0}")]
    AuthFailed(String),
    /// Credential resolution could not be satisfied.
    #[error("credential resolution failed: {0}")]
    CredentialError(String),
    /// SSRF guard blocked the target.
    #[error("SSRF guard blocked target: {0}")]
    SsrfBlocked(String),
    /// Rate limiting rejected the request.
    #[error("rate limit exceeded")]
    RateLimited,
    /// PEP evaluation denied the request.
    #[error("authorization denied: {0}")]
    PepDenied(String),
    /// A general internal failure (authoring remains precise).
    #[error("{0}")]
    Internal(String),
}

impl DomainError {
    /// Builds a validation error from a message.
    #[must_use]
    pub fn validation(detail: impl Into<String>) -> Self {
        Self::Validation {
            detail: detail.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// The single AIP-193 ladder: DomainError → CanonicalError
// ---------------------------------------------------------------------------

impl From<DomainError> for CanonicalError {
    fn from(err: DomainError) -> Self {
        match err {
            DomainError::UpstreamNotFound(alias) => UpstreamResource::not_found(&alias)
                .with_resource(&alias)
                .create(),
            DomainError::UpstreamAliasConflict(alias) => UpstreamResource::already_exists(&alias)
                .with_resource(&alias)
                .create(),
            DomainError::RouteNotFound(alias) => RouteResource::not_found(&alias)
                .with_resource(&alias)
                .create(),
            DomainError::RouteAliasConflict(alias) => RouteResource::already_exists(&alias)
                .with_resource(&alias)
                .create(),
            DomainError::RouteDisabledUpstream(route, upstream) => {
                RouteResource::failed_precondition()
                    .with_precondition_violation(
                        upstream,
                        format!("route `{route}` references a disabled or unknown upstream"),
                        "DISABLED_UPSTREAM",
                    )
                    .create()
            }
            DomainError::PluginNotFound(alias) => PluginResource::not_found(&alias)
                .with_resource(&alias)
                .create(),
            DomainError::PluginAliasConflict(alias) => PluginResource::already_exists(&alias)
                .with_resource(&alias)
                .create(),
            DomainError::PluginStillBound(alias) => PluginResource::aborted(alias)
                .with_reason("PLUGIN_STILL_BOUND")
                .create(),
            DomainError::PluginInUse(alias) => PluginResource::already_exists(&alias)
                .with_resource(&alias)
                .create(),
            DomainError::RouteStillBound(alias) => RouteResource::aborted(alias)
                .with_reason("ROUTE_STILL_BOUND")
                .create(),
            DomainError::UpstreamStillBound(alias) => UpstreamResource::aborted(alias)
                .with_reason("UPSTREAM_STILL_BOUND")
                .create(),
            DomainError::Validation { detail } => ProxyResource::invalid_argument()
                .with_field_violation("body", detail, "INVALID")
                .create(),
            DomainError::TenantScope(resource) => ProxyResource::permission_denied()
                .with_reason(format!("TENANT_SCOPE: {resource}"))
                .create(),
            DomainError::ProxyAliasNotFound(alias) => ProxyResource::not_found(&alias)
                .with_resource(&alias)
                .create(),
            DomainError::TransportNotPermitted(resource, detail) => {
                ProxyResource::failed_precondition()
                    .with_precondition_violation("upstream", detail, resource.as_str())
                    .create()
            }
            DomainError::GuardRejected(detail) => ProxyResource::invalid_argument()
                .with_field_violation("request", detail, "GUARD")
                .create(),
            DomainError::AuthFailed(detail) => CanonicalError::unauthenticated()
                .with_reason(detail)
                .create(),
            DomainError::CredentialError(detail) => ProxyResource::failed_precondition()
                .with_precondition_violation("credential", detail, "CREDENTIAL_ERROR")
                .create(),
            DomainError::SsrfBlocked(target) => ProxyResource::permission_denied()
                .with_reason(format!("SSRF_BLOCKED: {target}"))
                .create(),
            DomainError::RateLimited => ProxyResource::resource_exhausted(
                "rate limit exceeded; retry after the bucket refills",
            )
            .with_quota_violation("proxy_rate_limit", "too many requests")
            .create(),
            DomainError::PepDenied(detail) => ProxyResource::permission_denied()
                .with_reason(format!("PEP_DENIED: {detail}"))
                .create(),
            DomainError::Internal(detail) => CanonicalError::internal(detail).create(),
        }
    }
}
