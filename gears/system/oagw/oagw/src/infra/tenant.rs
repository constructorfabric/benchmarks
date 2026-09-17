//! Tenant-hierarchy adapter.
//!
//! Wraps the tenant-resolver client so the control plane and data plane can
//! walk the tenant chain (descendant → root) for alias resolution and
//! hierarchical configuration inheritance.
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::error::{OagwError, OagwResult};
use tenant_resolver_sdk::GetAncestorsOptions;

/// Tenant chain: caller's tenant first, then ancestors up to the root.
#[derive(Debug, Clone)]
pub struct TenantChain {
    /// Caller's tenant id.
    pub self_id: Uuid,
    /// Ancestors ordered nearest-parent first, ending at the root. Empty when
    /// the caller's tenant is the root.
    pub ancestors: Vec<Uuid>,
}

impl TenantChain {
    /// All ids in walk order (self, then ancestors).
    #[must_use]
    pub fn ids(&self) -> Vec<Uuid> {
        let mut ids = vec![self.self_id];
        ids.extend(self.ancestors.iter().copied());
        ids
    }
}

/// Cached tenant-chain resolver.
#[derive(Clone)]
pub struct TenantHierarchy {
    resolver: Option<Arc<dyn tenant_resolver_sdk::TenantResolverClient>>,
    cache: Arc<RwLock<HashMap<Uuid, (Instant, TenantChain)>>>,
    ttl: Duration,
}

impl std::fmt::Debug for TenantHierarchy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TenantHierarchy")
            .field("has_resolver", &self.resolver.is_some())
            .field("ttl", &self.ttl)
            .finish()
    }
}

impl TenantHierarchy {
    /// Builds a hierarchy resolver around the tenant-resolver client.
    ///
    /// When `resolver` is `None` every lookup degenerates to a single-tenant
    /// chain (the caller's tenant only), which keeps the gear usable in
    /// deployments without a tenant-resolver dependency.
    #[must_use]
    pub fn new(resolver: Option<Arc<dyn tenant_resolver_sdk::TenantResolverClient>>, ttl: Duration) -> Self {
        Self {
            resolver,
            cache: Arc::new(RwLock::new(HashMap::new())),
            ttl,
        }
    }

    /// Resolves the tenant chain for `tenant_id`.
    ///
    /// # Errors
    ///
    /// [`OagwError::RouteNotFound`] when the tenant does not exist,
    /// [`OagwError::Internal`] when the resolver fails.
    pub async fn chain(&self, ctx: &SecurityContext, tenant_id: Uuid) -> OagwResult<TenantChain> {
        if let Some(chain) = self.cached(tenant_id) {
            return Ok(chain);
        }
        let Some(resolver) = self.resolver.clone() else {
            return Ok(TenantChain {
                self_id: tenant_id,
                ancestors: Vec::new(),
            });
        };
        let response = resolver
            .get_ancestors(ctx, tenant_resolver_sdk::TenantId(tenant_id), &GetAncestorsOptions::default())
            .await
            .map_err(|err| match err {
                tenant_resolver_sdk::TenantResolverError::TenantNotFound { tenant_id } => {
                    OagwError::RouteNotFound(format!("tenant '{}' not found", tenant_id.0))
                }
                other => OagwError::Internal(format!("tenant resolver failed: {other}")),
            })?;
        let chain = TenantChain {
            self_id: tenant_id,
            ancestors: response.ancestors.iter().map(|t| t.id.0).collect(),
        };
        self.cache.write().insert(tenant_id, (Instant::now(), chain.clone()));
        Ok(chain)
    }

    fn cached(&self, tenant_id: Uuid) -> Option<TenantChain> {
        let guard = self.cache.read();
        match guard.get(&tenant_id) {
            Some((at, chain)) if at.elapsed() < self.ttl => Some(chain.clone()),
            _ => None,
        }
    }

    /// Drops cached chains (used by tests and config invalidation).
    pub fn invalidate(&self) {
        self.cache.write().clear();
    }
}
