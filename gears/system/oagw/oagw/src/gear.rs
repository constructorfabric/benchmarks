//! ToolKit gear wiring.
//!
//! Registers OAGW with the host runtime: resolves its dependencies from the
//! client hub during `init`, publishes its REST surface during the REST
//! phase, provisions its GTS type catalog in `post_init`, and runs the custom
//! plugin garbage collector on a background tick.

use std::future::Future;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use credstore_sdk::CredStoreClientV1;
use tenant_resolver_sdk::TenantResolverClient;
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::SystemCapability;
use toolkit::{Gear, GearCtx, RestApiCapability};
use tracing::{info, warn};
use types_registry_sdk::TypesRegistryClient;

use crate::api::rest::register_routes;
use crate::config::OagwConfig;
use crate::domain::ports::{PluginCatalog, TenantDirectory};
use crate::domain::services::management::ControlPlaneService;
use crate::infra::metrics::OagwMetrics;
use crate::infra::plugin::TokenCacheConfig;
use crate::infra::plugin::registry::PluginRegistries;
use crate::infra::proxy::circuit::CircuitBreakerRegistry;
use crate::infra::proxy::connector::UpstreamConnector;
use crate::infra::proxy::service::DataPlaneService;
use crate::infra::ratelimit::RateLimiterRegistry;
use crate::infra::storage::{MemoryPluginRepo, MemoryRouteRepo, MemoryStore, MemoryUpstreamRepo};
use crate::infra::tenant::{FlatTenantDirectory, TenantResolverDirectory};
use crate::infra::type_catalog;

/// How often the unlinked-plugin garbage collector runs.
const GC_TICK: Duration = Duration::from_secs(3600);

/// Wired state, built once during `init`.
struct Wiring {
    control_plane: Arc<ControlPlaneService>,
    data_plane: Arc<DataPlaneService>,
    config: OagwConfig,
}

/// The OAGW gear.
#[toolkit::gear(
    name = "oagw",
    deps = [types_registry, tenant_resolver, authz_resolver, credstore],
    capabilities = [system, rest]
)]
pub struct OagwGear {
    wiring: OnceLock<Arc<Wiring>>,
    registry: OnceLock<Arc<dyn TypesRegistryClient>>,
}

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            wiring: OnceLock::new(),
            registry: OnceLock::new(),
        }
    }
}

impl OagwGear {
    /// Sweep unlinked custom plugins on a fixed tick, for as long as the
    /// runtime's cancellation token is live.
    ///
    /// Plugins become GC-eligible one sweep after they fall out of every
    /// binding and are deleted on the first sweep past `gc_eligible_at`
    /// (default TTL 30 days), so a plugin that is re-bound in the meantime is
    /// never lost.
    fn spawn_plugin_gc(wiring: Arc<Wiring>, cancel: impl Future<Output = ()> + Send + 'static) {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(GC_TICK);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut cancel = std::pin::pin!(cancel);
            info!(
                target: "oagw.lifecycle",
                gc_tick_secs = GC_TICK.as_secs(),
                plugin_gc_ttl_days = wiring.config.plugin_gc_ttl_days,
                "oagw plugin garbage collector started"
            );
            loop {
                tokio::select! {
                    biased;
                    () = &mut cancel => break,
                    _ = interval.tick() => {
                        let linked = wiring.control_plane.linked_plugin_ids().await;
                        let deleted = wiring
                            .control_plane
                            .plugin_repo()
                            .collect_garbage(&linked, wiring.config.plugin_gc_ttl_days)
                            .await;
                        if deleted > 0 {
                            info!(
                                target: "oagw.lifecycle",
                                deleted,
                                "collected unlinked custom plugins"
                            );
                        }
                    }
                }
            }
            info!(target: "oagw.lifecycle", "oagw plugin garbage collector stopped");
        });
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
        if config.allow_http_upstream {
            warn!(
                target: "oagw.lifecycle",
                "allow_http_upstream is enabled: plaintext upstream connections are permitted"
            );
        }
        if !config.ssrf_policy.enabled {
            warn!(
                target: "oagw.lifecycle",
                "the SSRF guard is disabled: upstream endpoints may resolve to internal addresses"
            );
        }

        // Fail-closed on the credential store: without it no auth plugin can
        // inject anything, so every authenticated upstream would answer 500
        // for the lifetime of the process.
        let credstore = ctx
            .client_hub()
            .get::<dyn CredStoreClientV1>()
            .map_err(|err| anyhow::anyhow!("failed to get CredStoreClientV1: {err}"))?;

        let registries = Arc::new(PluginRegistries::with_builtins(
            credstore,
            TokenCacheConfig {
                ttl: config.token_cache_ttl(),
                capacity: config.token_cache_capacity,
            },
        ));

        // The tenant directory is degradable: without it OAGW still serves
        // each tenant's own configuration, it just cannot inherit an
        // ancestor's.
        let tenants: Arc<dyn TenantDirectory> =
            match ctx.client_hub().get::<dyn TenantResolverClient>() {
                Ok(client) => Arc::new(TenantResolverDirectory::new(
                    client,
                    config.ancestor_cache_ttl(),
                )),
                Err(err) => {
                    warn!(
                        target: "oagw.lifecycle",
                        error = %err,
                        "tenant-resolver client unavailable; hierarchy inheritance is disabled"
                    );
                    Arc::new(FlatTenantDirectory)
                }
            };

        let store = MemoryStore::shared();
        let control_plane = Arc::new(ControlPlaneService::new(
            Arc::new(MemoryUpstreamRepo::new(Arc::clone(&store))),
            Arc::new(MemoryRouteRepo::new(Arc::clone(&store))),
            Arc::new(MemoryPluginRepo::new(Arc::clone(&store))),
            Arc::clone(&registries) as Arc<dyn PluginCatalog>,
            tenants,
        ));
        let data_plane = Arc::new(DataPlaneService::new(
            Arc::clone(&control_plane),
            registries,
            Arc::new(UpstreamConnector::new(&config)),
            Arc::new(RateLimiterRegistry::new()),
            Arc::new(CircuitBreakerRegistry::new()),
            Arc::new(OagwMetrics::from_global()),
            config.clone(),
        ));

        if let Ok(registry) = ctx.client_hub().get::<dyn TypesRegistryClient>() {
            let _ = self.registry.set(registry);
        } else {
            warn!(
                target: "oagw.lifecycle",
                "types-registry client unavailable; the OAGW type catalog will not be provisioned"
            );
        }

        let wiring = Arc::new(Wiring {
            control_plane,
            data_plane,
            config,
        });
        self.wiring
            .set(Arc::clone(&wiring))
            .map_err(|_| anyhow::anyhow!("{} module already initialized", Self::MODULE_NAME))?;

        let cancel = ctx.cancellation_token().clone();
        Self::spawn_plugin_gc(wiring, async move { cancel.cancelled().await });

        info!(target: "oagw.lifecycle", "oagw module initialized");
        Ok(())
    }
}

#[async_trait]
impl SystemCapability for OagwGear {
    async fn post_init(&self, _sys: &toolkit::runtime::SystemContext) -> anyhow::Result<()> {
        if let Some(registry) = self.registry.get() {
            type_catalog::provision(registry.as_ref()).await;
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
        let wiring = self
            .wiring
            .get()
            .ok_or_else(|| anyhow::anyhow!("oagw services not initialized"))?;
        let router = register_routes(
            router,
            openapi,
            Arc::clone(&wiring.control_plane),
            Arc::clone(&wiring.data_plane),
        );
        info!(target: "oagw.lifecycle", "oagw REST routes registered");
        Ok(router)
    }
}
