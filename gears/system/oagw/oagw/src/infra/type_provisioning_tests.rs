//! Type-provisioning tests.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use toolkit_canonical_errors::CanonicalError;
use types_registry_sdk::testing::{invalid_gts_id, not_found};
use types_registry_sdk::{
    GtsInstance, GtsTypeSchema, InstanceQuery, RegisterResult, TypeSchemaQuery, TypesRegistryClient,
};
use uuid::Uuid;

use std::sync::Arc;

use super::{PROVISIONED_TYPES, documents, provision};

/// A real in-memory types-registry over a fresh store.
fn real_registry() -> (
    Arc<types_registry::domain::TypesRegistryService>,
    types_registry::domain::local_client::TypesRegistryLocalClient,
) {
    let config = types_registry::config::TypesRegistryConfig::default();
    let repo = Arc::new(types_registry::infra::InMemoryGtsRepository::new(
        config.to_gts_config(),
    ));
    let service = Arc::new(types_registry::domain::TypesRegistryService::new(
        repo, config,
    ));
    let client =
        types_registry::domain::local_client::TypesRegistryLocalClient::new(Arc::clone(&service));
    (service, client)
}

/// A registry that accepts every schema and remembers what it was handed.
#[derive(Default)]
struct Recording {
    submitted: Mutex<Vec<String>>,
    /// The error the next registration answers with.
    failure: Mutex<Option<CanonicalError>>,
}

impl Recording {
    fn ids(&self) -> Vec<String> {
        self.submitted.lock().expect("submitted").clone()
    }

    fn fail_on_next(error: CanonicalError) -> Self {
        Self {
            failure: Mutex::new(Some(error)),
            ..Self::default()
        }
    }
}

#[async_trait]
impl TypesRegistryClient for Recording {
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
        let mut submitted = self.submitted.lock().expect("submitted");
        let failure = self.failure.lock().expect("failure").take();
        let Some(failure) = failure else {
            let ids: Vec<RegisterResult> = type_schemas
                .iter()
                .map(|document| {
                    // Record the canonical id: the document's `$id` carries
                    // the `gts://` URI form.
                    let id = document["$id"]
                        .as_str()
                        .unwrap_or_default()
                        .strip_prefix("gts://")
                        .unwrap_or_default()
                        .to_owned();
                    submitted.push(id.clone());
                    RegisterResult::Ok { gts_id: id }
                })
                .collect();
            return Ok(ids);
        };
        let results: Vec<RegisterResult> = type_schemas
            .iter()
            .map(|document| RegisterResult::Err {
                gts_id: document["$id"].as_str().map(str::to_owned),
                error: failure.clone(),
            })
            .collect();
        Ok(results)
    }

    async fn get_type_schema(&self, _type_id: &str) -> Result<GtsTypeSchema, CanonicalError> {
        Err(not_found("unimplemented"))
    }

    async fn get_type_schema_by_uuid(
        &self,
        _type_uuid: Uuid,
    ) -> Result<GtsTypeSchema, CanonicalError> {
        Err(not_found("unimplemented"))
    }

    async fn get_type_schemas(
        &self,
        type_ids: Vec<String>,
    ) -> HashMap<String, Result<GtsTypeSchema, CanonicalError>> {
        type_ids
            .into_iter()
            .map(|id| (id, Err(not_found("unimplemented"))))
            .collect()
    }

    async fn get_type_schemas_by_uuid(
        &self,
        type_uuids: Vec<Uuid>,
    ) -> HashMap<Uuid, Result<GtsTypeSchema, CanonicalError>> {
        type_uuids
            .into_iter()
            .map(|uuid| (uuid, Err(not_found("unimplemented"))))
            .collect()
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
        Err(not_found("unimplemented"))
    }

    async fn get_instance_by_uuid(&self, _uuid: Uuid) -> Result<GtsInstance, CanonicalError> {
        Err(not_found("unimplemented"))
    }

    async fn get_instances(
        &self,
        ids: Vec<String>,
    ) -> HashMap<String, Result<GtsInstance, CanonicalError>> {
        ids.into_iter()
            .map(|id| (id, Err(not_found("unimplemented"))))
            .collect()
    }

    async fn get_instances_by_uuid(
        &self,
        uuids: Vec<Uuid>,
    ) -> HashMap<Uuid, Result<GtsInstance, CanonicalError>> {
        uuids
            .into_iter()
            .map(|uuid| (uuid, Err(not_found("unimplemented"))))
            .collect()
    }

    async fn list_instances(
        &self,
        _query: InstanceQuery,
    ) -> Result<Vec<GtsInstance>, CanonicalError> {
        Ok(vec![])
    }
}

#[tokio::test]
async fn every_owned_type_schema_is_submitted() {
    let registry = Recording::default();
    provision(&registry).await.expect("provisioning");
    assert_eq!(registry.ids().len(), PROVISIONED_TYPES.len());
    for id in PROVISIONED_TYPES {
        assert!(registry.ids().contains(&id.to_owned()), "{id} missing");
    }
}

#[tokio::test]
async fn documents_are_root_object_schemas_ending_in_a_type_marker() {
    for document in documents() {
        let id = document["$id"].as_str().unwrap_or_default();
        assert!(id.starts_with("gts://"), "{id} is not a GTS URI");
        assert!(id.ends_with('~'), "{id} is not a type-schema id");
        assert_eq!(document["type"], "object");
        assert!(
            document["properties"]
                .as_object()
                .is_some_and(|p| !p.is_empty())
        );
    }
}

#[tokio::test]
async fn the_registry_admits_every_document() {
    let (service, registry) = real_registry();
    provision(&registry).await.expect("provisioning");
    service.switch_to_ready().expect("ready");
    for id in PROVISIONED_TYPES {
        let schema = registry
            .get_type_schema(id)
            .await
            .unwrap_or_else(|error| panic!("{id}: {error}"));
        assert_eq!(schema.type_id.as_ref(), id);
    }
}

#[tokio::test]
async fn provisioning_twice_is_accepted() {
    let (service, registry) = real_registry();
    provision(&registry).await.expect("first pass");
    service.switch_to_ready().expect("ready");
    provision(&registry)
        .await
        .expect("second pass is idempotent");
}

#[tokio::test]
async fn a_rejected_schema_fails_the_provisioning() {
    let registry = Recording::fail_on_next(invalid_gts_id("nope".to_owned()));
    let error = provision(&registry).await.expect_err("must fail");
    assert!(error.to_string().contains("type provisioning failed"));
}
