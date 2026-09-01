//! Gear declaration for the OAGW (outbound API gateway).
//!
//! The OAGW is a `system` + `rest` gear: it stands up an in-memory control
//! plane (upstreams / routes / custom plugins) during init, and registers the
//! `/oagw/v1/*` REST surface plus the `/oagw/v1/proxy/{alias}/{*suffix}` data
//! plane during the REST phase.
//!
//! Dependency contracts (freeze):
//! - [`TenantResolverClient`] — tenant chain resolution for alias shadowing
//!   and effective-config merging;
//! - [`CredStoreClientV1`] — secret resolution for `cred://` references in
//!   auth/header plugins (API key injection, `OAuth2` client credentials).
//!
//! Both clients are resolved **loudly**: a missing dependency client aborts
//! initialization with a descriptive error instead of booting a half-wired
//! gear.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use credstore_sdk::CredStoreClientV1;
use tenant_resolver_sdk::TenantResolverClient;
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::SystemCapability;
use toolkit::{Gear, GearCtx, RestApiCapability};
use tracing::info;

use crate::config::OagwConfig;
use crate::domain::services::data_plane::DataPlaneService;
use crate::domain::services::management::ControlPlaneService;
use crate::infra::plugin::registry::{
    AuthPluginRegistry, GuardPluginRegistry, TransformPluginRegistry,
};
use crate::infra::storage::MemoryRepos;

/// OAGW gear.
///
/// ## Capabilities
///
/// - `system` — stands up the in-memory control plane during init so the
///   client-side services are available before the REST phase.
/// - `rest` — exposes the management CRUD + data-plane proxy routes.
///
/// ## Dependencies
///
/// - `tenant_resolver` — tenant ancestor chain resolution.
/// - `credstore` — credential / secret store for `cred://` references.
#[toolkit::gear(
    name = "oagw",
    capabilities = [system, rest],
    deps = [tenant_resolver, credstore]
)]
pub struct OagwGear {
    control: OnceLock<Arc<ControlPlaneService>>,
    data_plane: OnceLock<Arc<DataPlaneService>>,
}

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            control: OnceLock::new(),
            data_plane: OnceLock::new(),
        }
    }
}

#[async_trait]
impl Gear for OagwGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let config: OagwConfig = ctx.config_or_default()?;
        if let Some(err) = config.validation_errors().into_iter().next() {
            anyhow::bail!("oagw: invalid configuration: {err}");
        }

        // Dependency clients — fail loudly when missing (freeze constraint:
        // "如果客户端缺失，初始化时必须大声失败").
        let tenant_resolver = ctx
            .client_hub()
            .get::<dyn TenantResolverClient>()
            .map_err(|e| {
                anyhow::anyhow!(
                    "oagw: required dependency client `TenantResolverClient` is missing from the \
                     ClientHub — the gateway cannot resolve tenant chains for alias shadowing and \
                     effective-config merging: {e}"
                )
            })?;
        let credstore = ctx
            .client_hub()
            .get::<dyn CredStoreClientV1>()
            .map_err(|e| {
                anyhow::anyhow!(
                    "oagw: required dependency client `CredStoreClientV1` is missing from the \
                     ClientHub — the gateway cannot resolve `cred://` secret references for \
                     auth/header plugins: {e}"
                )
            })?;

        let repos = MemoryRepos::new();
        let plugin_repo: Arc<dyn crate::domain::repo::PluginRepository> = repos.plugins.clone();

        // Plugin registries (built-in plugins ADR-0002/0008, plus any custom
        // per-tenant plugins persisted in the plugin repository).
        let auth_registry = Arc::new(AuthPluginRegistry::with_builtins(
            credstore,
            None,
            config.token_cache,
            Arc::clone(&plugin_repo),
        ));
        let guard_registry = Arc::new(GuardPluginRegistry::with_builtins(Arc::clone(&plugin_repo)));
        let transform_registry = Arc::new(TransformPluginRegistry::with_builtins(Arc::clone(
            &plugin_repo,
        )));

        let control = Arc::new(ControlPlaneService::new(
            repos,
            tenant_resolver,
            Arc::clone(&auth_registry),
            Arc::clone(&guard_registry),
            Arc::clone(&transform_registry),
            config.clone(),
        ));

        let data_plane = Arc::new(DataPlaneService::new(
            Arc::clone(&control),
            auth_registry,
            guard_registry,
            transform_registry,
            config,
        ));

        self.control
            .set(Arc::clone(&control))
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;
        self.data_plane
            .set(Arc::clone(&data_plane))
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        info!("oagw gear initialized (control plane + data plane ready)");
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
        let control = self
            .control
            .get()
            .ok_or_else(|| anyhow::anyhow!("oagw control plane not initialized"))?
            .clone();
        let data_plane = self
            .data_plane
            .get()
            .ok_or_else(|| anyhow::anyhow!("oagw data plane not initialized"))?
            .clone();

        let router =
            crate::api::rest::routes::register_routes(router, openapi, control, data_plane);
        info!("oagw REST routes registered");
        Ok(router)
    }
}

/// Required by the `system` capability; the gateway wires its services during
/// `init`, so no pre/post-init hooks are needed.
#[async_trait]
impl SystemCapability for OagwGear {}
