//! ToolKit gear wiring for the Outbound API Gateway.
//!
//! Capabilities:
//!
//! * `system` — the control plane must be initialized before the data plane can
//!   resolve configuration,
//! * `rest` — exposes the management API (`/oagw/v1/upstreams`, ...) and, from
//!   the proxy slice onwards, the data-plane proxy endpoint.

use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use async_trait::async_trait;
use credstore_sdk::api::CredStoreClientV1;
use tenant_resolver_sdk::TenantResolverClient;
use toolkit::GearCtx;
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::SystemCapability;
use toolkit::runtime::SystemContext;
use toolkit::{Gear, RestApiCapability};
use tracing::{debug, info};

use crate::config::OagwConfig;
use crate::domain::metrics::OagwMetrics;
use crate::domain::policy::cors::CorsService;
use crate::domain::policy::rate_limit::{RateLimitLimiter, RateLimitService};
use crate::domain::routing::TenantHierarchy;
use crate::domain::services::control_plane::ControlPlaneService;
use crate::domain::services::data_plane::{DataPlaneService, ProxyHooks};
use crate::infra::plugin::{PluginEngineService, PluginRegistries, TokenCacheConfig};
use crate::infra::tenant_hierarchy::TenantResolverHierarchy;
use crate::infra::type_provisioning::TypeProvisioning;

/// The Outbound API Gateway gear.
#[toolkit::gear(
    name = "oagw",
    capabilities = [system, rest]
)]
pub struct OagwGear {
    service: OnceLock<Arc<ControlPlaneService>>,
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
    /// The initialized control-plane service, if [`Gear::init`] has run.
    #[must_use]
    pub fn service(&self) -> Option<Arc<ControlPlaneService>> {
        self.service.get().cloned()
    }

    /// The initialized data-plane service, if [`Gear::init`] has run.
    #[must_use]
    pub fn data_plane(&self) -> Option<Arc<DataPlaneService>> {
        self.data_plane.get().cloned()
    }

    /// The tenant-hierarchy source, when the host published a resolver client.
    ///
    /// Without one the proxy resolves configuration for the calling tenant
    /// only: inherited upstreams are then unreachable, which narrows — never
    /// widens — what a request can touch (see
    /// [`TenantHierarchy::chain`](crate::domain::routing::TenantHierarchy::chain)).
    fn tenant_hierarchy(ctx: &GearCtx) -> Option<Arc<dyn TenantHierarchy>> {
        let client = ctx
            .client_hub()
            .try_get::<dyn TenantResolverClient>()
            .inspect(|_| debug!("oagw data plane uses the host tenant resolver"))
            .or_else(|| {
                debug!("no tenant resolver published; the proxy degrades to the calling tenant");
                None
            })?;

        Some(Arc::new(TenantResolverHierarchy::new(client)))
    }

    /// The credential store, when the host published a client for it.
    ///
    /// The plugins that inject a secret (`cred://` reference) need it; without
    /// one those references stay unresolved, and the request that needed one
    /// fails closed with `cf.oagw.secret.not_found.v1` instead of being
    /// forwarded unauthenticated (DESIGN §2.1 "Credential Isolation").
    fn credstore(ctx: &GearCtx) -> Option<Arc<dyn CredStoreClientV1>> {
        ctx.client_hub()
            .try_get::<dyn CredStoreClientV1>()
            .inspect(|_| debug!("oagw plugins resolve credentials through the host credstore"))
            .or_else(|| {
                debug!("no credential store published; cred:// references fail closed");
                None
            })
    }

    /// The data-plane hooks of this slice: the rate limiter, the CORS handler
    /// and the plugin engine (ADR-0002, ADR-0003, ADR-0004, ADR-0008).
    ///
    /// The three are built here, where the gear holds the control-plane stores,
    /// and handed to [`DataPlaneService::with_hooks`] as one bundle: the data
    /// plane sequences them, they decide.
    fn proxy_hooks(ctx: &GearCtx, service: &ControlPlaneService) -> ProxyHooks {
        // The bucket map is bounded by configuration: a `scope: ip` counter is
        // keyed on an address the caller picks, so an unbounded map would be an
        // unbounded allocation driven by a request header (ADR-0003).
        let limiter = Arc::new(RateLimitLimiter::with_capacity(
            service.config().rate_limit_bucket_capacity,
        ));

        // The OAuth2 token cache is bounded by configuration too: it holds one
        // bearer token per (tenant, subject, auth method, config), so an
        // unbounded map would grow with the tenant population it serves
        // (ADR-0008 "Gear-Level Configuration").
        let token_cache = TokenCacheConfig {
            ttl: Duration::from_secs(service.config().token_cache_ttl_secs),
            capacity: usize::try_from(service.config().token_cache_capacity).unwrap_or(usize::MAX),
        };

        let registries =
            PluginRegistries::with_builtins_and_config(Self::credstore(ctx), token_cache);
        let engine = PluginEngineService::new(registries, Arc::clone(service.plugin_store()));

        ProxyHooks::new(
            Some(Arc::new(RateLimitService::new(limiter))),
            Some(Arc::new(CorsService)),
            Some(Arc::new(engine)),
        )
    }
}

/// OAGW declares the `system` capability: the control plane must be
/// initialized before the data plane can resolve configuration.
///
/// `post_init` publishes the GTS type declaration once every gear is
/// initialized: provisioning is a declaration, so it is announced to the log
/// instead of pushed to the types registry over the network (DESIGN §4.7).
#[async_trait]
impl SystemCapability for OagwGear {
    /// Publish the served GTS type ids.
    ///
    /// Infallible and cheap: it formats one log record and touches no network,
    /// so a failure here can never hold up the runtime's `post_init` phase.
    ///
    /// # Errors
    /// Never returns an error.
    async fn post_init(&self, _sys: &SystemContext) -> anyhow::Result<()> {
        let declaration = TypeProvisioning::oagw();

        info!(
            target: "oagw.provisioning",
            served_type_ids = declaration.served_type_ids().join(", "),
            "oagw serves the declared GTS types"
        );

        Ok(())
    }
}

#[async_trait]
impl Gear for OagwGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let config: OagwConfig = ctx.config_or_default()?;
        config
            .validate()
            .map_err(|err| anyhow::anyhow!("oagw config invalid: {err}"))?;

        debug!(
            proxy_timeout_secs = config.proxy_timeout_secs,
            allow_http_upstream = config.allow_http_upstream,
            ssrf_policy_enabled = config.ssrf_policy.is_enabled(),
            token_cache_ttl_secs = config.token_cache_ttl_secs,
            token_cache_capacity = config.token_cache_capacity,
            rate_limit_bucket_capacity = config.rate_limit_bucket_capacity,
            "Loaded oagw config"
        );

        let service = Arc::new(ControlPlaneService::new(config));
        self.service
            .set(Arc::clone(&service))
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        let hierarchy = Self::tenant_hierarchy(ctx);
        let mut data_plane = DataPlaneService::new(
            config,
            Arc::clone(&service),
            Arc::clone(service.upstream_store()),
            Arc::clone(service.route_store()),
        );
        if let Some(hierarchy) = hierarchy {
            data_plane = data_plane.with_tenant_hierarchy(hierarchy);
        }
        data_plane = data_plane.with_hooks(Self::proxy_hooks(ctx, &service));
        // The gear names the instrumentation scope, exactly as api-gateway names
        // its middleware's: the host owns the provider, this gear only says
        // whose instruments these are (DESIGN §4.2).
        data_plane = data_plane.with_metrics(OagwMetrics::from_global(Self::MODULE_NAME));

        self.data_plane
            .set(Arc::new(data_plane))
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

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
        info!("Registering oagw REST routes");

        let service = self
            .service
            .get()
            .ok_or_else(|| anyhow::anyhow!("oagw service not initialized"))?
            .clone();
        let data_plane = self
            .data_plane
            .get()
            .ok_or_else(|| anyhow::anyhow!("oagw data plane not initialized"))?
            .clone();

        let router = crate::api::rest::routes::register_routes(router, openapi, service);
        let router =
            crate::api::rest::proxy_routes::register_proxy_routes(router, openapi, data_plane);

        info!("oagw REST routes registered successfully");
        Ok(router)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use toolkit::runtime::{GearManager, GrpcInstallerStore};
    use uuid::Uuid;

    #[tokio::test]
    async fn post_init_publishes_the_served_type_ids() {
        let gear = OagwGear::default();
        let sys = SystemContext::new(
            Uuid::new_v4(),
            Arc::new(GearManager::new()),
            Arc::new(GrpcInstallerStore::new()),
        );

        gear.post_init(&sys)
            .await
            .expect("publishing the declaration is infallible");
    }

    #[tokio::test]
    async fn post_init_is_repeatable() {
        let gear = OagwGear::default();
        let sys = SystemContext::new(
            Uuid::new_v4(),
            Arc::new(GearManager::new()),
            Arc::new(GrpcInstallerStore::new()),
        );

        gear.post_init(&sys).await.expect("first run");
        gear.post_init(&sys).await.expect("second run");
    }
}
