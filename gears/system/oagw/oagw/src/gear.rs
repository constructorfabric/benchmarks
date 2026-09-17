//! OAGW gear wiring.
//!
//! The gear is **stateless**: it declares only the `rest` capability (no
//! `lifecycle`) because the crate's frozen dependency set does not include
//! `tokio-util` (required by the toolkit lifecycle runtime). It follows the
//! same shape as `types-registry` (rest-only). All state — repositories,
//! plugin registries, rate-limit buckets — lives in the control/data plane
//! services published into `OnceLock`s during `Gear::init`.
//!
//! Hard dependencies (declared in `deps`):
//! - **types-registry**: the OAGW GTS catalog is provisioned best-effort at
//!   init (`infra::type_provisioning`).
//! - **credstore**: `cred://` secret references in auth plugins.
//! - **tenant-resolver**: tenant ancestry for alias shadowing + hierarchy.
//! - **authz-resolver**: policy enforcement for CRUD (`create`/`override`/
//!   `read`/`delete`) and proxy `:invoke`.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use authz_resolver_sdk::models::Capability;
use authz_resolver_sdk::{AuthZResolverClient, PolicyEnforcer};
use credstore_sdk::CredStoreClientV1;
use tenant_resolver_sdk::TenantResolverClient;
use toolkit::api::OpenApiRegistry;
use toolkit::{Gear, GearCtx, RestApiCapability};
use tracing::info;
use types_registry_sdk::TypesRegistryClient;

use crate::api::rest::routes;
use crate::config::OagwConfig;
use crate::domain::services::management::ControlPlaneServiceImpl;
use crate::domain::services::hierarchy::TenantHierarchy;
use crate::infra::plugin::registry::{
    AuthPluginRegistry, GuardPluginRegistry, TransformPluginRegistry,
};
use crate::infra::plugin::CredStoreSecretResolver;
use crate::infra::proxy::client::OagwHttpClient;
use crate::infra::proxy::ratelimit::RateLimitManager;
use crate::infra::proxy::service::DataPlaneServiceImpl;
use crate::infra::storage::memory::{MemoryPluginRepository, MemoryRouteRepository, MemoryUpstreamRepository};
use crate::infra::storage::tenant_hierarchy::SdkTenantHierarchy;
use crate::infra::type_provisioning::register_oagw_types;

/// The OAGW gear.
#[toolkit::gear(
    name = "oagw",
    capabilities = [rest],
    deps = [types_registry, credstore, tenant_resolver, authz_resolver]
)]
pub struct OagwGear {
    control: OnceLock<Arc<ControlPlaneServiceImpl>>,
    data: OnceLock<Arc<DataPlaneServiceImpl>>,
}

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            control: OnceLock::new(),
            data: OnceLock::new(),
        }
    }
}

#[async_trait]
impl Gear for OagwGear {
    #[tracing::instrument(skip_all, fields(gear = "oagw"))]
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: OagwConfig = ctx.config_or_default()?;
        cfg.validate()
            .map_err(|err| anyhow::anyhow!("oagw config invalid: {err}"))?;
        info!(
            proxy_timeout_secs = cfg.proxy_timeout_secs,
            allow_http_upstream = cfg.allow_http_upstream,
            ssrf_enabled = cfg.ssrf_policy.enabled,
            "initializing oagw gear"
        );

        // --- Resolve hard dependencies from the client hub -----------------
        let types_registry: Arc<dyn TypesRegistryClient> = ctx
            .client_hub()
            .get::<dyn TypesRegistryClient>()
            .map_err(|e| anyhow::anyhow!("failed to get TypesRegistryClient: {e}"))?;
        let credstore: Arc<dyn CredStoreClientV1> = ctx
            .client_hub()
            .get::<dyn CredStoreClientV1>()
            .map_err(|e| anyhow::anyhow!("failed to get CredStoreClientV1: {e}"))?;
        let tenant_resolver: Arc<dyn TenantResolverClient> = ctx
            .client_hub()
            .get::<dyn TenantResolverClient>()
            .map_err(|e| anyhow::anyhow!("failed to get TenantResolverClient: {e}"))?;
        let authz: Arc<dyn AuthZResolverClient> = ctx
            .client_hub()
            .get::<dyn AuthZResolverClient>()
            .map_err(|e| anyhow::anyhow!("failed to get AuthZResolverClient: {e}"))?;
        info!("oagw dependency clients resolved from client hub");

        // PEP boundary (DESIGN §4.2) — fail-closed on missing authz.
        let enforcer = PolicyEnforcer::new(authz).with_capabilities(vec![Capability::TenantHierarchy]);

        // --- In-memory control-plane repositories --------------------------
        let upstreams: Arc<dyn crate::domain::repo::UpstreamRepository> =
            Arc::new(MemoryUpstreamRepository::default());
        let routes: Arc<dyn crate::domain::repo::RouteRepository> =
            Arc::new(MemoryRouteRepository::default());
        let plugins: Arc<dyn crate::domain::repo::PluginRepository> =
            Arc::new(MemoryPluginRepository::default());

        // Tenant hierarchy over the resolver gear (barrier-respecting).
        let hierarchy: Arc<dyn TenantHierarchy> =
            Arc::new(SdkTenantHierarchy::new(tenant_resolver));

        // --- Services -------------------------------------------------------
        let http_client = Arc::new(OagwHttpClient::new());
        let auth_registry =
            AuthPluginRegistry::with_builtins(credstore.clone(), http_client.clone(), cfg.token_cache());
        let guard_registry = GuardPluginRegistry::with_builtins();
        let transform_registry = TransformPluginRegistry::with_builtins();
        let secrets: Arc<dyn crate::domain::plugin::SecretResolver> =
            Arc::new(CredStoreSecretResolver::new(credstore));
        let rate_limiter = Arc::new(RateLimitManager::new());

        let control = Arc::new(ControlPlaneServiceImpl::new(
            upstreams.clone(),
            routes.clone(),
            plugins.clone(),
            hierarchy.clone(),
            enforcer.clone(),
        ));
        let data = Arc::new(DataPlaneServiceImpl::new(
            upstreams,
            routes,
            plugins,
            hierarchy,
            enforcer,
            auth_registry,
            guard_registry,
            transform_registry,
            secrets,
            rate_limiter,
            http_client,
            cfg.clone(),
        ));

        self.control
            .set(control.clone())
            .map_err(|_| anyhow::anyhow!("oagw gear already initialized"))?;
        self.data
            .set(data.clone())
            .map_err(|_| anyhow::anyhow!("oagw gear already initialized"))?;

        // --- GTS type provisioning (best-effort, non-fatal) -----------------
        register_oagw_types(&*types_registry).await;

        info!("oagw gear initialized");
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

        let control = self
            .control
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("oagw: control service not initialized"))?;
        let data = self
            .data
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("oagw: data service not initialized"))?;

        let router = routes::register_routes(router, openapi, control, data);

        info!("oagw REST routes registered");
        Ok(router)
    }
}
