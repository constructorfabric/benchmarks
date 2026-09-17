//! The `#[toolkit::gear]` declaration and the lifecycle entry point.
//!
//! OAGW declares the `system`, `rest` and `stateful` capabilities. There is
//! **no** `db` capability: the control plane is intentionally in-memory
//! (DESIGN §4), so nothing here touches the shared database.

use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use async_trait::async_trait;
use credstore_sdk::CredStoreClientV1;
use tenant_resolver_sdk::TenantResolverClient;
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::SystemCapability;
use toolkit::lifecycle::ReadySignal;
use toolkit::{Gear, GearCtx, RestApiCapability};
use tracing::info;

use crate::api::rest::extractors::ApiState;
use crate::config::OagwConfig;
use crate::domain::plugin::PluginRegistry;
use crate::domain::services::{PluginService, RouteService, UpstreamService};
use crate::infra::proxy::ProxyEngine;
use crate::infra::storage::InMemoryStore;

/// Interval between maintenance ticks of the data plane.
const MAINTENANCE_TICK_SECS: u64 = 60;
/// Rate-limit buckets idle longer than this are dropped.
const RATE_BUCKET_IDLE_SECS: u64 = 900;

/// Everything the control plane and the data plane share.
struct Runtime {
    config: Arc<OagwConfig>,
    store: Arc<InMemoryStore>,
    upstreams: Arc<UpstreamService>,
    routes: Arc<RouteService>,
    plugins: Arc<PluginService>,
    registry: Arc<PluginRegistry>,
    proxy: Arc<ProxyEngine>,
}

/// OAGW — the outbound API gateway gear.
#[toolkit::gear(
    name = "oagw",
    deps = [credstore, tenant_resolver, types_registry],
    capabilities = [system, rest, stateful],
    lifecycle(entry = "serve", stop_timeout = "30s", await_ready)
)]
pub struct OagwGear {
    runtime: OnceLock<Arc<Runtime>>,
}

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            runtime: OnceLock::new(),
        }
    }
}

impl OagwGear {
    /// Lifecycle entry point.
    ///
    /// OAGW has no background worker: the tick only performs maintenance
    /// (pruning idle rate-limit buckets and reporting in-memory occupancy) so
    /// the readiness signal is raised before the loop starts.
    pub(crate) async fn serve(
        self: Arc<Self>,
        cancel: CancellationToken,
        ready: ReadySignal,
    ) -> anyhow::Result<()> {
        let Some(runtime) = self.runtime.get().cloned() else {
            anyhow::bail!("oagw: serve invoked before init");
        };

        let tick = Duration::from_secs(MAINTENANCE_TICK_SECS);
        let mut interval = tokio::time::interval(tick);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        ready.notify();
        info!(
            target: "oagw.lifecycle",
            maintenance_tick_secs = tick.as_secs(),
            "oagw data plane ready"
        );

        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                _ = interval.tick() => {
                    let pruned = runtime.proxy.limiter().prune_idle(RATE_BUCKET_IDLE_SECS);
                    if pruned > 0 {
                        tracing::debug!(target: "oagw.lifecycle", pruned, "pruned idle rate-limit buckets");
                    }
                    tracing::debug!(
                        target: "oagw.lifecycle",
                        upstreams = runtime.store.upstream_count(),
                        "oagw maintenance tick"
                    );
                }
            }
        }

        info!(target: "oagw.lifecycle", "oagw lifecycle cancelled");
        Ok(())
    }
}

#[async_trait]
impl Gear for OagwGear {
    #[tracing::instrument(skip_all, fields(module = "oagw"))]
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let config: OagwConfig = ctx.config_or_default()?;
        info!(
            proxy_timeout_secs = config.proxy_timeout_secs,
            allow_http_upstream = config.allow_http_upstream,
            ssrf_policy_enabled = config.ssrf_policy.enabled,
            "initializing oagw module"
        );
        let config = Arc::new(config);

        // The control plane is in-memory; nothing is recovered from a database.
        let store = InMemoryStore::new();

        // credstore is a declared dependency but a missing client only degrades
        // secret resolution: inline credentials keep working, referenced ones
        // fail closed at request time with a 503.
        let credstore_client = ctx.client_hub().try_get::<dyn CredStoreClientV1>();
        match credstore_client {
            Some(_) => info!("credstore client resolved from client hub"),
            None => tracing::warn!(
                "credstore client unavailable; only inline upstream credentials are supported"
            ),
        }
        let registry = Arc::new(crate::infra::plugin::builtin_registry_with_credstore(
            credstore_client,
            usize::try_from(config.oauth2_token_cache_capacity).unwrap_or(4096),
        ));

        // The tenant chain drives hierarchical configuration: an ancestor's
        // shared upstream definitions, enforced rate limits and inherited
        // routes. The resolver is a declared dependency, but an unavailable or
        // failing one only costs inheritance — resolution degrades to the
        // caller's own tenant and a request is never failed for it.
        let chain: Arc<dyn crate::domain::TenantChain> =
            match ctx.client_hub().try_get::<dyn TenantResolverClient>() {
                Some(client) => {
                    info!(
                        ttl_secs = config.config_cache_ttl_secs,
                        "tenant-resolver client resolved from client hub"
                    );
                    Arc::new(crate::domain::hierarchy::ResolverChain::new(
                        client,
                        config.config_cache_ttl_secs,
                    ))
                }
                None => {
                    tracing::warn!(
                        "tenant-resolver client unavailable; hierarchical configuration is disabled"
                    );
                    Arc::new(crate::domain::hierarchy::SingleTenantChain)
                }
            };

        let upstreams = Arc::new(UpstreamService::new(
            Arc::clone(&store) as Arc<dyn crate::domain::UpstreamRepo>,
            Arc::clone(&config),
            Arc::clone(&registry),
            chain,
        ));
        let routes = Arc::new(RouteService::new(
            Arc::clone(&store) as Arc<dyn crate::domain::RouteRepo>,
            Arc::clone(&store) as Arc<dyn crate::domain::UpstreamRepo>,
            Arc::clone(&registry),
        ));
        let plugins = Arc::new(PluginService::new(
            Arc::clone(&store) as Arc<dyn crate::domain::PluginRepo>,
        ));
        let proxy = Arc::new(ProxyEngine::new(
            Arc::clone(&config),
            Arc::clone(&upstreams),
            Arc::clone(&routes),
            Arc::clone(&registry),
            Arc::clone(&store) as Arc<dyn crate::domain::PluginRepo>,
        ));

        self.runtime
            .set(Arc::new(Runtime {
                config: Arc::clone(&config),
                store: Arc::clone(&store),
                upstreams: Arc::clone(&upstreams),
                routes: Arc::clone(&routes),
                plugins: Arc::clone(&plugins),
                registry: Arc::clone(&registry),
                proxy: Arc::clone(&proxy),
            }))
            .map_err(|_| anyhow::anyhow!("{} module already initialized", Self::MODULE_NAME))?;

        // Publish the GTS type schemas. types-registry is a declared
        // dependency, but a transient registry outage must not stop the gear:
        // management CRUD and proxying do not depend on it.
        crate::infra::type_provisioning::register_types(ctx.client_hub()).await;

        info!("oagw module initialized");
        Ok(())
    }
}

// Empty system capability: OAGW needs no pre_init/post_init work, only the
// system-priority init ordering (see the module attribute above).
impl SystemCapability for OagwGear {}

impl RestApiCapability for OagwGear {
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: axum::Router,
        openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        info!("registering oagw REST routes");
        let runtime = self
            .runtime
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("oagw runtime not initialized"))?;
        let state = ApiState {
            upstreams: Arc::clone(&runtime.upstreams),
            routes: Arc::clone(&runtime.routes),
            plugins: Arc::clone(&runtime.plugins),
            plugin_registry: Arc::clone(&runtime.registry),
            proxy: Arc::clone(&runtime.proxy),
            config: Arc::clone(&runtime.config),
        };
        let router = crate::api::rest::register_routes(router, openapi, state);
        info!(base = crate::api::rest::BASE, "oagw REST routes registered");
        Ok(router)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gear_default_constructs_and_names_the_module() {
        let gear = OagwGear::default();
        assert_eq!(OagwGear::MODULE_NAME, "oagw");
        assert!(gear.runtime.get().is_none());
    }
}
