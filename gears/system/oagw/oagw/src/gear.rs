//! Gear declaration for the OAGW (outbound API gateway) gear.
//!
//! Part 1 owns the **control plane**: in-memory configuration storage plus the
//! `/oagw/v1/{upstreams,routes,plugins}` management API. The gear is registered
//! with `capabilities = [system, rest]` and **no database capability** — the
//! control plane is an in-memory [`crate::infra::storage::MemoryStorage`], so
//! the gear works in deployments that have no `database:` block at all.
//!
//! Part 2 adds the **data plane** ([`crate::infra::proxy`]): the same
//! [`OagwConfig`] and the same [`ManagementService`] store feed the proxy
//! pipeline, which is assembled here once and published to `register_rest` so
//! the two proxy routes can extract it as an `axum::Extension`.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use credstore_sdk::CredStoreClientV1;
use tenant_resolver_sdk::TenantResolverClient;
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::SystemCapability;
use toolkit::{Gear, GearCtx, RestApiCapability};
use tracing::info;
use types_registry_sdk::TypesRegistryClient;

use crate::config::OagwConfig;
use crate::domain::services::ManagementService;
use crate::infra::plugin::registry::{AuthPluginRegistry, PluginRegistries};
use crate::infra::proxy::circuit::CircuitBreakers;
use crate::infra::proxy::guards::SsrfPolicy;
use crate::infra::proxy::metrics::DpMetrics;
use crate::infra::proxy::pipeline::DataPlaneService;
use crate::infra::proxy::rate_limit::RateLimiter;
use crate::infra::proxy::runtime::PluginRuntime;
use crate::infra::proxy::secrets::{CredStoreSecretSource, InMemorySecretSource, SecretSource};
use crate::infra::proxy::transport::ProxyTransport;
use crate::infra::storage::MemoryStorage;
use crate::infra::type_provisioning::TypeProvisioner;

/// The OAGW gear.
#[toolkit::gear(
    name = "oagw",
    deps = [credstore, types_registry],
    capabilities = [system, rest]
)]
pub struct OagwGear {
    /// The control plane, published in `init` and consumed by `register_rest`.
    service: OnceLock<Arc<ManagementService>>,
    /// The data plane, published in `init` and consumed by `register_rest`.
    data_plane: OnceLock<Arc<DataPlaneService>>,
}

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            service: OnceLock::new(),
            data_plane: OnceLock::new(),
        }
    }
}

impl OagwGear {
    /// The control plane, once initialised.
    #[must_use]
    pub fn service(&self) -> Option<Arc<ManagementService>> {
        self.service.get().cloned()
    }

    /// The data plane, once initialised.
    #[must_use]
    pub fn data_plane(&self) -> Option<Arc<DataPlaneService>> {
        self.data_plane.get().cloned()
    }
}

#[async_trait]
impl Gear for OagwGear {
    #[tracing::instrument(skip_all, fields(module = "oagw"))]
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: OagwConfig = ctx.config_or_default()?;
        cfg.validate()
            .map_err(|err| anyhow::anyhow!("oagw config invalid: {err}"))?;
        info!(
            proxy_timeout_secs = cfg.proxy_timeout_secs,
            allow_http_upstream = cfg.allow_http_upstream,
            body_limit_bytes = cfg.body_limit_bytes,
            "initializing oagw module"
        );

        // In-memory control plane store: the gear has no `database:` block in
        // any supported deployment, so configuration lives in process.
        let store = Arc::new(MemoryStorage::new());
        let service = Arc::new(ManagementService::new(store, cfg.clone()));

        // The shared upstream dialer. `allow_http_upstream` is a *dial-time*
        // policy only: `http` stays a legal endpoint scheme either way.
        let transport = match ProxyTransport::with_defaults(
            Duration::from_secs(cfg.proxy_timeout_secs),
            cfg.allow_http_upstream,
        ) {
            Ok(transport) => Arc::new(transport),
            Err(err) => {
                return Err(anyhow::anyhow!(
                    "oagw upstream transport unavailable: {err}"
                ));
            }
        };

        // The plugin runtime: credstore-backed when the credstore gear is
        // reachable, an empty source otherwise (credential-injection plugins
        // then fail closed rather than forwarding an unauthenticated request).
        let secrets: Arc<dyn SecretSource> = match ctx.client_hub().get::<dyn CredStoreClientV1>() {
            Ok(client) => Arc::new(CredStoreSecretSource::new(client)),
            Err(err) => {
                info!(
                    err = %err,
                    "credstore client unavailable; OAGW credential plugins will fail closed"
                );
                Arc::new(InMemorySecretSource::new())
            }
        };
        let runtime = Arc::new(PluginRuntime::new(
            secrets,
            cfg.token_cache.ttl(),
            cfg.token_cache.capacity(),
            Some(Arc::clone(&transport)),
        ));
        let registries = Arc::new(PluginRegistries {
            auth: AuthPluginRegistry::with_builtins_for(Arc::clone(&runtime)),
            ..PluginRegistries::with_builtins()
        });
        let rate_limiter = Arc::new(RateLimiter::new(
            usize::try_from(cfg.dp_cache_capacity)
                .unwrap_or(usize::MAX)
                .max(16),
        ));

        // Tenant hierarchy lookups are best-effort: an unavailable resolver
        // means the calling tenant only, never a failed request.
        let tenant_resolver = ctx.client_hub().get::<dyn TenantResolverClient>().ok();

        let data_plane = Arc::new(DataPlaneService::new(
            Arc::clone(&service),
            transport,
            registries,
            runtime,
            rate_limiter,
            Arc::new(CircuitBreakers::default()),
            DpMetrics::new(),
            SsrfPolicy::from_config(&cfg.ssrf_policy),
            tenant_resolver,
        ));

        // Publish the OAGW type schemas to the types-registry. Best-effort: a
        // registry hiccup must not keep the gateway from serving
        // configuration (the registry is a hard `deps`, so it is up here).
        match ctx.client_hub().get::<dyn TypesRegistryClient>() {
            Ok(registry) => {
                let provisioner = TypeProvisioner::new(registry);
                provisioner.provision().await;
            }
            Err(err) => {
                info!(
                    err = %err,
                    "types-registry client unavailable; OAGW type schemas not provisioned"
                );
            }
        }

        self.service
            .set(Arc::clone(&service))
            .map_err(|_| anyhow::anyhow!("{} module already initialized", OagwGear::MODULE_NAME))?;
        self.data_plane
            .set(data_plane)
            .map_err(|_| anyhow::anyhow!("{} module already initialized", OagwGear::MODULE_NAME))?;

        info!("oagw module initialized");
        Ok(())
    }
}

// Empty system capability: OAGW needs no pre_init/post_init work, only the
// system-priority init ordering.
impl SystemCapability for OagwGear {}

impl RestApiCapability for OagwGear {
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: axum::Router,
        openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        info!("registering oagw REST routes");
        let svc = self
            .service()
            .ok_or_else(|| anyhow::anyhow!("oagw ManagementService not initialized"))?;
        let data_plane = self
            .data_plane()
            .ok_or_else(|| anyhow::anyhow!("oagw DataPlaneService not initialized"))?;
        let router = crate::api::rest::register_routes(router, openapi, svc, data_plane);
        info!("oagw REST routes registered");
        Ok(router)
    }
}
