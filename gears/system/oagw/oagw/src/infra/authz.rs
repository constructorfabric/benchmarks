//! Tenant-chain and PDP plumbing.
//!
//! `TenantChain` turns a [`SecurityContext`] into the ordered chain of tenants
//! (self first, then direct parent → root) that alias resolution and
//! configuration inheritance walk. `Pep` wraps the authz-resolver
//! [`PolicyEnforcer`] for the `oagw` resource types.

use std::sync::Arc;

use toolkit_security::SecurityContext;
use tenant_resolver_sdk::{GetAncestorsOptions, TenantResolverClient};
use uuid::Uuid;

use crate::domain::error::DomainError;

/// Ordered tenant chain: the caller's tenant first, then its ancestors
/// (direct parent → root).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantChain {
    entries: Vec<Uuid>,
}

impl TenantChain {
    /// Builds a chain from an explicit ordering (already descendant-first).
    pub fn from_entries(entries: Vec<Uuid>) -> Self {
        let mut seen = std::collections::HashSet::new();
        Self {
            entries: entries.into_iter().filter(|t| seen.insert(*t)).collect(),
        }
    }

    /// Resolves the chain for a security context through the tenant resolver.
    pub async fn for_context(
        resolver: &dyn TenantResolverClient,
        ctx: &SecurityContext,
    ) -> Result<Self, DomainError> {
        let tenant_id = ctx.subject_tenant_id();
        if tenant_id.is_nil() {
            return Err(DomainError::PermissionDenied {
                detail: "the security context carries no tenant".to_string(),
            });
        }

        let mut entries = vec![tenant_id];
        let response = resolver
            .get_ancestors(ctx, tenant_resolver_sdk::TenantId(tenant_id), &GetAncestorsOptions::default())
            .await
            .map_err(|e| {
                DomainError::Internal {
                    diagnostic: format!("tenant ancestor lookup failed: {e}"),
                }
            })?;
        for ancestor in response.ancestors {
            entries.push(ancestor.id.0);
        }
        Ok(Self { entries })
    }

    /// The caller's own tenant.
    pub fn self_tenant(&self) -> Uuid {
        self.entries[0]
    }

    /// The ordered entries (self first).
    pub fn entries(&self) -> &[Uuid] {
        &self.entries
    }

    /// The ancestors only (direct parent → root), without the caller's tenant.
    pub fn ancestors(&self) -> &[Uuid] {
        &self.entries[1.min(self.entries.len())..]
    }

    /// `true` when `tenant` appears anywhere in the chain.
    pub fn contains(&self, tenant: Uuid) -> bool {
        self.entries.contains(&tenant)
    }

    /// The position of `tenant` in the chain (0 = self).
    pub fn position(&self, tenant: Uuid) -> Option<usize> {
        self.entries.iter().position(|t| *t == tenant)
    }

    /// Whether `tenant` is an ancestor of the caller's tenant.
    pub fn is_ancestor(&self, tenant: Uuid) -> bool {
        self.position(tenant).is_some_and(|p| p > 0)
    }

    /// The first `limit` entries, used for scoped lookups.
    pub fn scope(&self) -> Vec<Uuid> {
        self.entries.clone()
    }
}

/// A reference to the authz-resolver PEP used by the management handlers.
#[derive(Clone)]
pub struct Pep {
    enforcer: Arc<authz_resolver_sdk::pep::PolicyEnforcer>,
}

impl std::fmt::Debug for Pep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Pep")
    }
}

impl Pep {
    /// Wraps a resolver client.
    pub fn new(authz: Arc<dyn authz_resolver_sdk::AuthZResolverClient>) -> Self {
        Self {
            enforcer: Arc::new(authz_resolver_sdk::pep::PolicyEnforcer::new(authz)),
        }
    }

    /// Wraps an existing enforcer.
    pub fn from_enforcer(enforcer: Arc<authz_resolver_sdk::pep::PolicyEnforcer>) -> Self {
        Self { enforcer }
    }

    /// Evaluates an access decision for an `oagw` resource.
    pub async fn access_scope(
        &self,
        ctx: &SecurityContext,
        resource: &'static str,
        action: &str,
        resource_id: Option<Uuid>,
    ) -> Result<toolkit_security::AccessScope, DomainError> {
        self.enforcer
            .access_scope(ctx, &resource_type(resource), action, resource_id)
            .await
            .map_err(map_enforcer_error)
    }
}

/// Builds an `oagw` [`ResourceType`] for the documented GTS resource id.
fn resource_type(gts_type: &'static str) -> authz_resolver_sdk::pep::ResourceType {
    authz_resolver_sdk::pep::ResourceType::from_static(
        gts_type,
        &[
            toolkit_security::access_scope::pep_properties::OWNER_TENANT_ID,
            toolkit_security::access_scope::pep_properties::RESOURCE_ID,
        ],
    )
}

/// Translates a PDP failure into a domain error, never leaking internals.
fn map_enforcer_error(e: authz_resolver_sdk::pep::EnforcerError) -> DomainError {
    use authz_resolver_sdk::pep::EnforcerError as EE;
    match e {
        EE::Denied { deny_reason } => DomainError::PermissionDenied {
            detail: deny_reason.map_or_else(
                || "denied by policy".to_string(),
                |reason| reason.details.unwrap_or(reason.error_code),
            ),
        },
        EE::EvaluationFailed(err) => DomainError::Internal {
            diagnostic: format!("policy evaluation failed: {err}"),
        },
        EE::CompileFailed(err) => DomainError::Internal {
            diagnostic: format!("policy constraint compilation failed: {err}"),
        },
    }
}

/// The `oagw` resource identifiers the PEP evaluates against.
pub mod resources {
    /// Upstream resource.
    pub const UPSTREAM: &str = crate::domain::gts_helpers::UPSTREAM_TYPE;
    /// Route resource.
    pub const ROUTE: &str = crate::domain::gts_helpers::ROUTE_TYPE;
    /// Auth plugin resource.
    pub const AUTH_PLUGIN: &str = crate::domain::gts_helpers::AUTH_PLUGIN_TYPE;
    /// Guard plugin resource.
    pub const GUARD_PLUGIN: &str = crate::domain::gts_helpers::GUARD_PLUGIN_TYPE;
    /// Transform plugin resource.
    pub const TRANSFORM_PLUGIN: &str = crate::domain::gts_helpers::TRANSFORM_PLUGIN_TYPE;
    /// Proxy (invoke) resource.
    pub const PROXY: &str = crate::domain::gts_helpers::PROXY_TYPE;
}

/// Ensures an access scope actually permits the addressed tenant row.
///
/// The PDP returns an [`AccessScope`] describing which rows the caller may
/// see; the management layer narrows on the caller's tenant first and only
/// falls back to the scope when it is unconstrained.
pub fn scope_allows(scope: &toolkit_security::AccessScope, tenant_id: Uuid) -> bool {
    if scope.is_unconstrained() {
        return true;
    }
    if scope.is_deny_all() {
        return false;
    }
    scope.contains_uuid(toolkit_security::access_scope::pep_properties::OWNER_TENANT_ID, tenant_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_is_descendant_first_and_deduplicated() {
        let root = uuid::Uuid::from_u128(1);
        let mid = uuid::Uuid::from_u128(2);
        let leaf = uuid::Uuid::from_u128(3);
        let chain = TenantChain::from_entries(vec![leaf, mid, root, mid]);
        assert_eq!(chain.self_tenant(), leaf);
        assert_eq!(chain.ancestors(), &[mid, root]);
        assert!(chain.contains(mid));
        assert!(chain.is_ancestor(mid));
        assert!(!chain.is_ancestor(leaf));
        assert_eq!(chain.position(leaf), Some(0));
    }

    #[test]
    fn single_entry_chain_has_no_ancestors() {
        let leaf = uuid::Uuid::from_u128(7);
        let chain = TenantChain::from_entries(vec![leaf]);
        assert!(chain.ancestors().is_empty());
        assert_eq!(chain.entries().len(), 1);
    }

    #[test]
    fn scope_allows_distinguishes_deny_unconstrained_and_constrained() {
        let tenant = uuid::Uuid::from_u128(9);
        assert!(scope_allows(&toolkit_security::AccessScope::allow_all(), tenant));
        assert!(!scope_allows(&toolkit_security::AccessScope::deny_all(), tenant));
        assert!(scope_allows(&toolkit_security::AccessScope::for_tenant(tenant), tenant));
        assert!(!scope_allows(
            &toolkit_security::AccessScope::for_tenant(uuid::Uuid::from_u128(10)),
            tenant
        ));
    }

    #[test]
    fn oagw_resource_constants_use_the_documented_gts_types() {
        assert_eq!(resources::UPSTREAM, crate::domain::gts_helpers::UPSTREAM_TYPE);
        assert_eq!(resources::ROUTE, crate::domain::gts_helpers::ROUTE_TYPE);
        assert_eq!(resources::PROXY, crate::domain::gts_helpers::PROXY_TYPE);
    }
}
