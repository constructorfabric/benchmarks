//! OAGW gear wiring: dependency resolution, component construction, and
//! REST route registration.
//!
//! The gear declares hard dependencies on the tenant resolver (to
//! resolve the caller's tenant chain), the types registry (GTS metadata
//! for upstream/route/plugin references), and the credential store
//! (secret resolution for auth plugins). It hosts no SDK clients of its
//! own — everything OAGW offers is HTTP-surface only.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use toolkit::{Gear, GearCtx, RestApiCapability};
use tracing::info;

use crate::api::rest::routes;
use crate::config::OagwConfig;
use crate::domain::services::ControlPlane;
use crate::infra::plugins::PluginEngine;
use crate::infra::proxy::DataPlane;
use crate::infra::storage::MemoryStore;

/// Main gear struct for the OAGW gear.
#[toolkit::gear(
    name = "oagw",
    deps = [tenant_resolver, types_registry, credstore],
    capabilities = [rest]
)]
#[allow(clippy::struct_field_names)]
pub struct OagwGear {
    control_plane: OnceLock<Arc<ControlPlane>>,
    data_plane: OnceLock<Arc<DataPlane>>,
}

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            control_plane: OnceLock::new(),
            data_plane: OnceLock::new(),
        }
    }
}

#[async_trait]
impl Gear for OagwGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let config = Arc::new(ctx.config_or_default::<OagwConfig>()?);

        let credstore: Arc<dyn credstore_sdk::CredStoreClientV1> = ctx
            .client_hub()
            .get::<dyn credstore_sdk::CredStoreClientV1>()
            .map_err(|e| anyhow::anyhow!("failed to get CredStoreClientV1: {e}"))?;

        let types_registry: Arc<dyn types_registry_sdk::TypesRegistryClient> = ctx
            .client_hub()
            .get::<dyn types_registry_sdk::TypesRegistryClient>()
            .map_err(|e| anyhow::anyhow!("failed to get TypesRegistryClient: {e}"))?;

        let tenants: Arc<dyn tenant_resolver_sdk::TenantResolverClient> = ctx
            .client_hub()
            .get::<dyn tenant_resolver_sdk::TenantResolverClient>()
            .map_err(|e| anyhow::anyhow!("failed to get TenantResolverClient: {e}"))?;

        let store = Arc::new(MemoryStore::new());
        let plugins = Arc::new(PluginEngine::new(credstore, &config));

        let control_plane = Arc::new(ControlPlane::new(
            store.clone(),
            config.clone(),
            types_registry,
        ));
        self.control_plane
            .set(control_plane.clone())
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        let data_plane = Arc::new(
            DataPlane::new(store, tenants, plugins, config)
                .map_err(|e| anyhow::anyhow!("failed to build OAGW data plane: {e:?}"))?,
        );
        self.data_plane
            .set(data_plane.clone())
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        info!("OAGW gear initialized (control plane + data plane)");
        Ok(())
    }
}

impl RestApiCapability for OagwGear {
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: axum::Router,
        openapi: &dyn toolkit::api::OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        info!("Registering OAGW REST routes");

        let control_plane = self
            .control_plane
            .get()
            .ok_or_else(|| anyhow::anyhow!("ControlPlane not initialized"))?
            .clone();

        let data_plane = self
            .data_plane
            .get()
            .ok_or_else(|| anyhow::anyhow!("DataPlane not initialized"))?
            .clone();

        let router = routes::register_routes(router, openapi, control_plane, data_plane);

        info!("OAGW REST routes registered successfully");
        Ok(router)
    }
}

