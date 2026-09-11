//! The OAGW gear: management API and proxy data plane.

use std::sync::{Arc, OnceLock};

use toolkit::RestApiCapability;
use toolkit::api::OpenApiRegistry;
use toolkit::{Gear, GearCtx};
use tracing::info;

use crate::api::error::ApiContext;
use crate::config::OagwConfig;
use crate::domain::service::ControlPlaneService;
use crate::infra::memory_repo::{
    MemoryPluginRepository, MemoryRouteRepository, MemoryUpstreamRepository,
};
use crate::infra::plugin::oauth2_client_cred_auth::TokenCacheConfig;
use crate::infra::plugin::{AuthPluginRegistry, GuardPluginRegistry, TransformPluginRegistry};
use crate::infra::proxy::service::ProxyService;

/// The outbound API gateway.
///
/// `init` builds the control plane, the plugin registries and the proxy data
/// plane; `register_rest` mounts the management API and the proxy endpoint.
#[toolkit::gear(
    name = "oagw",
    deps = [tenant_resolver],
    capabilities = [rest],
)]
pub struct OagwGear {
    /// Assembled REST context; set during [`Gear::init`].
    context: OnceLock<Arc<ApiContext>>,
    /// Effective configuration; set during [`Gear::init`].
    config: OnceLock<Arc<OagwConfig>>,
}

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            context: OnceLock::new(),
            config: OnceLock::new(),
        }
    }
}

impl OagwGear {
    /// The API context built during init.
    ///
    /// # Errors
    /// Returns an error when init has not run yet.
    pub fn context(&self) -> anyhow::Result<Arc<ApiContext>> {
        self.context
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("oagw: API context is unavailable before Gear::init"))
    }

    /// The effective configuration.
    ///
    /// # Errors
    /// Returns an error when init has not run.
    pub fn config(&self) -> anyhow::Result<Arc<OagwConfig>> {
        self.config
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("oagw: configuration is unavailable before Gear::init"))
    }
}

#[async_trait::async_trait]
impl Gear for OagwGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let config: OagwConfig = ctx.config_or_default()?;
        config
            .validate()
            .map_err(|err| anyhow::anyhow!("oagw: invalid configuration: {err}"))?;
        let config = Arc::new(config);

        let control_plane = Arc::new(ControlPlaneService::new(
            Arc::new(MemoryUpstreamRepository::default()),
            Arc::new(MemoryRouteRepository::default()),
            Arc::new(MemoryPluginRepository::default()),
            config.allow_http_upstream,
        ));

        let tenants = ctx
            .client_hub()
            .try_get::<dyn tenant_resolver_sdk::TenantResolverClient>();
        let credstore = ctx
            .client_hub()
            .try_get::<dyn credstore_sdk::CredStoreClientV1>();

        // Auth plugins degrade gracefully when no credential store is wired:
        // key lookups then simply fail to resolve.
        let auth_plugins = AuthPluginRegistry::with_builtins(
            credstore.unwrap_or_else(|| {
                Arc::new(crate::infra::plugin::absent_credstore::AbsentCredStore)
            }),
            TokenCacheConfig {
                ttl: std::time::Duration::from_secs(config.token_cache_ttl_secs),
                capacity: config.token_cache_capacity,
            },
        );
        let guard_plugins = GuardPluginRegistry::with_builtins();
        let transform_plugins = TransformPluginRegistry::with_builtins();

        let client = crate::infra::http_client::build_client(config.proxy_timeout());
        let proxy = Arc::new(ProxyService::new(
            Arc::clone(&control_plane),
            Arc::clone(&config),
            client,
            auth_plugins,
            guard_plugins,
            transform_plugins,
        ));

        let context = Arc::new(ApiContext {
            control_plane,
            config: Arc::clone(&config),
            proxy,
            tenants,
        });

        drop(self.config.set(Arc::clone(&config)));
        drop(self.context.set(context));
        info!(
            allow_http_upstream = config.allow_http_upstream,
            "oagw gear initialized"
        );
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
        let context = self.context()?;
        info!("registering oagw REST routes");
        Ok(crate::api::routes::register_routes(
            router, openapi, context,
        ))
    }
}
