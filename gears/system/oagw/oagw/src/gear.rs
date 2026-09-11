//! Gear declaration for the outbound API gateway.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use tenant_resolver_sdk::{GetAncestorsOptions, TenantResolverClient};
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::SystemCapability;
use toolkit::{Gear, GearCtx, RestApiCapability};
use toolkit_security::SecurityContext;
use tracing::info;
use uuid::Uuid;

use crate::api::{ApiState, NoAncestors};
use crate::config::OagwConfig;
use crate::domain::ControlPlane;
use crate::infra::credentials::{CredStoreResolver, SecretResolver};
use crate::domain::repo::{PluginRepository, RouteRepository, UpstreamRepository};
use crate::infra::plugin::{AuthPluginRegistry, GuardPluginRegistry, TransformPluginRegistry};
use crate::infra::proxy::service::{DataPlane, DataPlaneSettings};
use crate::infra::storage::memory::MemoryStores;

/// The outbound API gateway gear.
#[toolkit::gear(
    name = "oagw",
    deps = [credstore, types_registry, tenant_resolver],
    capabilities = [rest, system]
)]
pub struct OagwGear {
    state: OnceLock<ApiState>,
}

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            state: OnceLock::new(),
        }
    }
}

impl OagwGear {
    /// The API state the gear assembled at init, for tests and diagnostics.
    #[must_use]
    pub fn state(&self) -> Option<ApiState> {
        self.state.get().cloned()
    }
}

#[async_trait]
impl Gear for OagwGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let config: OagwConfig = ctx.config_or_default()?;
        config
            .validate()
            .map_err(|e| anyhow::anyhow!("invalid oagw config: {e}"))?;

        let stores = Arc::new(MemoryStores::new());
        let resolver = credential_resolver(ctx);
        let ancestors = ancestor_source(ctx);

        let settings = DataPlaneSettings {
            proxy_timeout_secs: config.proxy_timeout_secs,
            allow_http_upstream: config.allow_http_upstream,
            max_body_bytes: config.max_body_bytes,
        };
        let data = Arc::new(DataPlane::new(
            stores,
            AuthPluginRegistry::with_builtins(Arc::clone(&resolver)),
            GuardPluginRegistry::with_builtins(),
            TransformPluginRegistry::with_builtins(),
            settings,
            Some(resolver),
        ));
        let upstream_repo: Arc<dyn UpstreamRepository> =
            Arc::clone(&data.stores().upstreams) as _;
        let route_repo: Arc<dyn RouteRepository> = Arc::clone(&data.stores().routes) as _;
        let plugin_repo: Arc<dyn PluginRepository> = Arc::clone(&data.stores().plugins) as _;
        let control = Arc::new(ControlPlane::new(
            upstream_repo,
            route_repo,
            plugin_repo,
            config.allow_http_upstream,
        ));
        let state = ApiState::new(control, Arc::clone(&data), ancestors);
        self.state
            .set(state.clone())
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;
        info!(
            proxy_timeout_secs = config.proxy_timeout_secs,
            allow_http_upstream = config.allow_http_upstream,
            "OAGW gear initialized"
        );
        Ok(())
    }
}

impl SystemCapability for OagwGear {}

impl RestApiCapability for OagwGear {    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: axum::Router,
        _openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        let state = self
            .state
            .get()
            .ok_or_else(|| anyhow::anyhow!("OAGW gear is not initialized"))?
            .clone();
        Ok(router.merge(crate::api::router(state)))
    }
}

/// Resolver for `cred://` references, or one that always fails when the store is absent.
fn credential_resolver(ctx: &GearCtx) -> SecretResolver {
    match ctx.client_hub().get::<dyn credstore_sdk::CredStoreClientV1>() {
        Ok(store) => Arc::new(CredStoreResolver::new(store)),
        Err(_) => Arc::new(crate::infra::credentials::MissingResolver),
    }
}

/// Ancestor resolution over the tenant-resolver, or an empty chain when it is absent.
fn ancestor_source(ctx: &GearCtx) -> Arc<dyn crate::api::AncestorSource> {
    match ctx.client_hub().get::<dyn TenantResolverClient>() {
        Ok(client) => Arc::new(TenantAncestors::new(client)),
        Err(_) => Arc::new(NoAncestors),
    }
}

/// `Uuid` of the caller, wrapped for the tenant resolver.
fn tenant_id(tenant: &Uuid) -> tenant_resolver_sdk::TenantId {
    tenant_resolver_sdk::TenantId(*tenant)
}

/// [`crate::api::AncestorSource`] over the platform tenant resolver.
struct TenantAncestors {
    resolver: Arc<dyn TenantResolverClient>,
}

impl TenantAncestors {
    fn new(resolver: Arc<dyn TenantResolverClient>) -> Self {
        Self { resolver }
    }
}

#[async_trait::async_trait]
impl crate::api::AncestorSource for TenantAncestors {
    async fn ancestors(&self, tenant: Uuid) -> Vec<Uuid> {
        let context = SecurityContext::anonymous();
        let options = GetAncestorsOptions::default();
        match self.resolver.get_ancestors(&context, tenant_id(&tenant), &options).await {
            Ok(response) => response
                .ancestors
                .iter()
                .map(|t| t.id.0)
                .collect(),
            Err(error) => {
                tracing::warn!(tenant = %tenant, "tenant ancestor lookup failed: {error}");
                Vec::new()
            }
        }
    }
}
