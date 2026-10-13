//! Fake types registry: stores what is registered and answers instance lookups.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use gts::GtsInstanceId;
use parking_lot::Mutex;
use serde_json::Value;
use toolkit_canonical_errors::{CanonicalError, resource_error};
use types_registry_sdk::{
    GtsInstance, GtsTypeId, GtsTypeSchema, InstanceQuery, RegisterResult, TypeSchemaQuery,
    TypesRegistryClient,
};
use uuid::Uuid;

#[resource_error(gts_id!("cf.core.mini_chat.test_registry.v1~"))]
struct FakeRegistryError;

/// In-memory registry: registration of schemas and instances, instance listing by pattern.
#[derive(Default)]
pub struct FakeTypesRegistry {
    schemas: Mutex<Vec<GtsTypeSchema>>,
    instances: Mutex<Vec<GtsInstance>>,
}

/// A schema with an empty body and a complete (synthetic) parent chain.
fn schema_with_chain(type_id: &str, body: Value) -> Result<GtsTypeSchema, CanonicalError> {
    let parent = match GtsTypeSchema::derive_parent_type_id(type_id) {
        Some(parent_id) => Some(Arc::new(schema_with_chain(
            parent_id.as_ref(),
            serde_json::json!({}),
        )?)),
        None => None,
    };
    GtsTypeSchema::try_new(GtsTypeId::new(type_id), body, None, parent)
}

fn entity_id(entity: &Value) -> Option<String> {
    ["$id", "gtsId", "id"]
        .iter()
        .find_map(|key| entity.get(key).and_then(Value::as_str))
        .map(|id| id.strip_prefix("gts://").unwrap_or(id).to_owned())
}

fn matches_pattern(pattern: Option<&str>, id: &str) -> bool {
    match pattern {
        None => true,
        Some(pattern) => match pattern.strip_suffix('*') {
            Some(prefix) => id.starts_with(prefix),
            None => id == pattern,
        },
    }
}

impl FakeTypesRegistry {
    fn register_one(&self, entity: Value) -> RegisterResult {
        let Some(id) = entity_id(&entity) else {
            return RegisterResult::Err {
                gts_id: None,
                error: CanonicalError::internal("entity has no GTS id").create(),
            };
        };
        let outcome = if id.ends_with('~') {
            schema_with_chain(&id, entity).map(|schema| self.schemas.lock().push(schema))
        } else {
            self.register_instance(&id, entity)
        };
        match outcome {
            Ok(()) => RegisterResult::Ok { gts_id: id },
            Err(error) => RegisterResult::Err {
                gts_id: Some(id),
                error,
            },
        }
    }

    fn register_instance(&self, id: &str, content: Value) -> Result<(), CanonicalError> {
        let type_id = GtsInstance::derive_type_id(id)
            .ok_or_else(|| CanonicalError::internal("instance id has no type prefix").create())?;
        let schema = Arc::new(schema_with_chain(type_id.as_ref(), serde_json::json!({}))?);
        let segment = &id[type_id.as_ref().len()..];
        let instance_id = GtsInstanceId::new(type_id.as_ref(), segment);
        let instance = GtsInstance::try_new(instance_id, content, None, schema)?;
        self.instances.lock().push(instance);
        Ok(())
    }
}

#[async_trait]
impl TypesRegistryClient for FakeTypesRegistry {
    async fn register(&self, entities: Vec<Value>) -> Result<Vec<RegisterResult>, CanonicalError> {
        Ok(entities
            .into_iter()
            .map(|entity| self.register_one(entity))
            .collect())
    }

    async fn register_type_schemas(
        &self,
        type_schemas: Vec<Value>,
    ) -> Result<Vec<RegisterResult>, CanonicalError> {
        self.register(type_schemas).await
    }

    async fn get_type_schema(&self, type_id: &str) -> Result<GtsTypeSchema, CanonicalError> {
        self.schemas
            .lock()
            .iter()
            .find(|schema| schema.type_id == type_id)
            .cloned()
            .ok_or_else(|| {
                FakeRegistryError::not_found("type schema not registered")
                    .with_resource(type_id)
                    .create()
            })
    }

    async fn get_type_schema_by_uuid(
        &self,
        type_uuid: Uuid,
    ) -> Result<GtsTypeSchema, CanonicalError> {
        self.schemas
            .lock()
            .iter()
            .find(|schema| schema.type_uuid == type_uuid)
            .cloned()
            .ok_or_else(|| {
                FakeRegistryError::not_found("type schema not registered")
                    .with_resource(type_uuid.to_string())
                    .create()
            })
    }

    async fn get_type_schemas(
        &self,
        type_ids: Vec<String>,
    ) -> HashMap<String, Result<GtsTypeSchema, CanonicalError>> {
        let mut out = HashMap::new();
        for id in type_ids {
            let result = self.get_type_schema(&id).await;
            out.insert(id, result);
        }
        out
    }

    async fn get_type_schemas_by_uuid(
        &self,
        type_uuids: Vec<Uuid>,
    ) -> HashMap<Uuid, Result<GtsTypeSchema, CanonicalError>> {
        let mut out = HashMap::new();
        for uuid in type_uuids {
            let result = self.get_type_schema_by_uuid(uuid).await;
            out.insert(uuid, result);
        }
        out
    }

    async fn list_type_schemas(
        &self,
        query: TypeSchemaQuery,
    ) -> Result<Vec<GtsTypeSchema>, CanonicalError> {
        let pattern = query.pattern.as_deref();
        Ok(self
            .schemas
            .lock()
            .iter()
            .filter(|schema| matches_pattern(pattern, schema.type_id.as_ref()))
            .cloned()
            .collect())
    }

    async fn register_instances(
        &self,
        instances: Vec<Value>,
    ) -> Result<Vec<RegisterResult>, CanonicalError> {
        self.register(instances).await
    }

    async fn get_instance(&self, id: &str) -> Result<GtsInstance, CanonicalError> {
        self.instances
            .lock()
            .iter()
            .find(|instance| instance.id.as_ref() == id)
            .cloned()
            .ok_or_else(|| {
                FakeRegistryError::not_found("instance not registered")
                    .with_resource(id)
                    .create()
            })
    }

    async fn get_instance_by_uuid(&self, uuid: Uuid) -> Result<GtsInstance, CanonicalError> {
        self.instances
            .lock()
            .iter()
            .find(|instance| instance.uuid == uuid)
            .cloned()
            .ok_or_else(|| {
                FakeRegistryError::not_found("instance not registered")
                    .with_resource(uuid.to_string())
                    .create()
            })
    }

    async fn get_instances(
        &self,
        ids: Vec<String>,
    ) -> HashMap<String, Result<GtsInstance, CanonicalError>> {
        let mut out = HashMap::new();
        for id in ids {
            let result = self.get_instance(&id).await;
            out.insert(id, result);
        }
        out
    }

    async fn get_instances_by_uuid(
        &self,
        uuids: Vec<Uuid>,
    ) -> HashMap<Uuid, Result<GtsInstance, CanonicalError>> {
        let mut out = HashMap::new();
        for uuid in uuids {
            let result = self.get_instance_by_uuid(uuid).await;
            out.insert(uuid, result);
        }
        out
    }

    async fn list_instances(
        &self,
        query: InstanceQuery,
    ) -> Result<Vec<GtsInstance>, CanonicalError> {
        let pattern = query.pattern.as_deref();
        Ok(self
            .instances
            .lock()
            .iter()
            .filter(|instance| matches_pattern(pattern, instance.id.as_ref()))
            .cloned()
            .collect())
    }
}
