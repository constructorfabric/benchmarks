//! Shared test doubles for the OAGW domain layer (test builds only).
//!
//! * [`MockTenantResolver`] — a configurable
//!   [`TenantResolverClient`] for exercising the tenant-hierarchy walk
//!   (`tenant_chain` / `resolve_chain`) without a live resolver gear.

use async_trait::async_trait;
use std::collections::HashMap;
use tenant_resolver_sdk::{
    GetAncestorsOptions, GetAncestorsResponse, GetDescendantsOptions, GetDescendantsResponse,
    GetTenantsOptions, IsAncestorOptions, TenantId, TenantInfo, TenantRef, TenantResolverClient,
    TenantResolverError, TenantStatus,
};
use toolkit_security::SecurityContext;
use uuid::Uuid;

/// In-memory [`TenantResolverClient`] mapping a tenant to its ancestor chain
/// (ordered direct parent → root, matching the SDK contract). Ancestors are
/// reported `Active`; the unused read surfaces degrade to empty/error
/// responses (the OAGW control plane only calls `get_ancestors`).
pub struct MockTenantResolver {
    /// `tenant -> ancestors` (direct parent → root).
    hierarchy: HashMap<Uuid, Vec<Uuid>>,
}

impl MockTenantResolver {
    /// Create a resolver from `(tenant, ancestors)` pairs.
    #[must_use]
    pub fn new(hierarchy: impl IntoIterator<Item = (Uuid, Vec<Uuid>)>) -> Self {
        Self {
            hierarchy: hierarchy.into_iter().collect(),
        }
    }

    /// A resolver where `tenant` has no ancestors (single-tenant root).
    #[must_use]
    pub fn single_root(tenant: Uuid) -> Self {
        Self::new([(tenant, Vec::new())])
    }

    fn ref_of(id: Uuid) -> TenantRef {
        TenantRef {
            id: TenantId(id),
            status: TenantStatus::Active,
            tenant_type: None,
            parent_id: None,
            self_managed: false,
        }
    }
}

#[async_trait]
impl TenantResolverClient for MockTenantResolver {
    async fn get_tenant(
        &self,
        _ctx: &SecurityContext,
        id: TenantId,
    ) -> Result<TenantInfo, TenantResolverError> {
        Ok(TenantInfo {
            id,
            name: "mock-tenant".to_owned(),
            status: TenantStatus::Active,
            tenant_type: None,
            parent_id: self
                .hierarchy
                .get(&id.0)
                .and_then(|a| a.first())
                .copied()
                .map(TenantId),
            self_managed: false,
        })
    }

    async fn get_root_tenant(
        &self,
        _ctx: &SecurityContext,
    ) -> Result<TenantInfo, TenantResolverError> {
        Err(TenantResolverError::Internal(
            "not used by oagw tests".to_owned(),
        ))
    }

    async fn get_tenants(
        &self,
        _ctx: &SecurityContext,
        _ids: &[TenantId],
        _options: &GetTenantsOptions,
    ) -> Result<Vec<TenantInfo>, TenantResolverError> {
        Ok(Vec::new())
    }

    async fn get_ancestors(
        &self,
        _ctx: &SecurityContext,
        id: TenantId,
        _options: &GetAncestorsOptions,
    ) -> Result<GetAncestorsResponse, TenantResolverError> {
        let ancestors = self
            .hierarchy
            .get(&id.0)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(Self::ref_of)
            .collect();
        Ok(GetAncestorsResponse {
            tenant: Self::ref_of(id.0),
            ancestors,
        })
    }

    async fn get_descendants(
        &self,
        _ctx: &SecurityContext,
        _id: TenantId,
        _options: &GetDescendantsOptions,
    ) -> Result<GetDescendantsResponse, TenantResolverError> {
        Err(TenantResolverError::Internal(
            "not used by oagw tests".to_owned(),
        ))
    }

    async fn is_ancestor(
        &self,
        _ctx: &SecurityContext,
        _ancestor_id: TenantId,
        _descendant_id: TenantId,
        _options: &IsAncestorOptions,
    ) -> Result<bool, TenantResolverError> {
        Err(TenantResolverError::Internal(
            "not used by oagw tests".to_owned(),
        ))
    }
}
