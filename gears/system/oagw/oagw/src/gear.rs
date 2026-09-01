//! Gear declaration for the OAGW (Outbound API Gateway).

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::SystemCapability;
use toolkit::{Gear, GearCtx, RestApiCapability};
use tracing::{debug, info};

use credstore_sdk::CredStoreClientV1;
use tenant_resolver_sdk::TenantResolverClient;

use crate::config::OagwConfig;
use crate::domain::data_plane::DataPlaneService;
use crate::domain::hierarchy::{FlatTenantHierarchy, ResolverHierarchy, TenantHierarchy};
use crate::domain::service::ControlPlaneService;
use crate::infra::plugin::build_registries;
use crate::infra::proxy::HyperProxyEngine;
use crate::infra::storage::InMemoryRepository;
use crate::infra::type_provisioning::TypeProvisioner;

use types_registry_sdk::TypesRegistryClient;

/// OAGW gear.
///
/// ## Capabilities
///
/// - `system` — loads configuration and wires the in-memory control plane
///   during startup
/// - `rest` — serves the management API (`/api/oagw/v1/upstreams`,
///   `/routes`, `/plugins`) and the proxy API (`/api/oagw/v1/proxy/...`)
#[toolkit::gear(
    name = "oagw",
    capabilities = [system, rest]
)]
pub struct OagwGear {
    control_plane: OnceLock<Arc<ControlPlaneService<InMemoryRepository>>>,
    data_plane: OnceLock<Arc<DataPlaneService>>,
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
            "Loaded OAGW config"
        );

        // In-process per-tenant store (ADR-0006: MVP keeps configuration in
        // memory; the repository trait keeps a DB-backed slot-in possible).
        let repo: Arc<InMemoryRepository> = Arc::new(InMemoryRepository::default());

        // Tenant hierarchy: use the platform tenant resolver when it is
        // published in the client hub, else fall back to a flat
        // (single-tenant) chain.
        let hierarchy: Arc<dyn TenantHierarchy> =
            match ctx.client_hub().try_get::<dyn TenantResolverClient>() {
                Some(resolver) => Arc::new(ResolverHierarchy::new(resolver)),
                None => Arc::new(FlatTenantHierarchy),
            };

        // Credential store for auth plugins that reference secrets.  Absent
        // in standalone runs — plugins then fail closed at proxy time with
        // `SecretNotFound`.
        let credstore: Option<Arc<dyn CredStoreClientV1>> =
            ctx.client_hub().try_get::<dyn CredStoreClientV1>();

        let (auth_plugins, guard_plugins, transform_plugins) =
            build_registries(&cfg, credstore.as_ref());

        // Best-effort registration of the reserved (catalog-only) plugin types
        // (ADR-0009). The registry client is absent in standalone runs — the
        // provisioner then no-ops. Registration never blocks start-up: it runs
        // on its own task and failures are only logged.
        let provisioner =
            TypeProvisioner::new(ctx.client_hub().try_get::<dyn TypesRegistryClient>());
        tokio::spawn(async move {
            provisioner.register_reserved_types().await;
        });

        let engine = Arc::new(HyperProxyEngine::new(cfg.proxy_timeout_secs));

        let control_plane = Arc::new(ControlPlaneService::new(
            repo.clone(),
            hierarchy.clone(),
            cfg.allow_http_upstream,
        ));
        let data_plane = Arc::new(DataPlaneService::new(
            repo,
            hierarchy,
            auth_plugins,
            guard_plugins,
            transform_plugins,
            engine,
            &cfg,
        ));

        self.control_plane
            .set(control_plane)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;
        self.data_plane
            .set(data_plane)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        info!(
            proxy_timeout_secs = cfg.proxy_timeout_secs,
            "OAGW gear initialized"
        );
        Ok(())
    }
}

#[async_trait]
impl SystemCapability for OagwGear {
    /// Post-init hook: nothing to switch over beyond init wiring for the
    /// in-memory MVP (left as a no-op hook for future readiness gates).
    async fn post_init(&self, _sys: &toolkit::runtime::SystemContext) -> anyhow::Result<()> {
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
        let control_plane = self
            .control_plane
            .get()
            .ok_or_else(|| anyhow::anyhow!("Control plane not initialized"))?
            .clone();
        let data_plane = self
            .data_plane
            .get()
            .ok_or_else(|| anyhow::anyhow!("Data plane not initialized"))?
            .clone();

        let router =
            crate::api::rest::routes::register_routes(router, openapi, control_plane, data_plane);

        info!("OAGW REST routes registered");
        Ok(router)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn gear_defaults_are_uninitialized() {
        let gear = OagwGear::default();
        assert!(gear.control_plane.get().is_none());
        assert!(gear.data_plane.get().is_none());
    }
}
