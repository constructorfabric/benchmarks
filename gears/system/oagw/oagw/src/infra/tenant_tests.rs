//! Unit tests for [`super::tenant`].

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use async_trait::async_trait;
use toolkit_security::SecurityContext;
use tenant_resolver_sdk::error::TenantResolverError;
use tenant_resolver_sdk::models::{
    GetAncestorsOptions, GetAncestorsResponse, GetDescendantsOptions, GetDescendantsResponse,
    GetTenantsOptions, IsAncestorOptions, TenantId, TenantInfo, TenantRef, TenantStatus,
};
use tenant_resolver_sdk::TenantResolverClient;
use uuid::Uuid;

use super::fallback_chain;
use super::TenantChainResolver;

/// In-memory resolver returning a fixed ancestor chain.
#[derive(Debug)]
struct StubResolver {
    ancestors: Vec<Uuid>,
}

fn reference(id: TenantId) -> TenantRef {
    TenantRef {
        id,
        status: TenantStatus::Active,
        tenant_type: None,
        parent_id: None,
        self_managed: false,
    }
}

fn unknown(id: TenantId) -> TenantResolverError {
    TenantResolverError::TenantNotFound { tenant_id: id }
}

#[async_trait]
impl TenantResolverClient for StubResolver {
    async fn get_tenant(
        &self,
        _ctx: &SecurityContext,
        id: TenantId,
    ) -> Result<TenantInfo, TenantResolverError> {
        Err(unknown(id))
    }

    async fn get_root_tenant(
        &self,
        ctx: &SecurityContext,
    ) -> Result<TenantInfo, TenantResolverError> {
        Err(unknown(TenantId(ctx.subject_tenant_id())))
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
        Ok(GetAncestorsResponse {
            tenant: reference(id),
            ancestors: self.ancestors.iter().map(|id| reference(TenantId(*id))).collect(),
        })
    }

    async fn get_descendants(
        &self,
        _ctx: &SecurityContext,
        id: TenantId,
        _options: &GetDescendantsOptions,
    ) -> Result<GetDescendantsResponse, TenantResolverError> {
        Err(unknown(id))
    }

    async fn is_ancestor(
        &self,
        _ctx: &SecurityContext,
        _ancestor: TenantId,
        _descendant: TenantId,
        _options: &IsAncestorOptions,
    ) -> Result<bool, TenantResolverError> {
        Ok(false)
    }
}

fn ctx(tenant: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(tenant)
        .build()
        .unwrap()
}

#[tokio::test]
async fn the_chain_degrades_to_the_calling_tenant_without_a_resolver() {
    let tenant = Uuid::new_v4();
    let resolver = TenantChainResolver::new(None);
    assert_eq!(resolver.chain(&ctx(tenant)).await, vec![tenant]);
}

#[tokio::test]
async fn the_chain_is_ordered_leaf_first() {
    let leaf = Uuid::new_v4();
    let root = Uuid::new_v4();
    let resolver = TenantChainResolver::new(Some(Arc::new(StubResolver {
        ancestors: vec![root],
    })));
    assert_eq!(resolver.chain(&ctx(leaf)).await, vec![leaf, root]);
}

#[tokio::test]
async fn a_resolver_failure_degrades_to_the_calling_tenant() {
    struct FailingResolver;

    #[async_trait]
    impl TenantResolverClient for FailingResolver {
        async fn get_tenant(
            &self,
            _ctx: &SecurityContext,
            id: TenantId,
        ) -> Result<TenantInfo, TenantResolverError> {
            Err(unknown(id))
        }

        async fn get_root_tenant(
            &self,
            _ctx: &SecurityContext,
        ) -> Result<TenantInfo, TenantResolverError> {
            Err(TenantResolverError::Unauthorized)
        }

        async fn get_tenants(
            &self,
            _ctx: &SecurityContext,
            _ids: &[TenantId],
            _options: &GetTenantsOptions,
        ) -> Result<Vec<TenantInfo>, TenantResolverError> {
            Err(TenantResolverError::Unauthorized)
        }

        async fn get_ancestors(
            &self,
            _ctx: &SecurityContext,
            _id: TenantId,
            _options: &GetAncestorsOptions,
        ) -> Result<GetAncestorsResponse, TenantResolverError> {
            Err(TenantResolverError::Unauthorized)
        }

        async fn get_descendants(
            &self,
            _ctx: &SecurityContext,
            _id: TenantId,
            _options: &GetDescendantsOptions,
        ) -> Result<GetDescendantsResponse, TenantResolverError> {
            Err(TenantResolverError::Unauthorized)
        }

        async fn is_ancestor(
            &self,
            _ctx: &SecurityContext,
            _ancestor: TenantId,
            _descendant: TenantId,
            _options: &IsAncestorOptions,
        ) -> Result<bool, TenantResolverError> {
            Err(TenantResolverError::Unauthorized)
        }
    }

    let leaf = Uuid::new_v4();
    let resolver = TenantChainResolver::new(Some(Arc::new(FailingResolver)));
    assert_eq!(resolver.chain(&ctx(leaf)).await, vec![leaf]);
}

#[test]
fn the_fallback_chain_contains_a_single_tenant() {
    let tenant = Uuid::new_v4();
    assert_eq!(fallback_chain(tenant), vec![tenant]);
}
