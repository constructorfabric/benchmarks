//! Unit tests for GTS base-type provisioning (`cpt-cf-oagw-dod-gear-foundation-type-provisioning`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use async_trait::async_trait;
use toolkit_canonical_errors::{resource_error, CanonicalError};
use toolkit_gts::GTS_ID_URI_PREFIX;
use types_registry_sdk::api::TypesRegistryClient;
use types_registry_sdk::models::{
    GtsInstance, GtsTypeSchema, InstanceQuery, RegisterResult, TypeSchemaQuery,
};
use uuid::Uuid;

use super::*;

/// A test registry resource type, so the fakes below can synthesize the same
/// canonical envelopes the real client emits.
#[resource_error("gts.cf.core.oagw.test_registry_type.v1~")]
struct TestRegistryResource;

fn already_exists(id: &str) -> CanonicalError {
    TestRegistryResource::already_exists(format!("already registered: {id}"))
        .with_resource(id.to_owned())
        .create()
}

fn unimplemented() -> CanonicalError {
    TestRegistryResource::unimplemented("not implemented").create()
}

/// A fake registry that records what it was asked to register and answers
/// `AlreadyExists` on a repeat.
#[derive(Default)]
struct FakeRegistry {
    registered: Mutex<Vec<String>>,
    register_calls: AtomicUsize,
}

impl FakeRegistry {
    fn ids(&self) -> Vec<String> {
        self.registered.lock().expect("lock").clone()
    }
}

#[async_trait]
impl TypesRegistryClient for FakeRegistry {
    async fn register(
        &self,
        _entities: Vec<serde_json::Value>,
    ) -> Result<Vec<RegisterResult>, CanonicalError> {
        Ok(vec![])
    }

    async fn register_type_schemas(
        &self,
        type_schemas: Vec<serde_json::Value>,
    ) -> Result<Vec<RegisterResult>, CanonicalError> {
        self.register_calls.fetch_add(1, Ordering::SeqCst);
        let mut store = self.registered.lock().expect("lock");
        Ok(type_schemas
            .into_iter()
            .map(|schema| {
                let id = schema.get("$id").and_then(serde_json::Value::as_str).map(str::to_owned);
                match id {
                    Some(id) if store.contains(&id) => RegisterResult::Err {
                        gts_id: Some(id.clone()),
                        error: already_exists(&id),
                    },
                    Some(id) => {
                        store.push(id.clone());
                        RegisterResult::Ok { gts_id: id }
                    }
                    None => RegisterResult::Err { gts_id: None, error: unimplemented() },
                }
            })
            .collect())
    }

    async fn get_type_schema(&self, _type_id: &str) -> Result<GtsTypeSchema, CanonicalError> {
        Err(unimplemented())
    }
    async fn get_type_schema_by_uuid(
        &self,
        _type_uuid: Uuid,
    ) -> Result<GtsTypeSchema, CanonicalError> {
        Err(unimplemented())
    }
    async fn get_type_schemas(
        &self,
        type_ids: Vec<String>,
    ) -> HashMap<String, Result<GtsTypeSchema, CanonicalError>> {
        type_ids.into_iter().map(|id| (id, Err(unimplemented()))).collect()
    }
    async fn get_type_schemas_by_uuid(
        &self,
        type_uuids: Vec<Uuid>,
    ) -> HashMap<Uuid, Result<GtsTypeSchema, CanonicalError>> {
        type_uuids.into_iter().map(|id| (id, Err(unimplemented()))).collect()
    }
    async fn list_type_schemas(
        &self,
        _query: TypeSchemaQuery,
    ) -> Result<Vec<GtsTypeSchema>, CanonicalError> {
        Ok(vec![])
    }
    async fn register_instances(
        &self,
        _instances: Vec<serde_json::Value>,
    ) -> Result<Vec<RegisterResult>, CanonicalError> {
        Ok(vec![])
    }
    async fn get_instance(&self, _id: &str) -> Result<GtsInstance, CanonicalError> {
        Err(unimplemented())
    }
    async fn get_instance_by_uuid(&self, _uuid: Uuid) -> Result<GtsInstance, CanonicalError> {
        Err(unimplemented())
    }
    async fn get_instances(
        &self,
        ids: Vec<String>,
    ) -> HashMap<String, Result<GtsInstance, CanonicalError>> {
        ids.into_iter().map(|id| (id, Err(unimplemented()))).collect()
    }
    async fn get_instances_by_uuid(
        &self,
        uuids: Vec<Uuid>,
    ) -> HashMap<Uuid, Result<GtsInstance, CanonicalError>> {
        uuids.into_iter().map(|id| (id, Err(unimplemented()))).collect()
    }
    async fn list_instances(
        &self,
        _query: InstanceQuery,
    ) -> Result<Vec<GtsInstance>, CanonicalError> {
        Ok(vec![])
    }
}

/// A registry that rejects every registration for a reason other than
/// `AlreadyExists`.
#[derive(Default)]
struct RejectingRegistry;

#[async_trait]
impl TypesRegistryClient for RejectingRegistry {
    async fn register(
        &self,
        _entities: Vec<serde_json::Value>,
    ) -> Result<Vec<RegisterResult>, CanonicalError> {
        Err(unimplemented())
    }

    async fn register_type_schemas(
        &self,
        type_schemas: Vec<serde_json::Value>,
    ) -> Result<Vec<RegisterResult>, CanonicalError> {
        Ok(type_schemas
            .into_iter()
            .map(|schema| RegisterResult::Err {
                gts_id: schema.get("$id").and_then(serde_json::Value::as_str).map(str::to_owned),
                error: unimplemented(),
            })
            .collect())
    }

    async fn get_type_schema(&self, _type_id: &str) -> Result<GtsTypeSchema, CanonicalError> {
        Err(unimplemented())
    }
    async fn get_type_schema_by_uuid(
        &self,
        _type_uuid: Uuid,
    ) -> Result<GtsTypeSchema, CanonicalError> {
        Err(unimplemented())
    }
    async fn get_type_schemas(
        &self,
        type_ids: Vec<String>,
    ) -> HashMap<String, Result<GtsTypeSchema, CanonicalError>> {
        type_ids.into_iter().map(|id| (id, Err(unimplemented()))).collect()
    }
    async fn get_type_schemas_by_uuid(
        &self,
        type_uuids: Vec<Uuid>,
    ) -> HashMap<Uuid, Result<GtsTypeSchema, CanonicalError>> {
        type_uuids.into_iter().map(|id| (id, Err(unimplemented()))).collect()
    }
    async fn list_type_schemas(
        &self,
        _query: TypeSchemaQuery,
    ) -> Result<Vec<GtsTypeSchema>, CanonicalError> {
        Ok(vec![])
    }
    async fn register_instances(
        &self,
        _instances: Vec<serde_json::Value>,
    ) -> Result<Vec<RegisterResult>, CanonicalError> {
        Ok(vec![])
    }
    async fn get_instance(&self, _id: &str) -> Result<GtsInstance, CanonicalError> {
        Err(unimplemented())
    }
    async fn get_instance_by_uuid(&self, _uuid: Uuid) -> Result<GtsInstance, CanonicalError> {
        Err(unimplemented())
    }
    async fn get_instances(
        &self,
        ids: Vec<String>,
    ) -> HashMap<String, Result<GtsInstance, CanonicalError>> {
        ids.into_iter().map(|id| (id, Err(unimplemented()))).collect()
    }
    async fn get_instances_by_uuid(
        &self,
        uuids: Vec<Uuid>,
    ) -> HashMap<Uuid, Result<GtsInstance, CanonicalError>> {
        uuids.into_iter().map(|id| (id, Err(unimplemented()))).collect()
    }
    async fn list_instances(
        &self,
        _query: InstanceQuery,
    ) -> Result<Vec<GtsInstance>, CanonicalError> {
        Ok(vec![])
    }
}

#[tokio::test]
async fn the_six_base_types_are_registered() {
    let registry = FakeRegistry::default();
    let registered = register_base_types(&registry).await.expect("registered");
    assert_eq!(registered.len(), BASE_TYPES.len(), "one result per base type");
    for base_type in BASE_TYPES {
        // The submitted `$id` is the `gts://` URI form the GTS spec requires of
        // a Type Schema; the registry keys the entity by the canonical form it
        // strips from it.
        let uri = format!("{GTS_ID_URI_PREFIX}{base_type}");
        assert!(
            registry.ids().iter().any(|id| *id == uri),
            "`{uri}` registered as the schema id: {:?}",
            registry.ids()
        );
    }
    // The five the DoD names explicitly, plus the proxy base type.
    for expected in [
        "gts.cf.core.oagw.upstream.v1~",
        "gts.cf.core.oagw.route.v1~",
        "gts.cf.core.oagw.auth_plugin.v1~",
        "gts.cf.core.oagw.guard_plugin.v1~",
        "gts.cf.core.oagw.transform_plugin.v1~",
    ] {
        assert!(BASE_TYPES.contains(&expected), "`{expected}` is a registered base type");
    }
}

#[tokio::test]
async fn a_repeat_registration_is_an_idempotent_no_op() {
    let registry = FakeRegistry::default();
    register_base_types(&registry).await.expect("first pass");
    let first_calls = registry.register_calls.load(Ordering::SeqCst);
    let first_ids = registry.ids();

    let second = register_base_types(&registry).await.expect("second pass");
    assert_eq!(second.len(), BASE_TYPES.len(), "every base type is still accounted for");
    assert_eq!(registry.ids(), first_ids, "no duplicate registration");
    assert!(
        registry.register_calls.load(Ordering::SeqCst) > first_calls,
        "the registry was called again"
    );
    // Each id appears exactly once in the store.
    let mut sorted = registry.ids();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), registry.ids().len(), "the store holds each type once");
}

#[tokio::test]
async fn a_rejected_registration_fails_provisioning() {
    let error = register_base_types(&RejectingRegistry).await.expect_err("rejected");
    assert!(
        error.to_string().contains("rejected a base type"),
        "`{error}` names the provisioning failure"
    );
}

#[test]
fn every_base_type_schema_carries_its_id() {
    for schema in base_type_schemas() {
        let id = schema.get("$id").and_then(serde_json::Value::as_str).expect("$id");
        // The `$id` is the `gts://` URI form of the canonical identifier: a
        // bare canonical form is not a resolvable GTS id and the registry
        // refuses it on ingest.
        let canonical = id
            .strip_prefix(GTS_ID_URI_PREFIX)
            .unwrap_or(id);
        assert!(BASE_TYPES.contains(&canonical), "`{canonical}` is a base type");
        assert!(canonical.ends_with('~'), "a type-schema id ends with `~`");
        assert_eq!(schema["type"], serde_json::json!("object"));
    }
}

#[test]
fn a_base_type_schema_titles_itself_from_the_leaf_segment() {
    let schema = base_type_schema("gts.cf.core.oagw.upstream.v1~");
    assert_eq!(schema["title"], serde_json::json!("OAGW upstream v1"));
    let schema = base_type_schema("gts.cf.core.oagw.auth_plugin.v1~");
    assert_eq!(schema["title"], serde_json::json!("OAGW auth_plugin v1"));
}
