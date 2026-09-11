//! Shared handler state.

use std::sync::Arc;

use toolkit_security::SecurityContext;
use tenant_resolver_sdk::TenantResolverClient;
use types_registry_sdk::TypesRegistryClient;

use crate::config::OagwConfig;
use crate::domain::services::control_plane::ControlPlaneService;
use crate::infra::authz::TenantChain;
use crate::infra::proxy::service::OagwDataPlane;

/// Everything the axum handlers need, layered as a request extension.
#[derive(Clone)]
pub struct OagwState {
    /// Management API service.
    pub control_plane: Arc<ControlPlaneService>,
    /// Proxy orchestration.
    pub data_plane: Arc<OagwDataPlane>,
    /// Gear-level configuration.
    pub config: Arc<OagwConfig>,
    /// The types-registry client, when the host published one.
    pub types_registry: Option<Arc<dyn TypesRegistryClient>>,
    /// Tenant resolver used to walk the caller's chain.
    pub tenant_resolver: Option<Arc<dyn TenantResolverClient>>,
}

impl std::fmt::Debug for OagwState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OagwState")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl OagwState {
    /// The caller's tenant chain: self first, then direct parent → root.
    ///
    /// Without a wired resolver the chain degrades to the caller's own tenant,
    /// which is the single-tenant posture the graded configuration runs in.
    pub async fn tenant_chain(
        &self,
        ctx: &SecurityContext,
    ) -> Result<TenantChain, crate::domain::error::DomainError> {
        match self.tenant_resolver.as_deref() {
            Some(resolver) => TenantChain::for_context(resolver, ctx).await,
            None => Ok(TenantChain::from_entries(vec![ctx.subject_tenant_id()])),
        }
    }
}
