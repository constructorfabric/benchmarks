//! Gear definition: initialization and REST registration.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use toolkit::api::OpenApiRegistry;
use toolkit::{Gear, GearCtx};
use tracing::info;

use crate::api::rest::routes;
use crate::config::OagwConfig;
use crate::domain::service::ControlPlaneService;
use crate::infra::credstore::CredStoreSecretResolver;
use crate::infra::state::{GearState, ResolverTenantChain, StaticTenantChain};
use crate::infra::store::InMemoryStore;
use crate::proxy::data_plane::DataPlane;
use crate::proxy::ratelimit::{RateLimiter, SharedRateLimiter};

/// OAGW — outbound API gateway gear.
///
/// Holds the shared [`GearState`], the control-plane service and the data
/// plane behind `OnceLock`s populated during `init`.
#[toolkit::gear(
    name = "oagw",
    deps = [credstore, types_registry, authz_resolver, tenant_resolver],
    capabilities = [rest]
)]
pub struct Oagw {
    state: OnceLock<Arc<GearState>>,
    service: OnceLock<Arc<ControlPlaneService>>,
    plane: OnceLock<Arc<DataPlane>>,
}

impl Default for Oagw {
    fn default() -> Self {
        Self {
            state: OnceLock::new(),
            service: OnceLock::new(),
            plane: OnceLock::new(),
        }
    }
}

impl Oagw {
    /// The shared gear state, if initialized.
    #[must_use]
    pub fn state(&self) -> Option<Arc<GearState>> {
        self.state.get().cloned()
    }

    /// The control-plane service, if initialized.
    #[must_use]
    pub fn service(&self) -> Option<Arc<ControlPlaneService>> {
        self.service.get().cloned()
    }

    /// Builds the tenant-chain provider, preferring the resolver client.
    fn tenant_chain(ctx: &GearCtx) -> anyhow::Result<Arc<dyn crate::infra::state::TenantChain>> {
        match ctx
            .client_hub()
            .get::<dyn tenant_resolver_sdk::TenantResolverClient>()
        {
            Ok(client) => Ok(Arc::new(ResolverTenantChain::new(client))),
            Err(_) => {
                info!("oagw: no tenant-resolver client; falling back to a single-tenant chain");
                Ok(Arc::new(StaticTenantChain))
            }
        }
    }

    /// Builds the credential resolver, preferring the CredStore client.
    fn secret_resolver(
        ctx: &GearCtx,
    ) -> anyhow::Result<Arc<dyn crate::domain::plugin::SecretResolver>> {
        if let Ok(credstore) = ctx
            .client_hub()
            .get::<dyn credstore_sdk::CredStoreClientV1>()
        {
            return Ok(Arc::new(CredStoreSecretResolver::new(credstore)));
        }
        info!("oagw: no credstore client; secret references will not resolve");
        Ok(Arc::new(crate::infra::credstore::UnresolvedSecretResolver))
    }
}

#[async_trait]
impl Gear for Oagw {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let config: OagwConfig = ctx
            .config_or_default()
            .map_err(|err| anyhow::anyhow!("failed to read oagw gear config: {err}"))?;

        let secrets = Self::secret_resolver(ctx)?;
        let tenants = Self::tenant_chain(ctx)?;
        let store = InMemoryStore::new();
        let state = Arc::new(GearState::from_parts(
            config,
            store.clone(),
            secrets,
            tenants,
        ));
        let limiter = SharedRateLimiter::new(Arc::new(RateLimiter::new()));
        let plane = Arc::new(DataPlane::new((*state).clone(), limiter));
        let service = Arc::new(ControlPlaneService::new(store));

        self.state
            .set(state)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;
        self.plane
            .set(plane)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;
        self.service
            .set(service)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        info!("oagw gear initialized (management + proxy surface)");
        Ok(())
    }
}

impl toolkit::RestApiCapability for Oagw {
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: axum::Router,
        openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        let state = self
            .state
            .get()
            .ok_or_else(|| anyhow::anyhow!("oagw state not initialized"))?
            .clone();
        let service = self
            .service
            .get()
            .ok_or_else(|| anyhow::anyhow!("oagw control-plane service not initialized"))?
            .clone();
        let plane = self
            .plane
            .get()
            .ok_or_else(|| anyhow::anyhow!("oagw data plane not initialized"))?
            .clone();
        let limiter = SharedRateLimiter::new(Arc::new(RateLimiter::new()));

        let router = routes::register_routes(
            router,
            openapi,
            (*state).clone(),
            (*service).clone(),
            plane,
            limiter,
        );
        info!("oagw REST routes registered");
        Ok(router)
    }
}
