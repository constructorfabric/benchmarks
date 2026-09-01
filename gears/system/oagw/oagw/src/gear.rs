//! OAGW gear wiring: resolves cross-gear clients, builds the control and
//! data planes, and registers the REST surface (DESIGN §3.4).
//!
//! Lifecycle:
//! - `init`: validate config, resolve `credstore` / `tenant-resolver` /
//!   `types-registry` clients from the client hub, build the in-memory
//!   repository, control-plane service and data-plane service. Idempotent.
//! - `post_init` (system capability): best-effort registration of the OAGW
//!   resource type-schemas into `types-registry`. Runs after every gear's
//!   `init`, when the catalogue is (usually) ready; failures are logged and
//!   never fail the runtime — discovery metadata is not load-bearing.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use toolkit::runtime::SystemContext;
use toolkit::{Gear, GearCtx, RestApiCapability, SystemCapability};
use tracing::{info, warn};
use types_registry_sdk::{RegisterResult, TypesRegistryClient};

use crate::api::rest::routes as rest_routes;
use crate::config::OagwConfig;
use crate::domain::control::ControlPlaneService;
use crate::infra::data_plane::DataPlaneService;
use crate::infra::memory_repo::MemoryRepository;

/// The OAGW gear. Resources and wiring are populated during [`Gear::init`];
/// the REST surface is attached during [`RestApiCapability::register_rest`].
#[toolkit::gear(
    name = "oagw",
    deps = [types_registry, credstore, tenant_resolver],
    capabilities = [system, rest]
)]
pub struct OagwGear {
    /// Control-plane service handle (wired in `init`).
    control: OnceLock<Arc<ControlPlaneService>>,
    /// Data-plane service handle (wired in `init`).
    data_plane: OnceLock<Arc<DataPlaneService>>,
    /// `types-registry` client retained for `post_init` catalog provisioning.
    types_registry: OnceLock<Arc<dyn TypesRegistryClient>>,
}

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            control: OnceLock::new(),
            data_plane: OnceLock::new(),
            types_registry: OnceLock::new(),
        }
    }
}

/// System capability: after every gear's `init`, publish the OAGW resource
/// type-schemas to `types-registry` (best-effort; see module docs).
#[async_trait]
impl SystemCapability for OagwGear {
    async fn post_init(&self, _sys: &SystemContext) -> anyhow::Result<()> {
        self.provision_catalog().await;
        Ok(())
    }
}

/// Core gear wiring.
#[async_trait]
impl Gear for OagwGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let config = ctx.config_or_default::<OagwConfig>()?;
        config
            .validate()
            .map_err(|e| anyhow::anyhow!("invalid OAGW configuration: {e}"))?;

        let hub = ctx.client_hub();
        let credstore: Arc<dyn credstore_sdk::CredStoreClientV1> = hub
            .get::<dyn credstore_sdk::CredStoreClientV1>()
            .map_err(|e| anyhow::anyhow!("failed to resolve CredStoreClientV1: {e}"))?;
        let tenants: Arc<dyn tenant_resolver_sdk::TenantResolverClient> = hub
            .get::<dyn tenant_resolver_sdk::TenantResolverClient>()
            .map_err(|e| anyhow::anyhow!("failed to resolve TenantResolverClient: {e}"))?;
        let types_registry: Arc<dyn TypesRegistryClient> = hub
            .get::<dyn TypesRegistryClient>()
            .map_err(|e| anyhow::anyhow!("failed to resolve TypesRegistryClient: {e}"))?;

        // `init` is idempotent: a second call reuses the already-wired
        // services instead of failing (the first build wins).
        let control = self
            .control
            .get_or_init(|| {
                let repo = Arc::new(MemoryRepository::new());
                Arc::new(ControlPlaneService::new(repo, tenants))
            })
            .clone();

        if self.data_plane.get().is_none() {
            let data_plane = Arc::new(
                DataPlaneService::new(control, credstore, None, config)
                    .map_err(|e| anyhow::anyhow!("failed to build OAGW data plane: {e}"))?,
            );
            if self.data_plane.set(data_plane).is_err() {
                tracing::debug!("OAGW data plane already initialized; keeping the first");
            }
        } else {
            tracing::debug!("OAGW data plane already initialized; keeping the first");
        }

        // Retained for best-effort post_init provisioning. `init` is
        // idempotent, so a second call must not clobber the earlier client.
        if self.types_registry.set(types_registry).is_err() {
            tracing::debug!("OAGW types-registry client already set; keeping the first");
        }

        info!("OAGW gear initialized");
        Ok(())
    }
}

/// REST API capability: attach the management/proxy routes.
impl RestApiCapability for OagwGear {
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: axum::Router,
        openapi: &dyn toolkit::api::OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        let control = self
            .control
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("OAGW control plane not initialized"))?;
        let data_plane = self
            .data_plane
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("OAGW data plane not initialized"))?;
        let router = rest_routes::register_routes(router, openapi, control, data_plane);
        info!("OAGW REST routes registered");
        Ok(router)
    }
}

impl OagwGear {
    /// Best-effort `types-registry` provisioning of the OAGW resource
    /// type-schemas (upstream, route, plugins, protocol).
    ///
    /// Every outcome is logged; nothing here is allowed to fail the runtime.
    async fn provision_catalog(&self) {
        let Some(registry) = self.types_registry.get().cloned() else {
            return;
        };
        match registry.register(oagw_type_schemas()).await {
            Ok(results) => {
                for result in results {
                    log_register_result(result);
                }
            }
            Err(error) => {
                warn!(%error, "types-registry unavailable during OAGW catalog provisioning");
            }
        }
    }
}

/// Log a single `types-registry` registration outcome (best-effort).
fn log_register_result(result: RegisterResult) {
    match result {
        RegisterResult::Ok { gts_id } => {
            info!(%gts_id, "registered OAGW type schema in types-registry");
        }
        RegisterResult::Err { gts_id, error } => {
            warn!(
                gts_id = gts_id.as_deref().unwrap_or("(unknown)"),
                error = %error,
                "types-registry rejected an OAGW type schema (best-effort; \
                 discovery metadata deferred)"
            );
        }
    }
}

/// Minimal draft-07 type-schema documents for every OAGW resource type.
///
/// The full JSON schemas ship under `docs/schemas/`; here only the identity
/// envelope is published so discovery tooling can resolve OAGW types by their
/// GTS type identifiers.
fn oagw_type_schemas() -> Vec<serde_json::Value> {
    let entries: &[(&str, &str)] = &[
        (crate::gts::UPSTREAM_TYPE_ID, "OAGW Upstream"),
        (crate::gts::ROUTE_TYPE_ID, "OAGW Route"),
        (crate::gts::AUTH_PLUGIN_TYPE_ID, "OAGW Auth Plugin"),
        (crate::gts::GUARD_PLUGIN_TYPE_ID, "OAGW Guard Plugin"),
        (
            crate::gts::TRANSFORM_PLUGIN_TYPE_ID,
            "OAGW Transform Plugin",
        ),
        (crate::gts::PROTOCOL_TYPE_ID, "OAGW Protocol"),
    ];
    entries
        .iter()
        .map(|(type_id, title)| {
            serde_json::json!({
                "$id": type_id,
                "$schema": "http://json-schema.org/draft-07/schema#",
                "title": title,
                "type": "object"
            })
        })
        .collect()
}
