//! Gear declaration for the Types Registry gear.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::SystemCapability;
use toolkit::{Gear, GearCtx, RestApiCapability};
use toolkit_gts::{all_inventory_instances, all_inventory_type_schemas};
use tracing::{debug, info};
use types_registry_sdk::{RegisterResult, RegisterSummary, TypesRegistryClient};

use crate::config::TypesRegistryConfig;
use crate::domain::local_client::TypesRegistryLocalClient;
use crate::domain::service::TypesRegistryService;
use crate::infra::InMemoryGtsRepository;

/// Types Registry gear.
///
/// Provides GTS entity registration, storage, validation, and REST API endpoints.
///
/// ## Capabilities
///
/// - `system` — Core infrastructure gear, initialized early in startup
/// - `rest` — Exposes REST API endpoints
///
/// ## Link-time inventory seeding
///
/// At startup, this gear seeds its own registry with every GTS Type Schema
/// and well-known Instance submitted to the process-wide `toolkit-gts`
/// inventory — the `InventoryTypeSchema` / `InventoryInstance` collectors
/// populated by `#[gts_type_schema]` / `gts_instance!` from any linked crate.
/// The seeding happens via the internal `TypesRegistryService::register`
/// (no `ClientHub` round-trip) before the client is published, so
/// downstream consumers always see the base types at first access.
///
/// `toolkit-gts` is a content-agnostic aggregator: types-registry code
/// never references specific type names — it simply calls
/// `all_inventory_type_schemas()` / `all_inventory_instances()`. New entries
/// are picked up automatically as soon as a contributing crate is in
/// the dependency graph.
#[toolkit::gear(
    name = "types-registry",
    capabilities = [system, rest]
)]
pub struct TypesRegistryGear {
    service: OnceLock<Arc<TypesRegistryService>>,
    local_client: OnceLock<Arc<TypesRegistryLocalClient>>,
}

impl Default for TypesRegistryGear {
    fn default() -> Self {
        Self {
            service: OnceLock::new(),
            local_client: OnceLock::new(),
        }
    }
}

#[async_trait]
impl Gear for TypesRegistryGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: TypesRegistryConfig = ctx.config_or_default()?;
        debug!(
            "Loaded types_registry config: entity_id_fields={:?}, schema_id_fields={:?}, \
             local_client.cache.type_schemas={{capacity={}, ttl={:?}}}, \
             local_client.cache.instances={{capacity={}, ttl={:?}}}",
            cfg.entity_id_fields,
            cfg.schema_id_fields,
            cfg.local_client.cache.type_schemas.capacity,
            cfg.local_client.cache.type_schemas.ttl,
            cfg.local_client.cache.instances.capacity,
            cfg.local_client.cache.instances.ttl,
        );

        let gts_config = cfg.to_gts_config();
        let static_entities = cfg.entities.clone();
        let type_schemas_cache_cfg = cfg.local_client.cache.type_schemas.to_cache_config();
        let instances_cache_cfg = cfg.local_client.cache.instances.to_cache_config();

        let repo = Arc::new(InMemoryGtsRepository::new(gts_config));
        let service = Arc::new(TypesRegistryService::new(repo, cfg));

        // Seed the process-wide toolkit-gts inventory (auto-discovered via
        // `inventory` at link time). Content-agnostic: types-registry never
        // names specific types — it calls aggregators. Runs before the
        // client is published so downstream consumers always see the base
        // types on first access.
        let inventory_type_schemas = all_inventory_type_schemas()
            .map_err(|e| anyhow::anyhow!("Failed to collect GTS Type Schemas: {e}"))?;
        let inventory_instances = all_inventory_instances()
            .map_err(|e| anyhow::anyhow!("Failed to collect GTS Instances: {e}"))?;
        let schema_count = inventory_type_schemas.len();
        let instance_count = inventory_instances.len();
        let mut inventory_entries = inventory_type_schemas;
        inventory_entries.extend(inventory_instances);
        debug!(
            schema_count,
            instance_count, "Seeding GTS inventory into types-registry"
        );
        let seed_results = service.register(inventory_entries);
        RegisterResult::ensure_all_ok(&seed_results)
            .map_err(|e| anyhow::anyhow!("Failed to register GTS inventory: {e}"))?;

        // Register static entities from config (before ready-mode validation)
        if !static_entities.is_empty() {
            let entity_count = static_entities.len();
            let results = service.register(static_entities);
            let summary = RegisterSummary::from_results(&results);

            if !summary.all_succeeded() {
                for result in &results {
                    if let RegisterResult::Err { gts_id, error } = result {
                        tracing::error!(
                            gts_id = gts_id.as_deref().unwrap_or("<unknown>"),
                            error = %error,
                            "Failed to register static GTS entity"
                        );
                    }
                }
                anyhow::bail!(
                    "types-registry: {}/{} static entities failed to register",
                    summary.failed,
                    summary.total()
                );
            }

            info!(
                count = entity_count,
                "Registered static GTS entities from config"
            );
        }

        self.service
            .set(service.clone())
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        let local_client = Arc::new(TypesRegistryLocalClient::with_cache_configs(
            service,
            type_schemas_cache_cfg,
            instances_cache_cfg,
        ));
        self.local_client
            .set(local_client.clone())
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        let api: Arc<dyn TypesRegistryClient> = local_client;
        ctx.client_hub().register::<dyn TypesRegistryClient>(api);

        Ok(())
    }
}

#[async_trait]
impl SystemCapability for TypesRegistryGear {
    /// Post-init hook: switches the registry to ready mode.
    ///
    /// This runs AFTER `init()` has completed for ALL gears.
    /// At this point, all gears have had a chance to register their types,
    /// so we can safely validate and switch to ready mode.
    async fn post_init(&self, _sys: &toolkit::runtime::SystemContext) -> anyhow::Result<()> {
        info!("types_registry post_init: switching to ready mode");

        let service = self
            .service
            .get()
            .ok_or_else(|| anyhow::anyhow!("Service not initialized"))?
            .clone();

        service.switch_to_ready().map_err(|e| {
            if let Some(errors) = e.validation_errors() {
                for err in errors {
                    // Try to get the entity content for debugging
                    let entity_content = match service.get(&err.gts_id) {
                        Ok(entity) => serde_json::to_string_pretty(&entity.content)
                            .unwrap_or_else(|_| "Failed to serialize".to_owned()),
                        _ => "Entity not found or failed to retrieve".to_owned(),
                    };

                    tracing::error!(
                        gts_id = %err.gts_id,
                        message = %err.message,
                        entity_content = %entity_content,
                        "GTS validation error"
                    );
                }
            }
            anyhow::anyhow!("Failed to switch to ready mode: {e}")
        })?;

        // Drop any cached entries built before the ready transition (e.g.
        // best-effort builds that may have had unresolved parents). After
        // switch_to_ready, the persistent store has the final picture and
        // subsequent get_*/list_* calls rebuild against it.
        if let Some(client) = self.local_client.get() {
            client.clear_caches();
        }

        info!("types_registry switched to ready mode successfully");
        Ok(())
    }
}

impl RestApiCapability for TypesRegistryGear {
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: axum::Router,
        openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        info!("Registering types_registry REST routes");

        let service = self
            .service
            .get()
            .ok_or_else(|| anyhow::anyhow!("Service not initialized"))?
            .clone();

        let router = crate::api::rest::routes::register_routes(router, openapi, service);

        info!("Types registry REST routes registered successfully");
        Ok(router)
    }
}
