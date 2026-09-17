//! OAGW gear definition: wiring, lifecycle and REST registration.
//!
//! The gear exposes both halves of the gateway from one process: the control
//! plane (upstream/route/plugin CRUD) and the data plane (proxying), over one
//! REST surface registered under `/oagw/v1`.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::SystemCapability;
use toolkit::{Gear, GearCtx, RestApiCapability};
use tracing::info;

use crate::api::rest::handlers::OagwApi;
use crate::api::rest::routes;
use crate::config::OagwConfig;
use crate::infra::proxy::service::{DataPlane, DataPlaneDeps};
use crate::infra::storage::memory::MemoryStore;
use crate::infra::tenant::TenantHierarchy;

/// Main OAGW gear.
#[toolkit::gear(
    name = "oagw",
    deps = [tenant_resolver],
    capabilities = [system, rest]
)]
pub struct OagwGear {
    config: OnceLock<Arc<OagwConfig>>,
    api: OnceLock<Arc<OagwApi>>,
}

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            config: OnceLock::new(),
            api: OnceLock::new(),
        }
    }
}

#[async_trait]
impl Gear for OagwGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let config: OagwConfig = ctx
            .config_or_default()
            .map_err(|err| anyhow::anyhow!("failed to read oagw config: {err}"))?;
        info!(
            "OAGW config: allow_http_upstream={} proxy_timeout_secs={} max_body_bytes={}",
            config.allow_http_upstream, config.proxy_timeout_secs, config.max_body_bytes
        );
        self.config
            .set(Arc::new(config.clone()))
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        // Tenant hierarchy: the resolver client is the declared dependency.
        let resolver = match ctx.client_hub().get::<dyn tenant_resolver_sdk::TenantResolverClient>() {
            Ok(client) => Some(client),
            Err(_) => {
                info!("OAGW: no tenant resolver on the client hub; alias resolution is single-tenant");
                None
            }
        };
        let tenants = TenantHierarchy::new(resolver, std::time::Duration::from_secs(60));

        // Credential store is optional: only the API-key and OAuth2 plugins
        // need it, and they fail closed when it is absent.
        let credstore = ctx
            .client_hub()
            .get::<dyn credstore_sdk::CredStoreClientV1>()
            .ok();

        let store = MemoryStore::new();
        let data_plane = Arc::new(
            DataPlane::new(DataPlaneDeps {
                store: store.clone(),
                tenants,
                credstore,
                config: config.clone(),
            })?,
        );
        let management = Arc::new(crate::infra::management::ManagementService::new(
            store,
            Some(data_plane.clone()),
            config.allow_http_upstream,
        ));

        self.api
            .set(Arc::new(OagwApi {
                management,
                data_plane,
            }))
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        info!("OAGW gear initialized");
        Ok(())
    }
}

// `system` has no gear-specific pre/post init work: the data plane is built
// in `init` and holds no external resources of its own.
impl SystemCapability for OagwGear {}

impl RestApiCapability for OagwGear {
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: axum::Router,
        openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        let api = self
            .api
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("OAGW API not initialized"))?;
        info!("Registering OAGW REST routes");
        Ok(routes::register_routes(router, openapi, api))
    }
}

/// Readiness is implied by a completed `init`; no external dependency has to
/// be polled before the first proxy request.
impl OagwGear {
    /// Installed config, for diagnostics.
    #[must_use]
    pub fn config(&self) -> Option<Arc<OagwConfig>> {
        self.config.get().cloned()
    }

    /// Shared handler state, for tests and embedders.
    #[must_use]
    pub fn api(&self) -> Option<Arc<OagwApi>> {
        self.api.get().cloned()
    }
}
