#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Test doubles for plugin registration and resolution: an in-memory
//! types-registry and a gear context over a configurable section.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use toolkit::client_hub::ClientHub;
use toolkit::config::ConfigProvider;
use toolkit::context::GearCtx;
use toolkit_canonical_errors::CanonicalError;
use types_registry_sdk::testing::{internal, make_test_instance, not_found};
use types_registry_sdk::{
    GtsInstance, GtsTypeSchema, InstanceQuery, RegisterResult, TypeSchemaQuery, TypesRegistryClient,
};
use uuid::Uuid;

/// In-memory types-registry: `register` stores instances, `list_instances`
/// returns every stored instance (the real registry filters by pattern; the
/// adapter under test filters by type id itself).
#[derive(Default)]
pub struct FakeTypesRegistry {
    instances: Mutex<Vec<GtsInstance>>,
    list_error: Mutex<Option<String>>,
    register_error: Mutex<Option<String>>,
}

impl FakeTypesRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Stores an instance with the given id and content.
    pub fn add_instance(&self, id: &str, content: serde_json::Value) {
        self.instances
            .lock()
            .unwrap()
            .push(make_test_instance(id, content));
    }

    /// Ids of the stored instances in registration order.
    pub fn instance_ids(&self) -> Vec<String> {
        self.instances
            .lock()
            .unwrap()
            .iter()
            .map(|i| i.id.as_ref().to_owned())
            .collect()
    }

    /// Content of the stored instance `id`.
    pub fn instance_object(&self, id: &str) -> Option<serde_json::Value> {
        self.instances
            .lock()
            .unwrap()
            .iter()
            .find(|i| i.id.as_ref() == id)
            .map(|i| i.object.clone())
    }

    /// Makes every `list_instances` fail.
    pub fn fail_list(&self, message: &str) {
        *self.list_error.lock().unwrap() = Some(message.to_owned());
    }

    /// Makes every `register` fail.
    pub fn fail_register(&self, message: &str) {
        *self.register_error.lock().unwrap() = Some(message.to_owned());
    }
}

#[async_trait]
impl TypesRegistryClient for FakeTypesRegistry {
    async fn register(
        &self,
        entities: Vec<serde_json::Value>,
    ) -> Result<Vec<RegisterResult>, CanonicalError> {
        if let Some(message) = self.register_error.lock().unwrap().clone() {
            return Err(internal(message));
        }
        let mut results = Vec::new();
        for entity in entities {
            let id = entity["id"].as_str().expect("instance id").to_owned();
            self.add_instance(&id, entity);
            results.push(RegisterResult::Ok { gts_id: id });
        }
        Ok(results)
    }

    async fn register_type_schemas(
        &self,
        _type_schemas: Vec<serde_json::Value>,
    ) -> Result<Vec<RegisterResult>, CanonicalError> {
        Ok(Vec::new())
    }

    async fn get_type_schema(&self, type_id: &str) -> Result<GtsTypeSchema, CanonicalError> {
        Err(not_found(type_id))
    }

    async fn get_type_schema_by_uuid(
        &self,
        type_uuid: Uuid,
    ) -> Result<GtsTypeSchema, CanonicalError> {
        Err(not_found(type_uuid.to_string()))
    }

    async fn get_type_schemas(
        &self,
        _type_ids: Vec<String>,
    ) -> HashMap<String, Result<GtsTypeSchema, CanonicalError>> {
        HashMap::new()
    }

    async fn get_type_schemas_by_uuid(
        &self,
        _type_uuids: Vec<Uuid>,
    ) -> HashMap<Uuid, Result<GtsTypeSchema, CanonicalError>> {
        HashMap::new()
    }

    async fn list_type_schemas(
        &self,
        _query: TypeSchemaQuery,
    ) -> Result<Vec<GtsTypeSchema>, CanonicalError> {
        Ok(Vec::new())
    }

    async fn register_instances(
        &self,
        instances: Vec<serde_json::Value>,
    ) -> Result<Vec<RegisterResult>, CanonicalError> {
        self.register(instances).await
    }

    async fn get_instance(&self, id: &str) -> Result<GtsInstance, CanonicalError> {
        Err(not_found(id))
    }

    async fn get_instance_by_uuid(&self, uuid: Uuid) -> Result<GtsInstance, CanonicalError> {
        Err(not_found(uuid.to_string()))
    }

    async fn get_instances(
        &self,
        _ids: Vec<String>,
    ) -> HashMap<String, Result<GtsInstance, CanonicalError>> {
        HashMap::new()
    }

    async fn get_instances_by_uuid(
        &self,
        _uuids: Vec<Uuid>,
    ) -> HashMap<Uuid, Result<GtsInstance, CanonicalError>> {
        HashMap::new()
    }

    async fn list_instances(
        &self,
        _query: InstanceQuery,
    ) -> Result<Vec<GtsInstance>, CanonicalError> {
        if let Some(message) = self.list_error.lock().unwrap().clone() {
            return Err(internal(message));
        }
        Ok(self.instances.lock().unwrap().clone())
    }
}

/// Config provider over fixed gear sections.
#[derive(Default)]
pub struct FixedConfig(HashMap<String, serde_json::Value>);

impl FixedConfig {
    /// A provider holding `gears.<gear>` = `{ "config": section }`.
    pub fn with_section(gear: &str, section: serde_json::Value) -> Self {
        let mut gear_section = serde_json::Map::new();
        gear_section.insert("config".to_owned(), section);
        let mut gears = HashMap::new();
        gears.insert(gear.to_owned(), serde_json::Value::Object(gear_section));
        Self(gears)
    }
}

impl ConfigProvider for FixedConfig {
    fn get_gear_config(&self, gear_name: &str) -> Option<&serde_json::Value> {
        self.0.get(gear_name)
    }
}

/// Gear context of `gear` over `config` and a hub holding `registry`.
pub fn gear_ctx(
    gear: &str,
    config: FixedConfig,
    hub: &Arc<ClientHub>,
    registry: &Arc<FakeTypesRegistry>,
) -> GearCtx {
    hub.register::<dyn TypesRegistryClient>(Arc::clone(registry) as Arc<dyn TypesRegistryClient>);
    GearCtx::new(
        gear.to_owned(),
        Uuid::nil(),
        Arc::new(config),
        Arc::clone(hub),
        CancellationToken::new(),
    )
}
