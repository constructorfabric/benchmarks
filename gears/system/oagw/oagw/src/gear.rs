//! The gear itself: registration, wiring, and route registration.
//!
//! `init` reads the gear configuration, resolves the credential-store and
//! tenant-resolver clients from the client hub, and builds the
//! [`crate::domain::service::Service`] the control and data planes share.
//! `register_rest` is called after `init` per the toolkit lifecycle contract,
//! so the [`OnceLock`] read below is infallible in practice; the `ok_or_else`
//! turns a misordered runtime into a precise bootstrap failure instead of a
//! panic.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use tenant_resolver_sdk::TenantResolverClient;
use toolkit::api::OpenApiRegistry;
use toolkit::{Gear, GearCtx, RestApiCapability};
use tracing::info;

use crate::config::OagwConfig;
use crate::credstore_client::SharedCredentialStore;
use crate::domain::plugin::ControlPlane;
use crate::domain::ratelimit::RateLimiter;
use crate::domain::service::Service;
use crate::domain::store::Store;
use crate::infra::outbound::Outbound;
use crate::infra::token_cache::TokenCache;

/// The outbound API gateway gear.
#[toolkit::gear(
    name = "oagw",
    deps = [credstore, tenant_resolver],
    capabilities = [rest]
)]
pub struct OagwGear {
    service: OnceLock<Arc<Service>>,
    outbound: OnceLock<Arc<Outbound>>,
}

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            service: OnceLock::new(),
            outbound: OnceLock::new(),
        }
    }
}

#[async_trait]
impl Gear for OagwGear {
    #[tracing::instrument(skip_all, fields(module = "oagw"))]
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let config: OagwConfig = ctx.config_or_default()?;
        info!(
            allow_http_upstream = config.allow_http_upstream,
            proxy_timeout_secs = config.proxy_timeout_secs,
            max_body_size_bytes = config.max_body_size_bytes,
            "initializing oagw module"
        );

        // Fail-closed: without the credential store no `secret_ref` resolves,
        // so the data plane has nothing to inject. `credstore` is a hard
        // `deps` and initializes first.
        let credential_store: SharedCredentialStore = ctx
            .client_hub()
            .get::<dyn credstore_sdk::CredStoreClientV1>()
            .map_err(|err| anyhow::anyhow!("failed to get CredStoreClientV1: {err}"))?;
        info!("credstore client resolved from client hub; credential injection wired");

        // The hierarchy is optional: without a resolver every caller is its
        // own root, which is the single-tenant shape.
        let tenants: Option<Arc<dyn TenantResolverClient>> = match ctx
            .client_hub()
            .get::<dyn TenantResolverClient>(
        ) {
            Ok(client) => {
                info!("tenant-resolver client resolved from client hub; hierarchy merge enabled");
                Some(client)
            }
            Err(err) => {
                info!(error = %err, "no tenant-resolver client; treating every caller as its own root");
                None
            }
        };

        let plugins = Arc::new(ControlPlane::with_builtins(
            Arc::clone(&credential_store),
            Arc::new(TokenCache::new(config.token_cache_capacity)),
        ));
        let outbound = Arc::new(Outbound::new(
            config.connection_pool_size,
            config.connect_timeout(),
            config.proxy_timeout(),
        ));

        let service = Service::new(
            Store::new(),
            plugins,
            credential_store,
            tenants,
            config,
            Arc::new(RateLimiter::new()),
        );

        self.service
            .set(Arc::clone(&service))
            .map_err(|_| anyhow::anyhow!("oagw module already initialized"))?;
        self.outbound
            .set(outbound)
            .map_err(|_| anyhow::anyhow!("oagw module already initialized"))?;

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
        let service = self
            .service
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("oagw Service not initialized"))?;
        let outbound = self
            .outbound
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("oagw Outbound transport not initialized"))?;
        let router = crate::api::routes::register_routes(
            router,
            openapi,
            crate::api::ApiState { service, outbound },
        );
        info!("oagw REST routes registered at /oagw/v1");
        Ok(router)
    }
}
