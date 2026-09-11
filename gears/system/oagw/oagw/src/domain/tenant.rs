//! Tenant-hierarchy contract.
//!
//! OAGW receives `tenant_id` from the `SecurityContext` and needs the ancestor
//! chain to resolve aliases and merge configuration. The hierarchy itself is
//! owned by the tenant-resolver gear, so the domain only states what it needs.

use async_trait::async_trait;
use toolkit_security::SecurityContext;
use uuid::Uuid;

/// The ancestor chain of a tenant, nearest parent first.
#[async_trait]
pub trait TenantChain: Send + Sync {
    /// Ancestors of `tenant_id`, ordered parent → root.
    async fn ancestors(&self, ctx: &SecurityContext, tenant_id: Uuid) -> Vec<Uuid>;

    /// The full resolution chain: `tenant_id` followed by its ancestors.
    async fn chain(&self, ctx: &SecurityContext, tenant_id: Uuid) -> Vec<Uuid> {
        let mut chain = vec![tenant_id];
        chain.extend(self.ancestors(ctx, tenant_id).await);
        chain
    }
}
