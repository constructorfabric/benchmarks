//! Gear declaration for the Outbound API Gateway.
//!
//! The gear wires the Control Plane (configuration CRUD) and the Data Plane
//! (proxy execution) together, publishes both service handles in the
//! [`toolkit::ClientHub`] and mounts the REST sub-router under the two
//! documented prefixes (`/oagw/v1/...` and `/api/oagw/v1/...`).

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use toolkit::api::OpenApiRegistry;
use toolkit::{Gear, GearCtx, RestApiCapability};
use tracing::{debug, info};

use crate::config::OagwConfig;
use crate::domain::services::management::ControlPlaneService;
use crate::infra::controlplane::ControlPlaneServiceImpl;
use crate::infra::metrics::OagwMetrics;
use crate::infra::plugin::BuiltinPlugins;
use crate::infra::proxy::service::{DataPlaneServiceImpl, ProxyOptions};

/// Outbound API Gateway gear.
///
/// ## Capabilities
///
/// - `rest` — Control Plane CRUD plus the Data Plane proxy endpoints
///
/// ## Services
///
/// * [`ControlPlaneServiceImpl`] — in-memory upstream / route / plugin store
///   with tenant-hierarchy resolution and alias shadowing.
/// * [`DataPlaneServiceImpl`] — plugin chain execution, rate limiting, CORS,
///   circuit breaking and the streaming outbound relay.
#[toolkit::gear(
    name = "oagw",
    deps = [types_registry, tenant_resolver, authz_resolver, credstore],
    capabilities = [rest]
)]
pub struct OagwGear {
    /// Control Plane service handle, set during `init`.
    control_plane: OnceLock<Arc<ControlPlaneServiceImpl>>,
    /// Data Plane service handle, set during `init`.
    data_plane: OnceLock<Arc<DataPlaneServiceImpl>>,
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
        let cfg: OagwConfig = ctx.config_or_default()?;
        debug!(
            proxy_timeout_secs = cfg.proxy_timeout_secs,
            allow_http_upstream = cfg.allow_http_upstream,
            ssrf_enabled = cfg.ssrf_policy.enabled,
            trusted_proxies = cfg.trusted_proxies().len(),
            "Loaded oagw config"
        );

        let credstore = ctx
            .client_hub()
            .try_get::<dyn credstore_sdk::CredStoreClientV1>();
        let plugins = BuiltinPlugins::with_token_cache(
            credstore,
            cfg.token_cache_ttl_secs(),
            cfg.token_cache_capacity(),
        );

        let tenants = ctx
            .client_hub()
            .try_get::<dyn tenant_resolver_sdk::TenantResolverClient>();
        let authz = ctx
            .client_hub()
            .try_get::<dyn authz_resolver_sdk::AuthZResolverClient>();
        let store = crate::infra::storage::InMemoryStore::new();
        let control_plane = Arc::new(
            ControlPlaneServiceImpl::new(store, tenants)
                .with_authz(authz)
                .allowing_http_upstream(cfg.allow_http_upstream),
        );

        let options = ProxyOptions {
            proxy_timeout_secs: cfg.proxy_timeout_secs,
            allow_http_upstream: cfg.allow_http_upstream,
            ssrf_enabled: cfg.ssrf_policy.enabled,
        };
        let data_plane = Arc::new(
            DataPlaneServiceImpl::new(control_plane.clone(), plugins, OagwMetrics::new(), options)
                .with_trusted_proxies(cfg.trusted_proxies()),
        );

        ctx.client_hub()
            .register::<dyn ControlPlaneService>(control_plane.clone());

        if self.control_plane.set(control_plane.clone()).is_err() {
            debug!("oagw control plane already initialised");
        }
        if self.data_plane.set(data_plane.clone()).is_err() {
            debug!("oagw data plane already initialised");
        }

        info!("oagw gear initialised");
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
        info!("Registering oagw REST routes");

        let control_plane = self
            .control_plane
            .get()
            .ok_or_else(|| anyhow::anyhow!("oagw control plane not initialized"))?
            .clone();
        let data_plane = self
            .data_plane
            .get()
            .ok_or_else(|| anyhow::anyhow!("oagw data plane not initialized"))?
            .clone();

        let sub = crate::api::rest::routes::build_router(control_plane, data_plane, openapi);
        let router = router.merge(sub.clone()).nest("/api", sub);

        info!("oagw REST routes registered successfully");
        Ok(router)
    }
}
