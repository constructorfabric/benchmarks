// Updated: 2026-09-01 by Constructor Tech
//! The gear: how OAGW registers itself with the host runtime.
//!
//! OAGW is a pure Control Plane + Data Plane pair with no database of its own
//! — its configuration is small, read-heavy and rewritten rarely, so it keeps
//! an in-memory store and re-reads it on every proxy exchange. It depends on
//! four sibling gears: the tenant resolver (hierarchy walk), the authz
//! resolver (who may write and who may proxy), the credstore (secret refs in
//! the auth plugins) and the types registry (the GTS base types it declares).

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use authz_resolver_sdk::{AuthZResolverClient, PolicyEnforcer};
use tenant_resolver_sdk::TenantResolverClient;
use toolkit::api::OpenApiRegistry;
use toolkit::{Gear, GearCtx, RestApiCapability};
use tracing::info;
use types_registry_sdk::TypesRegistryClient;

use crate::config::{OagwConfig, OagwConfigRaw};
use crate::domain::services::management::ManagementService;
use crate::infra::plugin::registry::PluginRegistry;
use crate::infra::proxy::service::ProxyService;
use crate::infra::storage::memory::{self, Stores};

/// The OAGW gear.
#[toolkit::gear(
    name = "oagw",
    deps = [credstore, types_registry, tenant_resolver, authz_resolver],
    capabilities = [rest],
    lifecycle(entry = "serve", stop_timeout = "30s", await_ready)
)]
pub struct OagwGear {
    management: OnceLock<Arc<ManagementService>>,
    proxy: OnceLock<Arc<ProxyService>>,
    config: OnceLock<Arc<OagwConfig>>,
}

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            management: OnceLock::new(),
            proxy: OnceLock::new(),
            config: OnceLock::new(),
        }
    }
}

impl OagwGear {
    /// The lifecycle entry point: nothing runs continuously, so this only
    /// reports readiness once the background plugin collector has started.
    #[allow(
        clippy::redundant_pub_crate,
        reason = "module-private serve entry-point invoked by the toolkit runtime"
    )]
    pub(crate) async fn serve(
        self: Arc<Self>,
        cancel: tokio_util::sync::CancellationToken,
        ready: toolkit::lifecycle::ReadySignal,
    ) -> anyhow::Result<()> {
        ready.notify();
        info!(target: "oagw.lifecycle", "oagw data plane ready");

        // Soft-deleted custom plugins are collected once an interval, so that
        // a plugin deleted while a proxy exchange holds a reference keeps
        // working until that exchange is done.
        if let Some(config) = self.config.get() {
            let management = self
                .management
                .get()
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("oagw: management service not initialized"))?;
            let ttl = config.plugin_gc_ttl;
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(ttl.max(Duration::from_secs(1)));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tokio::select! {
                        biased;
                        () = cancel.cancelled() => break,
                        _ = interval.tick() => {
                            let marked = management.refresh_gc_marks(ttl).await;
                            if marked > 0 {
                                let collected = management.collect_plugins().await;
                                tracing::debug!(marked, collected, "oagw plugin gc");
                            }
                        }
                    }
                }
            });
        }

        info!(target: "oagw.lifecycle", "oagw plugin collector cancelled");
        Ok(())
    }
}

#[async_trait]
impl Gear for OagwGear {
    #[tracing::instrument(skip_all, fields(module = "oagw"))]
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let raw: OagwConfigRaw = ctx.config_or_default()?;
        let config = Arc::new(OagwConfig::from_raw(&raw));
        info!(
            proxy_timeout_secs = config.proxy_timeout.as_secs(),
            connect_timeout_secs = config.connect_timeout.as_secs(),
            allow_http_upstream = config.allow_http_upstream,
            ssrf_enabled = config.ssrf_policy.enabled,
            "initializing oagw module"
        );

        // Sibling gears, resolved through the hub. types-registry is best
        // effort — a failed registration only skips the catalogue entry — but
        // the other three are what the resolution and authorization paths are
        // built on, and both services degrade to "see everything" without
        // them, which is only correct for the anonymous single-tenant case.
        let tenants = ctx
            .client_hub()
            .get::<dyn TenantResolverClient>()
            .map_err(|e| anyhow::anyhow!("oagw: failed to get TenantResolverClient: {e}"))?;
        let authz = ctx
            .client_hub()
            .get::<dyn AuthZResolverClient>()
            .map_err(|e| anyhow::anyhow!("oagw: failed to get AuthZResolverClient: {e}"))?;
        let enforcer = Arc::new(PolicyEnforcer::new(authz));
        let registry_client = ctx
            .client_hub()
            .get::<dyn TypesRegistryClient>()
            .map_err(|e| anyhow::anyhow!("oagw: failed to get TypesRegistryClient: {e}"))?;
        // credstore backs the plugins that resolve a `secret_ref`; without it
        // those plugins still register and report an infrastructure failure at
        // request time rather than silently disappearing.
        let credstore = ctx
            .client_hub()
            .get::<dyn credstore_sdk::CredStoreClientV1>()
            .ok();

        let registry = PluginRegistry::with_builtins(
            credstore,
            crate::config::TokenCacheConfig::from(config.as_ref()),
        );

        let stores = Stores::default();
        let repos = memory::repos(&stores);

        let management = Arc::new(ManagementService::new(
            repos.clone(),
            Some(Arc::clone(&tenants)),
            Some(enforcer),
            Arc::clone(&registry),
            (*config).clone(),
        ));
        let proxy = Arc::new(ProxyService::new(
            repos,
            Some(Arc::clone(&tenants)),
            Arc::clone(&registry),
            (*config).clone(),
        ));

        // The base types OAGW declares are announced, not asserted: the gear
        // still serves when the registry is behind.
        crate::infra::type_provisioning::provision(&*registry_client).await;

        self.config
            .set(Arc::clone(&config))
            .map_err(|_| anyhow::anyhow!("oagw: config already initialized"))?;
        self.proxy
            .set(Arc::clone(&proxy))
            .map_err(|_| anyhow::anyhow!("oagw: proxy service already initialized"))?;
        self.management
            .set(management)
            .map_err(|_| anyhow::anyhow!("oagw: management service already initialized"))?;

        info!("oagw module initialized");
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
        info!("registering oagw REST routes");
        let management = self
            .management
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("oagw management service not initialized"))?;
        let proxy = self
            .proxy
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("oagw proxy service not initialized"))?;
        let config = self
            .config
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("oagw config not initialized"))?;

        let router =
            crate::api::rest::routes::register_routes(router, openapi, management, proxy, config);
        info!("oagw REST routes registered");
        Ok(router)
    }
}
