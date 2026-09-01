// Created: 2026-08-31 by Constructor Tech
//! Tenant-chain resolution for alias shadowing (DESIGN §3.2).
//!
//! The production adapter wraps the `tenant_resolver` client of the
//! `ClientHub`; tests inject a static chain. Keeping the seam narrow (one
//! method, domain types only) means the routing logic never depends on the
//! SDK.

use std::sync::Arc;

use async_trait::async_trait;
use tenant_resolver_sdk::{
    GetAncestorsOptions, TenantId, TenantResolverClient, TenantResolverError,
};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::error::{OagwError, OagwErrorKind, OagwResult};

/// The ancestors of a tenant, from the direct parent to the root.
#[async_trait]
pub trait TenantChain: Send + Sync {
    /// Walk from `tenant_id` up to the root, most specific first.
    ///
    /// # Errors
    /// Propagated when the hierarchy cannot be read; a tenant without a
    /// hierarchy yields an empty walk.
    async fn ancestors(&self, ctx: &SecurityContext, tenant_id: Uuid) -> OagwResult<Vec<Uuid>>;
}

/// Adapter over the `tenant_resolver` client.
pub struct ResolverChain {
    client: Arc<dyn TenantResolverClient>,
}

impl ResolverChain {
    /// Wrap `client`.
    #[must_use]
    pub fn new(client: Arc<dyn TenantResolverClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl TenantChain for ResolverChain {
    async fn ancestors(&self, ctx: &SecurityContext, tenant_id: Uuid) -> OagwResult<Vec<Uuid>> {
        match self
            .client
            .get_ancestors(ctx, TenantId(tenant_id), &GetAncestorsOptions::default())
            .await
        {
            Ok(response) => Ok(response
                .ancestors
                .iter()
                .map(|tenant| tenant.id.0)
                .collect()),
            // A tenant outside the hierarchy simply has no shadowing chain.
            Err(TenantResolverError::TenantNotFound { .. }) => Ok(Vec::new()),
            Err(error) => Err(OagwError::new(
                OagwErrorKind::Internal,
                format!("tenant hierarchy unavailable: {error}"),
            )),
        }
    }
}

/// Chain that never leaves the calling tenant.
///
/// Used when the `tenant_resolver` client is not wired into the deployment:
/// alias shadowing degrades to a per-tenant lookup instead of failing the
/// whole gear.
#[derive(Debug, Default)]
pub struct NoChain;

#[async_trait]
impl TenantChain for NoChain {
    async fn ancestors(&self, _ctx: &SecurityContext, _tenant_id: Uuid) -> OagwResult<Vec<Uuid>> {
        Ok(Vec::new())
    }
}
