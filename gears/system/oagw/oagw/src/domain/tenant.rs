//! Tenant hierarchy lookups used by alias shadowing and config inheritance.

use async_trait::async_trait;
use toolkit_security::SecurityContext;
use uuid::Uuid;

/// Resolves the descendant → root chain a proxy request inherits from.
#[async_trait]
pub trait TenantDirectory: Send + Sync {
    /// Chain starting with `tenant_id` itself, then each ancestor up to the
    /// root.
    ///
    /// Implementations degrade to `[tenant_id]` rather than failing: a
    /// hierarchy lookup outage must not take down proxying for a tenant's own
    /// upstreams.
    async fn ancestor_chain(&self, ctx: &SecurityContext, tenant_id: Uuid) -> Vec<Uuid>;
}

/// Directory for deployments with no hierarchy — every tenant is its own root.
#[derive(Debug, Default, Clone, Copy)]
pub struct FlatTenantDirectory;

#[async_trait]
impl TenantDirectory for FlatTenantDirectory {
    async fn ancestor_chain(&self, _ctx: &SecurityContext, tenant_id: Uuid) -> Vec<Uuid> {
        vec![tenant_id]
    }
}
