// Created: 2026-08-29 by Constructor Tech
//! Tenant-hierarchy adapter (DESIGN §3.2 hierarchical configuration).
//!
//! The platform exposes no ancestor API to a gear, so the adapter resolves a
//! single-tenant chain: every tenant is its own root. Hierarchical lookups
//! therefore see exactly one level, which keeps alias shadowing and rate-limit
//! inheritance well-defined while a real hierarchy source stays a drop-in
//! replacement for [`TenantHierarchy`].

use async_trait::async_trait;
use uuid::Uuid;

use crate::domain::error::DomainResult;
use crate::domain::services::proxy::TenantHierarchy;

/// Hierarchy source that returns one level per tenant.
#[derive(Debug, Clone, Copy, Default)]
pub struct FlatTenantHierarchy;

#[async_trait]
impl TenantHierarchy for FlatTenantHierarchy {
    async fn chain(&self, tenant_id: Uuid) -> DomainResult<Vec<Uuid>> {
        Ok(vec![tenant_id])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn every_tenant_is_its_own_root() {
        let tenant = Uuid::new_v4();
        let chain = FlatTenantHierarchy.chain(tenant).await.expect("chain");
        assert_eq!(chain, vec![tenant]);
    }
}
