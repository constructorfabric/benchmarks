//! Tenant-hierarchy access for the data plane.
//!
//! Alias resolution walks descendant → root and the closest match wins, so the
//! data plane needs the caller's ancestor chain. The trait keeps that walk out
//! of the resolution loop and lets tests supply a fixed hierarchy.

use std::sync::Arc;

use async_trait::async_trait;
use uuid::Uuid;

/// Answers "which tenants are above this one".
#[async_trait]
pub trait TenantHierarchy: Send + Sync {
    /// Returns the tenant chain, closest first, including the tenant itself.
    ///
    /// # Errors
    /// Returns an error when the hierarchy cannot be read.
    async fn chain(&self, tenant_id: Uuid) -> anyhow::Result<Vec<Uuid>>;
}

/// Implementation over the `tenant-resolver` SDK.
pub struct SdkTenantHierarchy {
    client: Arc<dyn tenant_resolver_sdk::TenantResolverClient>,
}

impl SdkTenantHierarchy {
    /// Builds the hierarchy reader over a resolved SDK client.
    #[must_use]
    pub fn new(client: Arc<dyn tenant_resolver_sdk::TenantResolverClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl TenantHierarchy for SdkTenantHierarchy {
    async fn chain(&self, tenant_id: Uuid) -> anyhow::Result<Vec<Uuid>> {
        let context = toolkit_security::SecurityContext::anonymous();
        let response = self
            .client
            .get_ancestors(
                &context,
                tenant_resolver_sdk::TenantId(tenant_id),
                &tenant_resolver_sdk::GetAncestorsOptions::default(),
            )
            .await?;
        let mut chain = Vec::with_capacity(response.ancestors.len() + 1);
        chain.push(response.tenant.id.0);
        for ancestor in response.ancestors {
            chain.push(ancestor.id.0);
        }
        Ok(chain)
    }
}

/// Whether a hierarchy read failed because the tenant itself is unknown.
///
/// A caller whose tenant the resolver does not know cannot resolve any alias,
/// which is a `404` rather than a gateway fault; only the resolver being
/// unreachable or broken is a `502`.
#[must_use]
pub fn is_unknown_tenant(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<tenant_resolver_sdk::TenantResolverError>())
        .any(|error| {
            matches!(
                error,
                tenant_resolver_sdk::TenantResolverError::TenantNotFound { .. }
                    | tenant_resolver_sdk::TenantResolverError::Unauthorized
            )
        })
}

/// Implementation over a client hub.
///
/// The tenant-resolver gear registers its client after this gear initializes,
/// so the client is probed per lookup rather than captured at init; the probe
/// is a read-locked map read, not a network call. Until it appears, every
/// tenant stands alone.
pub struct HubTenantHierarchy {
    hub: Arc<toolkit::client_hub::ClientHub>,
}

impl HubTenantHierarchy {
    /// Builds the hierarchy reader over a client hub.
    #[must_use]
    pub fn new(hub: Arc<toolkit::client_hub::ClientHub>) -> Self {
        Self { hub }
    }
}

#[async_trait]
impl TenantHierarchy for HubTenantHierarchy {
    async fn chain(&self, tenant_id: Uuid) -> anyhow::Result<Vec<Uuid>> {
        match self.hub.try_get::<dyn tenant_resolver_sdk::TenantResolverClient>() {
            Some(client) => SdkTenantHierarchy::new(client).chain(tenant_id).await,
            None => Ok(vec![tenant_id]),
        }
    }
}

/// A hierarchy fixed at construction, for tests.
#[derive(Debug, Default)]
pub struct StaticTenantHierarchy {
    /// Tenant id → its chain, closest first, including itself.
    pub chains: std::collections::BTreeMap<Uuid, Vec<Uuid>>,
}

impl StaticTenantHierarchy {
    /// Builds a hierarchy where every tenant is its own root unless listed.
    #[must_use]
    pub fn from_pairs(pairs: &[(Uuid, Uuid)]) -> Arc<Self> {
        let mut map = std::collections::BTreeMap::new();
        for (child, parent) in pairs {
            let entry = map.entry(*child).or_insert_with(|| vec![*child]);
            entry.push(*parent);
        }
        Arc::new(Self {
            chains: map,
        })
    }
}

#[async_trait]
impl TenantHierarchy for StaticTenantHierarchy {
    async fn chain(&self, tenant_id: Uuid) -> anyhow::Result<Vec<Uuid>> {
        Ok(self
            .chains
            .get(&tenant_id)
            .cloned()
            .unwrap_or_else(|| vec![tenant_id]))
    }
}
