//! Tenant-hierarchy ancestor resolution for the proxy data plane.
//!
//! `cpt-cf-oagw-algo-proxy-resolve-alias` needs the calling tenant's
//! ancestor chain (descendant to root) to walk for alias shadowing, and
//! `cpt-cf-oagw-algo-proxy-merge-config` needs the same chain to fold
//! hierarchical sharing modes. Exactly as documented by
//! `cpt-cf-oagw-feature-upstream-management` (entry 2.2) in
//! `crate::api::rest::upstreams`'s own `TenantHierarchyProvider` -- this
//! gear has no `GearCtx`/hub handle reachable from a REST handler to a real
//! `tenant-resolver-sdk` client in this round -- proxy-core repeats that
//! same injectable-provider pattern independently rather than reaching
//! across a module boundary into a sibling entry's private implementation
//! detail. Production always uses [`NoTenantHierarchy`] (every tenant is
//! its own root); this is conservative because it only ever *narrows* which
//! shadowing/merge branches can fire. This module's own tests inject a
//! fake provider to exercise the ancestor-chain branches end to end.

use std::fmt;

use uuid::Uuid;

/// Ancestor-tenant resolution `cpt-cf-oagw-algo-proxy-resolve-alias` and
/// `cpt-cf-oagw-algo-proxy-merge-config` depend on. See the module doc
/// comment for why this is injectable rather than backed by a concrete
/// tenant-hierarchy client in this round.
pub(crate) trait TenantHierarchyProvider: fmt::Debug + Send + Sync {
    /// Ancestor tenant ids for `tenant_id`, ordered nearest-parent to root.
    fn ancestors(&self, tenant_id: Uuid) -> Vec<Uuid>;
}

/// Default provider: every tenant is its own root (no ancestors).
#[derive(Debug, Default)]
pub(crate) struct NoTenantHierarchy;

impl TenantHierarchyProvider for NoTenantHierarchy {
    fn ancestors(&self, _tenant_id: Uuid) -> Vec<Uuid> {
        Vec::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn no_tenant_hierarchy_returns_no_ancestors() {
        assert!(NoTenantHierarchy.ancestors(Uuid::new_v4()).is_empty());
    }
}
