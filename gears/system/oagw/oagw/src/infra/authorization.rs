//! The authorization and tenant-hierarchy adapters of the management surface
//! (FEATURE entry 2.2).
//!
//! The domain states *what* it needs — "evaluate this permission for this
//! actor over this resource" and "the ancestor chain of this tenant" — through
//! [`crate::domain::services::management::ManagementAuthorizer`] and
//! [`crate::domain::services::management::AncestorResolver`]. This module is
//! the *only* place the `toolkit-security` context, the `authz_resolver` PEP
//! and the `tenant_resolver` client meet the domain's actor, so the domain
//! layer keeps its zero-`toolkit_security` boundary.
//!
//! # No credential work here
//!
//! `credstore` is deliberately absent: neither the existence nor the tenant
//! accessibility of a `cred://` reference is verified on the management path.
//! That is entry 2.6's request-time work, and the FEATURE forbids a
//! `cred_store` call anywhere on this path.

use std::sync::Arc;

use async_trait::async_trait;
use authz_resolver_sdk::pep::{AccessRequest, EnforcerError, PolicyEnforcer, ResourceType};
use tenant_resolver_sdk::{GetAncestorsOptions, TenantId, TenantResolverClient};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::gts_helpers::{PERM_BIND, UPSTREAM_BASE_TYPE};
use crate::domain::services::management::{Actor, AncestorResolver, AuthorizeError, ManagementAuthorizer};

/// The PEP resource type of an upstream record: the anonymous base type, with
/// no supported constraint property, because the management surface filters
/// tenant scope itself and needs no row-level projection.
const UPSTREAM_RESOURCE: ResourceType = ResourceType::from_static(UPSTREAM_BASE_TYPE, &[]);

/// The PEP resource type of the `oagw:upstream:bind` hierarchy permission:
/// the same record, under the hierarchy permission name.
const BIND_RESOURCE: ResourceType = ResourceType::from_static(PERM_BIND, &[]);

/// The context the authorization decision is evaluated under.
///
/// The domain hands over a [`Actor`]; the PEP needs a
/// [`SecurityContext`]. The adapter reconstructs the two fields the PDP
/// reads — the subject and its tenant — and carries nothing else, because the
/// api-gateway has already validated the bearer token before this adapter
/// runs.
fn context_of(actor: &Actor) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(actor.principal_id)
        .subject_tenant_id(actor.tenant_id)
        .build()
        .unwrap_or_else(|_| SecurityContext::anonymous())
}

/// The [`ManagementAuthorizer`] over the `authz_resolver` PEP.
///
/// Every management operation evaluates exactly one permission; the decision
/// is obtained through [`PolicyEnforcer::access_scope_with`] with
/// `require_constraints: false`, because the management surface applies the
/// tenant scoping itself and a `deny_all` scope only means "the permission was
/// not granted".
pub struct AuthzManagementAuthorizer {
    enforcer: PolicyEnforcer,
}

impl AuthzManagementAuthorizer {
    /// An authorizer over `authz_resolver`.
    #[must_use]
    pub fn new(authz: Arc<dyn authz_resolver_sdk::AuthZResolverClient>) -> Self {
        Self { enforcer: PolicyEnforcer::new(authz) }
    }
}

impl std::fmt::Debug for AuthzManagementAuthorizer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("AuthzManagementAuthorizer").finish()
    }
}

#[async_trait]
impl ManagementAuthorizer for AuthzManagementAuthorizer {
    async fn authorize(
        &self,
        actor: &Actor,
        permission: &str,
        resource_id: &str,
    ) -> Result<(), AuthorizeError> {
        // The `oagw:upstream:bind` hierarchy permission is evaluated against
        // its own permission identifier; the per-operation permissions are
        // evaluated against the upstream base type.
        let resource = if permission == PERM_BIND {
            &BIND_RESOURCE
        } else {
            &UPSTREAM_RESOURCE
        };
        let context = context_of(actor);
        let scope = self
            .enforcer
            .access_scope_with(
                &context,
                resource,
                permission,
                None,
                &AccessRequest::new()
                    .require_constraints(false)
                    .context_tenant_id(actor.tenant_id),
            )
            .await;
        match scope {
            Ok(scope) if !scope.is_deny_all() => Ok(()),
            Ok(_) => Err(AuthorizeError::Denied {
                permission: permission.to_owned(),
                detail: format!("the decision for `{permission}` over `{resource_id}` is deny"),
            }),
            Err(EnforcerError::Denied { deny_reason }) => Err(AuthorizeError::Denied {
                permission: permission.to_owned(),
                detail: deny_reason.as_ref().map_or_else(
                    || "the decision is deny".to_owned(),
                    |reason| format!("the decision is deny: {reason:?}"),
                ),
            }),
            Err(error) => Err(AuthorizeError::Unavailable {
                detail: format!("the authorization decision could not be obtained: {error}"),
            }),
        }
    }
}

/// The permission the metrics surface evaluates
/// (`cpt-cf-oagw-dod-observability-and-state-metric-surface`).
///
/// It names the OAGW proxy base type under the operations action `metrics`, so
/// it is a decision the same PDP that answers the management and proxy
/// permissions takes, and a grant of `gts.cf.core.oagw.proxy.v1~:invoke` does
/// not imply it.
pub const PERM_METRICS: &str = "gts.cf.core.oagw.proxy.v1~:metrics";

/// The PEP resource type of the metrics surface: the OAGW proxy base type,
/// with no supported constraint property, because the exposition is a
/// gear-wide surface and carries no row-level projection.
const METRICS_RESOURCE: ResourceType = ResourceType::from_static(crate::gts::PROXY_BASE_TYPE, &[]);

/// The admin gate of `GET /metrics`
/// (`cpt-cf-oagw-dod-observability-and-state-metric-surface`).
///
/// The exposition is served only to a caller the platform edge authenticated
/// and the PDP granted `PERM_METRICS` for; the decision is evaluated in
/// root-only tenant mode, because the metrics surface is gear-wide and carries
/// no tenant projection of its own.
pub struct MetricsGate {
    enforcer: PolicyEnforcer,
}

impl MetricsGate {
    /// A gate over `authz_resolver`.
    #[must_use]
    pub fn new(authz: Arc<dyn authz_resolver_sdk::AuthZResolverClient>) -> Self {
        Self { enforcer: PolicyEnforcer::new(authz) }
    }
}

impl std::fmt::Debug for MetricsGate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("MetricsGate").finish()
    }
}

impl MetricsGate {
    /// Whether `actor` may read the metrics exposition.
    ///
    /// # Errors
    ///
    /// The [`AuthorizeError`] the denied or unobtainable decision maps to, the
    /// same two categories the management authorizer reports.
    pub async fn authorize(&self, actor: &Actor) -> Result<(), AuthorizeError> {
        let context = context_of(actor);
        let scope = self
            .enforcer
            .access_scope_with(
                &context,
                &METRICS_RESOURCE,
                PERM_METRICS,
                None,
                &AccessRequest::new()
                    .require_constraints(false)
                    .tenant_mode(authz_resolver_sdk::TenantMode::RootOnly),
            )
            .await;
        match scope {
            Ok(scope) if !scope.is_deny_all() => Ok(()),
            Ok(_) => Err(AuthorizeError::Denied {
                permission: PERM_METRICS.to_owned(),
                detail: format!("the decision for `{PERM_METRICS}` over the metrics surface is deny"),
            }),
            Err(EnforcerError::Denied { deny_reason }) => Err(AuthorizeError::Denied {
                permission: PERM_METRICS.to_owned(),
                detail: deny_reason.as_ref().map_or_else(
                    || "the decision is deny".to_owned(),
                    |reason| format!("the decision is deny: {reason:?}"),
                ),
            }),
            Err(error) => Err(AuthorizeError::Unavailable {
                detail: format!("the authorization decision could not be obtained: {error}"),
            }),
        }
    }
}

/// The [`AncestorResolver`] over `tenant_resolver`.
///
/// The chain is requested with the *direct parent first* ordering
/// `get_ancestors` documents, and is handed to the domain as plain tenant
/// identifiers. An unresolvable chain is an internal failure, never a silent
/// empty chain: a tenant the resolver does not know is a broken actor context.
pub struct TenantHierarchyAncestors {
    resolver: Arc<dyn TenantResolverClient>,
}


impl TenantHierarchyAncestors {
    /// A resolver over `tenant_resolver`.
    #[must_use]
    pub fn new(resolver: Arc<dyn TenantResolverClient>) -> Self {
        Self { resolver }
    }
}

impl std::fmt::Debug for TenantHierarchyAncestors {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("TenantHierarchyAncestors").finish()
    }
}

#[async_trait]
impl AncestorResolver for TenantHierarchyAncestors {
    async fn ancestors(&self, actor: &Actor, tenant_id: Uuid) -> Result<Vec<Uuid>, DomainError> {
        let context = context_of(actor);
        let response = self
            .resolver
            .get_ancestors(&context, TenantId(tenant_id), &GetAncestorsOptions::default())
            .await
            .map_err(|error| DomainError::Internal(format!("tenant chain unavailable: {error}")))?;
        Ok(response.ancestors.into_iter().map(|tenant| tenant.id.0).collect())
    }
}
