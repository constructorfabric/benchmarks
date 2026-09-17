//! Tenant-hierarchy implementations.
//!
//! - [`SdkTenantHierarchy`] — production: delegates to the tenant resolver
//!   gear (barrier-respecting ancestry).
//! - [`MemoryHierarchy`] — tests/selftests: explicit parent map.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use tenant_resolver_sdk::{GetAncestorsOptions, TenantResolverClient};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model::TenantId;
use crate::domain::services::hierarchy::TenantHierarchy;

/// Production hierarchy backed by the tenant-resolver gear.
pub struct SdkTenantHierarchy {
    resolver: Arc<dyn TenantResolverClient>,
}

impl SdkTenantHierarchy {
    /// Construct from a resolved tenant-resolver client.
    #[must_use]
    pub fn new(resolver: Arc<dyn TenantResolverClient>) -> Self {
        Self { resolver }
    }
}

#[async_trait]
impl TenantHierarchy for SdkTenantHierarchy {
    async fn tenant_chain(&self, tenant: TenantId) -> Result<Vec<Uuid>, DomainError> {
        let ctx = SecurityContext::builder()
            .subject_id(tenant)
            .subject_tenant_id(tenant)
            .build()
            .map_err(|e| {
                DomainError::Internal(format!("security context build failed: {e}"))
            })?;
        let response = self
            .resolver
            .get_ancestors(&ctx, tenant_resolver_sdk::TenantId(tenant), &GetAncestorsOptions::default())
            .await
            .map_err(|e| DomainError::Internal(format!("tenant-resolver get_ancestors: {e}")))?;
        // `tenant` is the requested row; `ancestors` run parent → root.
        let mut chain = vec![response.tenant.id.0];
        for anc in response.ancestors {
            let id = anc.id.0;
            if chain.last() != Some(&id) {
                chain.push(id);
            }
        }
        Ok(chain)
    }
}

/// In-memory hierarchy (test/selftest): `child → parent` map.
#[derive(Clone, Default)]
pub struct MemoryHierarchy {
    parents: Arc<DashMap<Uuid, Uuid>>,
}

impl MemoryHierarchy {
    /// Register a parent/child edge. Returns a builder for ergonomic setup.
    #[must_use]
    pub fn add_edge(self, child: Uuid, parent: Uuid) -> Self {
        self.parents.insert(child, parent);
        self
    }

    /// Build a simple hierarchy helper from an explicit map.
    #[must_use]
    pub fn from_parents(map: &[(Uuid, Uuid)]) -> Self {
        let parents = Arc::new(DashMap::new());
        for (c, p) in map {
            parents.insert(*c, *p);
        }
        Self { parents }
    }
}

#[async_trait]
impl TenantHierarchy for MemoryHierarchy {
    async fn tenant_chain(&self, tenant: TenantId) -> Result<Vec<Uuid>, DomainError> {
        let mut chain: Vec<Uuid> = Vec::new();
        let mut current = Some(tenant);
        let mut guard: HashMap<Uuid, ()> = HashMap::new();
        while let Some(t) = current {
            if guard.insert(t, ()).is_some() {
                return Err(DomainError::Internal(
                    "tenant hierarchy contains a cycle".to_owned(),
                ));
            }
            chain.push(t);
            current = self.parents.get(&t).map(|p| *p);
        }
        Ok(chain)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn memory_hierarchy_builds_chain_descendant_first() {
        let root = Uuid::new_v4();
        let l1 = Uuid::new_v4();
        let l2 = Uuid::new_v4();
        let h = MemoryHierarchy::default()
            .add_edge(l2, l1)
            .add_edge(l1, root);
        let chain = h.tenant_chain(l2).await.unwrap();
        assert_eq!(chain, vec![l2, l1, root]);
        assert_eq!(h.tenant_chain(root).await.unwrap(), vec![root]);
    }

    #[tokio::test]
    async fn memory_hierarchy_detects_cycle() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let h = MemoryHierarchy::default().add_edge(a, b).add_edge(b, a);
        assert!(h.tenant_chain(a).await.is_err());
    }
}
