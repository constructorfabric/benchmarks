//! ToolKit gear wiring.
//!
//! No `db` capability: the crate has no `toolkit-db` dependency and its
//! repositories are in-process (see [`crate::infra::storage`]). No
//! `stateful` capability either — there is no background runner, so plugin
//! garbage collection is driven from the write paths that can unlink a
//! plugin.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use authz_resolver_sdk::{AuthZResolverClient, PolicyEnforcer};
use tenant_resolver_sdk::TenantResolverClient;
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::SystemCapability;
use toolkit::{Gear, GearCtx, RestApiCapability};
use tracing::{info, warn};
use types_registry_sdk::TypesRegistryClient;

use crate::api::rest::OagwState;
use crate::config::OagwConfig;
use crate::domain::services::management::{ControlPlaneService, ControlPlaneServiceImpl};
use crate::domain::services::proxy::DataPlaneService;
use crate::domain::services::tenancy::{FlatHierarchy, TenantHierarchy};
use crate::infra::metrics::OagwMetrics;
use crate::infra::plugin::{PluginRegistries, TokenCacheConfig};
use crate::infra::proxy::DataPlaneServiceImpl;
use crate::infra::storage::{InMemoryPluginRepo, InMemoryRouteRepo, InMemoryUpstreamRepo};
use crate::infra::tenant_resolver::TenantResolverHierarchy;
use crate::infra::type_provisioning;

/// The Outbound API Gateway gear.
#[toolkit::gear(
    name = "oagw",
    deps = [authz_resolver, tenant_resolver, types_registry, credstore],
    capabilities = [system, rest]
)]
pub struct OagwGear {
    state: OnceLock<Arc<OagwState>>,
    registry: OnceLock<Arc<dyn TypesRegistryClient>>,
    provision_types: OnceLock<bool>,
}

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            state: OnceLock::new(),
            registry: OnceLock::new(),
            provision_types: OnceLock::new(),
        }
    }
}

#[async_trait]
impl Gear for OagwGear {
    #[tracing::instrument(skip_all, fields(module = "oagw"))]
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: OagwConfig = ctx.config_or_default()?;
        cfg.validate()
            .map_err(|err| anyhow::anyhow!("oagw config invalid: {err}"))?;

        if cfg.allow_http_upstream {
            warn!(
                target: "oagw.config",
                "allow_http_upstream is enabled: plaintext upstream connections are permitted. \
                 The default posture is HTTPS-only (cpt-cf-oagw-constraint-https-only)."
            );
        }
        if !cfg.ssrf_policy.enabled {
            warn!(
                target: "oagw.config",
                "SSRF address filtering is disabled: resolved upstream addresses are not checked"
            );
        }

        // Credential store — auth plugins resolve `cred://` references per
        // request; OAGW never holds secret material itself.
        let credstore = ctx
            .client_hub()
            .get::<dyn credstore_sdk::CredStoreClientV1>()
            .map_err(|e| anyhow::anyhow!("failed to get CredStoreClientV1: {e}"))?;

        // Tenant hierarchy — alias shadowing and configuration inheritance.
        let hierarchy: Arc<dyn TenantHierarchy> =
            match ctx.client_hub().get::<dyn TenantResolverClient>() {
                Ok(client) => Arc::new(TenantResolverHierarchy::new(client)),
                Err(err) => {
                    warn!(
                        target: "oagw.tenancy",
                        error = %err,
                        "tenant-resolver client unavailable; every tenant resolves as its own root"
                    );
                    Arc::new(FlatHierarchy)
                }
            };

        let control_plane: Arc<dyn ControlPlaneService> = Arc::new(ControlPlaneServiceImpl::new(
            Arc::new(InMemoryUpstreamRepo::new()),
            Arc::new(InMemoryRouteRepo::new()),
            Arc::new(InMemoryPluginRepo::new()),
            hierarchy,
            cfg.plugin_gc_ttl_secs,
            cfg.allow_http_upstream,
        ));

        let registries = Arc::new(PluginRegistries::with_builtins(
            credstore,
            TokenCacheConfig {
                ttl: cfg.token_cache_ttl(),
                capacity: cfg.token_cache_capacity,
            },
        ));
        let metrics = Arc::new(OagwMetrics::from_global());

        let data_plane: Arc<dyn DataPlaneService> = Arc::new(DataPlaneServiceImpl::new(
            Arc::clone(&control_plane),
            registries,
            metrics,
            cfg.clone(),
        ));

        // Inbound authorization. Fail-closed: without a PDP the management
        // and proxy surfaces would be open to any authenticated caller.
        let authz_client = ctx
            .client_hub()
            .get::<dyn AuthZResolverClient>()
            .map_err(|e| anyhow::anyhow!("failed to get AuthZResolverClient: {e}"))?;
        let enforcer = Arc::new(PolicyEnforcer::new(authz_client));

        let state = Arc::new(OagwState {
            control_plane,
            data_plane,
            enforcer: Some(enforcer),
        });
        self.state
            .set(state)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        if cfg.provision_types {
            match ctx.client_hub().get::<dyn TypesRegistryClient>() {
                Ok(registry) => {
                    let _ = self.registry.set(registry);
                }
                Err(err) => warn!(
                    target: "oagw.types",
                    error = %err,
                    "types-registry client unavailable; OAGW type catalog will not be published"
                ),
            }
        }
        let _ = self.provision_types.set(cfg.provision_types);

        info!(
            proxy_timeout_secs = cfg.proxy_timeout_secs,
            allow_http_upstream = cfg.allow_http_upstream,
            ssrf_enabled = cfg.ssrf_policy.enabled,
            "oagw gear initialized"
        );
        Ok(())
    }
}

#[async_trait]
impl SystemCapability for OagwGear {
    async fn post_init(&self, _sys: &toolkit::runtime::SystemContext) -> anyhow::Result<()> {
        if self.provision_types.get().copied() != Some(true) {
            return Ok(());
        }
        if let Some(registry) = self.registry.get() {
            type_provisioning::provision(registry).await;
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
