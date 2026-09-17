//! Composition root for the `oagw` gear.
//!
//! `init` builds the in-memory repositories, the control plane and the data
//! plane, resolves the three hard dependencies from the client hub and
//! provisions the GTS type-schemas; `register_rest` mounts the management and
//! proxy routes.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use toolkit::Gear;
use toolkit::GearCtx;
use toolkit::RestApiCapability;
use toolkit::api::OpenApiRegistry;
use types_registry_sdk::TypesRegistryClient;

use crate::config::OagwConfig;
use crate::infra::credentials::SecretResolver;
use crate::infra::storage::memory::MemoryStore;

#[toolkit::gear(
    name = "oagw",
    deps = [credstore, tenant_resolver, types_registry],
    capabilities = [rest]
)]
pub struct OagwGear {
    state: OnceLock<Arc<crate::api::rest::state::OagwState>>,
}

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            state: OnceLock::new(),
        }
    }
}

impl OagwGear {
    #[allow(
        clippy::unused_self,
        reason = "the state is shared through the OnceLock the attribute macro's registration path reads back"
    )]
    fn read_config(ctx: &GearCtx) -> anyhow::Result<Arc<OagwConfig>> {
        let config: OagwConfig = ctx
            .config_or_default()
            .map_err(|error| anyhow::anyhow!("oagw config invalid: {error}"))?;
        Ok(Arc::new(config))
    }

    /// Resolve a hard dependency, failing closed when the gear is missing.
    fn dependency<T: ?Sized + Send + Sync + 'static>(
        ctx: &GearCtx,
        name: &str,
    ) -> anyhow::Result<Arc<T>> {
        ctx.client_hub()
            .get::<T>()
            .map_err(|error| anyhow::anyhow!("failed to get {name}: {error}"))
    }
}

#[async_trait]
impl Gear for OagwGear {
    #[tracing::instrument(skip_all, fields(module = "oagw"))]
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let config = Self::read_config(ctx)?;
        tracing::info!(
            proxy_timeout_secs = config.proxy_timeout_secs,
            allow_http_upstream = config.allow_http_upstream,
            "initializing oagw module"
        );

        let _credentials =
            Self::dependency::<dyn credstore_sdk::CredStoreClientV1>(ctx, "CredStoreClientV1")?;
        let _tenants = Self::dependency::<dyn tenant_resolver_sdk::TenantResolverClient>(
            ctx,
            "TenantResolverClient",
        )?;
        let registry = Self::dependency::<dyn TypesRegistryClient>(ctx, "TypesRegistryClient")?;

        // Fail-closed: without the credential store no plugin secret resolves,
        // so the data plane must not come up. The resolved client backs the
        // secret resolver used by every plugin.
        let store = MemoryStore::new();
        let control_plane = Arc::new(
            crate::domain::services::management::ControlPlaneService::new(
                store.clone(),
                store.clone() as Arc<dyn crate::domain::repo::RouteRepository>,
                store as Arc<dyn crate::domain::repo::PluginRepository>,
            ),
        );
        let resolver = SecretResolver::new(_credentials);
        let state = Arc::new(crate::api::rest::state::OagwState::assemble(
            control_plane,
            &resolver,
            config,
        ));

        crate::infra::type_provisioning::provision(registry.as_ref())
            .await
            .map_err(|error| anyhow::anyhow!("oagw type provisioning failed: {error}"))?;

        self.state
            .set(state)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        tracing::info!("oagw module initialized");
        Ok(())
    }
}

impl RestApiCapability for OagwGear {
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: axum::Router,
        openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        tracing::info!("registering oagw REST routes");
        let state = self
            .state
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("oagw state not initialized"))?;
        let router = crate::api::rest::routes::register_routes(router, openapi, state);
        tracing::info!("oagw REST routes registered");
        Ok(router)
    }
}
