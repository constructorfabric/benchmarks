//! Recording `TypesRegistryClient` fake.
//!
//! `MockTypesRegistryClient` asserts that `register` is never called with entities, so plugin
//! `init` tests use this fake, which accepts and records registrations.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use toolkit_canonical_errors::CanonicalError;
use types_registry_sdk::{
    GtsInstance, GtsTypeSchema, InstanceQuery, RegisterResult, TypeSchemaQuery, TypesRegistryClient,
};
use uuid::Uuid;

#[derive(Default)]
pub struct RecordingRegistry {
    registered: Mutex<Vec<serde_json::Value>>,
}

impl RecordingRegistry {
    /// Entities passed to `register`, in order.
    pub fn registered(&self) -> Vec<serde_json::Value> {
        self.registered.lock().expect("lock").clone()
    }
}

#[async_trait]
impl TypesRegistryClient for RecordingRegistry {
    async fn register(
        &self,
        entities: Vec<serde_json::Value>,
    ) -> Result<Vec<RegisterResult>, CanonicalError> {
        let results = entities
            .iter()
            .map(|e| RegisterResult::Ok {
                gts_id: e["id"].as_str().unwrap_or_default().to_owned(),
            })
            .collect();
        self.registered.lock().expect("lock").extend(entities);
        Ok(results)
    }
    async fn register_type_schemas(
        &self,
        _type_schemas: Vec<serde_json::Value>,
    ) -> Result<Vec<RegisterResult>, CanonicalError> {
        unimplemented!()
    }
    async fn get_type_schema(&self, _type_id: &str) -> Result<GtsTypeSchema, CanonicalError> {
        unimplemented!()
    }
    async fn get_type_schema_by_uuid(
        &self,
        _type_uuid: Uuid,
    ) -> Result<GtsTypeSchema, CanonicalError> {
        unimplemented!()
    }
    async fn get_type_schemas(
        &self,
        _type_ids: Vec<String>,
    ) -> HashMap<String, Result<GtsTypeSchema, CanonicalError>> {
        unimplemented!()
    }
    async fn get_type_schemas_by_uuid(
        &self,
        _type_uuids: Vec<Uuid>,
    ) -> HashMap<Uuid, Result<GtsTypeSchema, CanonicalError>> {
        unimplemented!()
    }
    async fn list_type_schemas(
        &self,
        _query: TypeSchemaQuery,
    ) -> Result<Vec<GtsTypeSchema>, CanonicalError> {
        unimplemented!()
    }
    async fn register_instances(
        &self,
        _instances: Vec<serde_json::Value>,
    ) -> Result<Vec<RegisterResult>, CanonicalError> {
        unimplemented!()
    }
    async fn get_instance(&self, _id: &str) -> Result<GtsInstance, CanonicalError> {
        unimplemented!()
    }
    async fn get_instance_by_uuid(&self, _uuid: Uuid) -> Result<GtsInstance, CanonicalError> {
        unimplemented!()
    }
    async fn get_instances(
        &self,
        _ids: Vec<String>,
    ) -> HashMap<String, Result<GtsInstance, CanonicalError>> {
        unimplemented!()
    }
    async fn get_instances_by_uuid(
        &self,
        _uuids: Vec<Uuid>,
    ) -> HashMap<Uuid, Result<GtsInstance, CanonicalError>> {
        unimplemented!()
    }
    async fn list_instances(
        &self,
        _query: InstanceQuery,
    ) -> Result<Vec<GtsInstance>, CanonicalError> {
        unimplemented!()
    }
}
