//! ToolKit gear wiring.
//!
//! `init` builds the Control Plane, the Data Plane and the plugin registries
//! and hands the REST layer a single shared state. `post_init` publishes the
//! GTS type catalog, which needs the types-registry to already be in ready
//! mode — hence the later phase.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use authz_resolver_sdk::{AuthZResolverClient, PolicyEnforcer};
use credstore_sdk::CredStoreClientV1;
use tenant_resolver_sdk::TenantResolverClient;
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::SystemCapability;
use toolkit::{Gear, GearCtx, RestApiCapability};
use tracing::{info, warn};
use types_registry_sdk::TypesRegistryClient;

use crate::api::rest::state::OagwState;
use crate::config::OagwConfig;
use crate::domain::services::ControlPlaneService;
use crate::domain::tenant::TenantDirectory;
use crate::infra::metrics::OagwMetrics;
use crate::infra::plugin::PluginRegistries;
use crate::infra::plugin::oauth2_client_cred_auth::TokenCacheConfig;
use crate::infra::proxy::DataPlaneService;
use crate::infra::proxy::connector::UpstreamConnector;
use crate::infra::rate_limit::RateLimiterRegistry;
use crate::infra::storage::InMemoryStore;
use crate::infra::tenant::TenantResolverDirectory;
use crate::infra::type_catalog;

/// The Outbound API Gateway gear.
#[toolkit::gear(
    name = "oagw",
    deps = [types_registry, tenant_resolver, authz_resolver, credstore],
    capabilities = [system, rest]
)]
pub struct OagwGear {
    state: OnceLock<Arc<OagwState>>,
    types: OnceLock<Arc<dyn TypesRegistryClient>>,
}

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            state: OnceLock::new(),
            types: OnceLock::new(),
        }
    }
}

impl std::fmt::Debug for OagwGear {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OagwGear")
            .field("initialized", &self.state.get().is_some())
            .finish()
    }
}

#[async_trait]
impl Gear for OagwGear {
    #[tracing::instrument(skip_all, fields(module = "oagw"))]
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let config: OagwConfig = ctx.config_or_default()?;
        config
            .validate()
            .map_err(|err| anyhow::anyhow!("oagw config invalid: {err}"))?;
        info!(
            proxy_timeout_secs = config.proxy_timeout_secs,
            allow_http_upstream = config.allow_http_upstream,
            ssrf_enabled = config.ssrf_policy.enabled,
            "initializing oagw gear"
        );

        // Fail closed on the credential store: an auth plugin that cannot
        // resolve a `cred://` reference is not a degraded gateway, it is one
        // that would forward uncredentialed requests to a third party.
        let credstore = ctx
            .client_hub()
            .get::<dyn CredStoreClientV1>()
            .map_err(|err| anyhow::anyhow!("failed to get CredStoreClientV1: {err}"))?;

        let tenants: Arc<dyn TenantDirectory> =
            match ctx.client_hub().get::<dyn TenantResolverClient>() {
                Ok(client) => Arc::new(TenantResolverDirectory::new(
                    client,
                    config.tenant_cache_ttl_secs,
                )),
                Err(err) => {
                    // Alias shadowing degrades to single-tenant resolution
                    // rather than refusing to start.
                    warn!(
                        error = %err,
                        "tenant-resolver client unavailable; alias resolution will not walk the \
                         tenant hierarchy"
                    );
                    Arc::new(crate::domain::tenant::FlatTenantDirectory)
                }
            };

        let authz = match ctx.client_hub().get::<dyn AuthZResolverClient>() {
            Ok(client) => Some(PolicyEnforcer::new(client)),
            Err(err) => {
                warn!(error = %err, "authz-resolver client unavailable; policy checks disabled");
                None
            }
        };

        if let Ok(client) = ctx.client_hub().get::<dyn TypesRegistryClient>() {
            let _ = self.types.set(client);
        } else {
            warn!("types-registry client unavailable; the OAGW type catalog will not be published");
        }

        let store = InMemoryStore::shared();
        let control = Arc::new(ControlPlaneService::new(
            Arc::clone(&store) as Arc<dyn crate::domain::repo::UpstreamRepository>,
            Arc::clone(&store) as Arc<dyn crate::domain::repo::RouteRepository>,
            Arc::clone(&store) as Arc<dyn crate::domain::repo::PluginRepository>,
            tenants,
        ));

        let registries = Arc::new(PluginRegistries::with_builtins(
            credstore,
            TokenCacheConfig {
                ttl: config.token_cache_ttl(),
                capacity: config.token_cache_capacity,
            },
        ));

        let data_plane = Arc::new(DataPlaneService::new(
            Arc::clone(&control),
            UpstreamConnector::shared(&config),
            registries,
            Arc::clone(&store) as Arc<dyn crate::domain::repo::PluginRepository>,
            Arc::new(RateLimiterRegistry::new()),
            Arc::new(OagwMetrics::from_global()),
            config.clone(),
        ));

        let state = Arc::new(OagwState {
            control,
            data_plane,
            store,
            authz,
            config,
        });
        self.state
            .set(state)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        info!("oagw gear initialized");
        Ok(())
    }
}

#[async_trait]
impl SystemCapability for OagwGear {
    async fn post_init(&self, _sys: &toolkit::runtime::SystemContext) -> anyhow::Result<()> {
        if let Some(client) = self.types.get() {
            type_catalog::register_catalog(client).await;
        }
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
        let state = self
            .state
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("oagw state not initialized"))?;
        let router = crate::api::rest::register_routes(router, openapi, state);
        info!("oagw REST routes registered under /oagw/v1");
        Ok(router)
    }
}
