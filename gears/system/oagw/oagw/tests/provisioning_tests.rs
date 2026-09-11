//! GTS type-catalogue provisioning tests.
//!
//! Covers `cpt-cf-oagw-algo-type-catalog-provisioning`,
//! `cpt-cf-oagw-flow-type-provisioning` and `cpt-cf-oagw-dod-gts-type-catalog`:
//! the batch shape (7 base schemas, 2 protocol instances, 21 error instances,
//! no built-in plugin instance), parents-before-children ordering, per-entry
//! failure classification, idempotent re-run over identical content, the
//! fail-immediately behaviour on a catastrophic SDK error, and the
//! real-registry resolvability the gear-foundation feature accepts on: after
//! the ready commit, every provisioned entry resolves back byte-identical,
//! the 21 error identifiers resolve — the two management-conflict ones among
//! them — and an identical re-registration does not fail startup. A per-entry
//! refusal is asserted to reach the ERROR log with its identifier.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::missing_panics_doc)]

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use oagw::gts::catalog::catalog_entities;
use oagw::gts::provisioning::{CatalogProvisioned, ProvisioningError, provision};
use oagw::gts::{ERR_CORS_METHOD_NOT_ALLOWED, ERR_CORS_ORIGIN_NOT_ALLOWED};
use oagw::{
    AUTH_PLUGIN_TYPE, ERR_ALIAS_CONFLICT, ERR_AUTH_FAILED, ERR_CIRCUIT_BREAKER_OPEN,
    ERR_DOWNSTREAM_ERROR, ERR_INVALID_TARGET_HOST, ERR_LINK_UNAVAILABLE, ERR_MATCH_CONFLICT,
    ERR_MISSING_TARGET_HOST, ERR_PAYLOAD_TOO_LARGE, ERR_PLUGIN_IN_USE, ERR_PLUGIN_NOT_FOUND,
    ERR_PROTOCOL_ERROR, ERR_RATE_LIMIT_EXCEEDED, ERR_ROUTE_NOT_FOUND, ERR_SECRET_NOT_FOUND,
    ERR_STREAM_ABORTED, ERR_TIMEOUT_CONNECTION, ERR_TIMEOUT_IDLE, ERR_TIMEOUT_REQUEST,
    ERR_UNKNOWN_TARGET_HOST, ERR_VALIDATION, GUARD_PLUGIN_TYPE, TRANSFORM_PLUGIN_TYPE,
};
use serde_json::Value;
use toolkit_canonical_errors::CanonicalError;
use types_registry::config::TypesRegistryConfig;
use types_registry::domain::local_client::TypesRegistryLocalClient;
use types_registry::domain::TypesRegistryService;
use types_registry::infra::InMemoryGtsRepository;
use types_registry_sdk::testing::{make_test_instance, make_test_type_schema};
use types_registry_sdk::{
    GtsInstance, GtsTypeSchema, InstanceQuery, RegisterResult, TypeSchemaQuery, TypesRegistryClient,
};
use uuid::Uuid;

/// How the fake should answer the next `register` call.
#[derive(Debug, Default)]
enum Behaviour {
    /// Every entry is accepted.
    #[default]
    AcceptAll,
    /// A catastrophic backend failure — the call itself errors.
    Catastrophic(&'static str),
    /// The named GTS identifiers are refused; everything else is accepted.
    Refuse(Vec<&'static str>),
}

/// A hand-rolled [`TypesRegistryClient`] that records what provisioning
/// submitted and can be configured to refuse chosen identifiers.
#[derive(Debug, Default)]
struct RecordingRegistryClient {
    behaviour: Mutex<Behaviour>,
    submitted: Mutex<Vec<Value>>,
    register_calls: Mutex<usize>,
    /// Entities the registry already holds, so the identical-content path is
    /// exercised.
    pre_held: Vec<String>,
    read_backs: Mutex<Vec<String>>,
}

impl RecordingRegistryClient {
    /// A client that accepts everything.
    fn accepting() -> Arc<Self> {
        Arc::default()
    }

    /// A client whose `register` fails catastrophically.
    fn catastrophic(reason: &'static str) -> Arc<Self> {
        Arc::new(Self {
            behaviour: Mutex::new(Behaviour::Catastrophic(reason)),
            ..Self::default()
        })
    }

    /// A client that refuses the named identifiers and accepts the rest.
    fn refusing(refused: Vec<&'static str>) -> Arc<Self> {
        Arc::new(Self {
            behaviour: Mutex::new(Behaviour::Refuse(refused)),
            ..Self::default()
        })
    }

    /// A client that already holds the named entries, answering read-backs for
    /// them from a store seeded with identical content.
    fn pre_seeded(ids: Vec<String>) -> Arc<Self> {
        Arc::new(Self {
            pre_held: ids,
            ..Self::default()
        })
    }

    /// The identifiers submitted, in submission order.
    fn submitted_ids(&self) -> Vec<String> {
        self.submitted
            .lock()
            .expect("submitted lock")
            .iter()
            .map(entity_id)
            .collect()
    }

    /// The number of `register` calls.
    fn register_calls(&self) -> usize {
        *self.register_calls.lock().expect("register call lock")
    }

    /// The identifiers a read-back was attempted for.
    fn read_backs(&self) -> Vec<String> {
        self.read_backs.lock().expect("read-back lock").clone()
    }
}

/// The GTS identifier carried by an entity, from `$id` (type schema) or `id`
/// (instance). The registry strips the `gts://` scheme off `$id`; so does this
/// helper, so assertions speak in GTS identifiers.
fn entity_id(entity: &Value) -> String {
    let raw = entity
        .get("$id")
        .or_else(|| entity.get("id"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    raw.strip_prefix("gts://").unwrap_or(raw).to_owned()
}

/// Builds the `GtsTypeSchema` a read-back answers with.
fn held_type_schema(type_id: &str) -> GtsTypeSchema {
    make_test_type_schema(type_id)
}

/// Builds the `GtsInstance` a read-back answers with.
fn held_instance(id: &str, content: Value) -> GtsInstance {
    make_test_instance(id, content)
}

#[async_trait]
impl TypesRegistryClient for RecordingRegistryClient {
    async fn register(&self, entities: Vec<Value>) -> Result<Vec<RegisterResult>, CanonicalError> {
        *self.register_calls.lock().expect("register call lock") += 1;

        match &*self.behaviour.lock().expect("behaviour lock") {
            Behaviour::Catastrophic(reason) => {
                Err(CanonicalError::internal(String::from(*reason)).create())
            }
            Behaviour::AcceptAll => {
                let ids: Vec<String> = entities.iter().map(entity_id).collect();
                self.submitted
                    .lock()
                    .expect("submitted lock")
                    .extend(entities);
                Ok(ids
                    .into_iter()
                    .map(|gts_id| RegisterResult::Ok { gts_id })
                    .collect())
            }
            Behaviour::Refuse(refused) => {
                let ids: Vec<String> = entities.iter().map(entity_id).collect();
                self.submitted
                    .lock()
                    .expect("submitted lock")
                    .extend(entities);
                Ok(ids
                    .into_iter()
                    .map(|gts_id| {
                        if refused.contains(&gts_id.as_str()) {
                            RegisterResult::Err {
                                gts_id: Some(gts_id),
                                error: CanonicalError::internal("duplicate content").create(),
                            }
                        } else {
                            RegisterResult::Ok { gts_id }
                        }
                    })
                    .collect())
            }
        }
    }

    async fn register_type_schemas(
        &self,
        _type_schemas: Vec<Value>,
    ) -> Result<Vec<RegisterResult>, CanonicalError> {
        Ok(vec![])
    }

    async fn get_type_schema(&self, type_id: &str) -> Result<GtsTypeSchema, CanonicalError> {
        self.read_backs
            .lock()
            .expect("read-back lock")
            .push(type_id.to_owned());
        if self.pre_held.iter().any(|held| held == type_id) {
            Ok(held_type_schema(type_id))
        } else {
            Err(types_registry_sdk::testing::not_found(type_id))
        }
    }

    async fn get_type_schema_by_uuid(
        &self,
        type_uuid: Uuid,
    ) -> Result<GtsTypeSchema, CanonicalError> {
        Err(types_registry_sdk::testing::not_found(
            type_uuid.to_string(),
        ))
    }

    async fn get_type_schemas(
        &self,
        type_ids: Vec<String>,
    ) -> HashMap<String, Result<GtsTypeSchema, CanonicalError>> {
        type_ids
            .into_iter()
            .map(|id| (id.clone(), Err(types_registry_sdk::testing::not_found(&id))))
            .collect()
    }

    async fn get_type_schemas_by_uuid(
        &self,
        type_uuids: Vec<Uuid>,
    ) -> HashMap<Uuid, Result<GtsTypeSchema, CanonicalError>> {
        type_uuids
            .into_iter()
            .map(|uuid| {
                let err = types_registry_sdk::testing::not_found(uuid.to_string());
                (uuid, Err(err))
            })
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
        _instances: Vec<Value>,
    ) -> Result<Vec<RegisterResult>, CanonicalError> {
        Ok(vec![])
    }

    async fn get_instance(&self, id: &str) -> Result<GtsInstance, CanonicalError> {
        self.read_backs
            .lock()
            .expect("read-back lock")
            .push(id.to_owned());
        if self.pre_held.iter().any(|held| held == id) {
            Ok(held_instance(id, serde_json::json!({ "id": id })))
        } else {
            Err(types_registry_sdk::testing::not_found(id))
        }
    }

    async fn get_instance_by_uuid(&self, uuid: Uuid) -> Result<GtsInstance, CanonicalError> {
        Err(types_registry_sdk::testing::not_found(uuid.to_string()))
    }

    async fn get_instances(
        &self,
        ids: Vec<String>,
    ) -> HashMap<String, Result<GtsInstance, CanonicalError>> {
        ids.into_iter()
            .map(|id| (id.clone(), Err(types_registry_sdk::testing::not_found(&id))))
            .collect()
    }

    async fn get_instances_by_uuid(
        &self,
        uuids: Vec<Uuid>,
    ) -> HashMap<Uuid, Result<GtsInstance, CanonicalError>> {
        uuids
            .into_iter()
            .map(|uuid| {
                let err = types_registry_sdk::testing::not_found(uuid.to_string());
                (uuid, Err(err))
            })
            .collect()
    }

    async fn list_instances(
        &self,
        _query: InstanceQuery,
    ) -> Result<Vec<GtsInstance>, CanonicalError> {
        Ok(vec![])
    }
}

/// The batch, without its duplicate identifiers, in submission order.
fn batch() -> Vec<Value> {
    catalog_entities().expect("catalogue assembles from its frozen inputs")
}

const BASE_TYPES: [&str; 7] = [
    "gts.cf.core.oagw.upstream.v1~",
    "gts.cf.core.oagw.route.v1~",
    "gts.cf.core.oagw.protocol.v1~",
    "gts.cf.core.oagw.auth_plugin.v1~",
    "gts.cf.core.oagw.guard_plugin.v1~",
    "gts.cf.core.oagw.transform_plugin.v1~",
    "gts.cf.core.errors.err.v1~",
];

const PROTOCOL_INSTANCES: [&str; 2] = [
    "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
    "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1",
];

#[tokio::test]
async fn happy_path_submits_every_entry_and_reports_success() {
    let client = RecordingRegistryClient::accepting();

    let outcome: CatalogProvisioned = provision(client.as_ref())
        .await
        .expect("provisioning succeeds");

    assert_eq!(
        outcome.total,
        42,
        "7 base types + 2 protocol + 21 error + 12 plugin"
    );
    assert_eq!(outcome.succeeded, 42);
    assert_eq!(client.register_calls(), 1, "exactly one batch register");
    assert_eq!(client.submitted_ids().len(), 42);
}

/// The parent type id of a GTS identifier: everything up to and including the
/// last `~`. `None` for a base type itself.
fn chain_parent(gts_id: &str) -> Option<String> {
    gts_id
        .split_once('~')
        .map(|(prefix, _)| format!("{prefix}~"))
}

#[tokio::test]
async fn parents_precede_their_children_in_the_batch() {
    let entities = batch();

    let mut seen: Vec<String> = Vec::new();
    for entity in &entities {
        let gts_id = entity_id(entity);
        if let Some(parent) = chain_parent(&gts_id)
            && parent != gts_id
        {
            assert!(
                seen.contains(&parent),
                "{gts_id} appears before its parent {parent}"
            );
        }
        seen.push(gts_id);
    }

    for base in BASE_TYPES {
        let index = seen
            .iter()
            .position(|id| id == base)
            .expect("base type present");
        assert!(index < 7, "{base} must sit in the parents-first prefix");
    }
}

#[tokio::test]
async fn the_batch_carries_the_declared_catalogue_shape() {
    let entities = batch();
    let ids = entities.iter().map(entity_id).collect::<Vec<_>>();

    let error_type = "gts.cf.core.errors.err.v1~";
    let base_types = ids.iter().filter(|id| id.ends_with('~')).count();
    let protocol = ids
        .iter()
        .filter(|id| PROTOCOL_INSTANCES.contains(&id.as_str()))
        .count();
    let errors = ids
        .iter()
        .filter(|id| id.starts_with(error_type) && *id != error_type)
        .count();

    assert_eq!(base_types, 7, "7 base type schemas");
    assert_eq!(protocol, 2, "2 protocol instances");
    assert_eq!(errors, 21, "21 distinct error instances");
    assert_eq!(ids.len(), 42, "the 30 foundation rows plus 12 plugin rows");

    for base in BASE_TYPES {
        assert!(ids.contains(&base.to_owned()), "{base} must be present");
    }
    for instance in PROTOCOL_INSTANCES {
        assert!(
            ids.contains(&instance.to_owned()),
            "{instance} must be present"
        );
    }
}

#[tokio::test]
async fn the_catalogue_carries_the_twelve_plugin_instances() {
    // `cpt-cf-oagw-dod-builtin-catalogue`: the plugin-system feature registers
    // all twelve plugin identifiers of the built-in and catalog-only catalogue
    // in the types-registry, backed and catalog-only alike.
    let entities = batch();
    let ids = entities.iter().map(entity_id).collect::<Vec<_>>();

    let owned = |plugin_type: &str| ids.iter().filter(|id| id.starts_with(plugin_type)).count();
    assert_eq!(
        owned(AUTH_PLUGIN_TYPE),
        7,
        "the base schema plus 4 backed and 2 catalog-only auth ids"
    );
    assert_eq!(
        owned(GUARD_PLUGIN_TYPE),
        4,
        "the base schema plus 1 backed and 2 catalog-only guard ids"
    );
    assert_eq!(
        owned(TRANSFORM_PLUGIN_TYPE),
        4,
        "the base schema plus 1 backed and 2 catalog-only transform ids"
    );
    for identifier in oagw::gts::plugin_catalog::all() {
        assert!(ids.contains(&identifier.to_owned()), "{identifier} present");
    }
}

#[tokio::test]
async fn base_type_schemas_are_json_schema_objects_with_a_gts_id() {
    let entities = batch();
    for entity in entities.iter().take(7) {
        assert!(
            entity
                .get("$id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .ends_with('~'),
            "base type carries `$id` with the `gts://` uri: {entity}"
        );
        assert_eq!(
            entity["type"], "object",
            "base type declares `type: object`"
        );
        assert!(
            entity.get("$schema").is_some(),
            "base type declares its JSON Schema meta-schema"
        );
    }
}

#[tokio::test]
async fn error_instances_carry_their_catalogue_row() {
    const ERROR_TYPE: &str = "gts.cf.core.errors.err.v1~";
    let entities = batch();
    let errors = entities
        .iter()
        .filter(|e| {
            let id = entity_id(e);
            id.starts_with(ERROR_TYPE) && id != ERROR_TYPE
        })
        .collect::<Vec<_>>();

    assert_eq!(errors.len(), 21, "21 error instance rows");
    for error in errors {
        assert!(error.get("title").and_then(Value::as_str).is_some());
        assert!(error.get("http_status").and_then(Value::as_u64).is_some());
        assert!(error.get("retriable").and_then(Value::as_bool).is_some());
        assert!(
            error.get("$id").is_none(),
            "instances carry `id`, not `$id`"
        );
    }
}

#[tokio::test]
async fn one_per_item_failure_fails_the_phase_but_submits_the_rest() {
    let refused = "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1";
    let client = RecordingRegistryClient::refusing(vec![refused]);

    let error: ProvisioningError = provision(client.as_ref())
        .await
        .expect_err("one refusal fails the phase");

    match error {
        ProvisioningError::EntriesFailed {
            failed,
            total,
            identifiers,
        } => {
            assert_eq!(failed, 1);
            assert_eq!(total, 42);
            assert_eq!(identifiers, refused);
        }
        other => panic!("expected EntriesFailed, got {other:?}"),
    }

    assert_eq!(client.register_calls(), 1);
    assert_eq!(
        client.submitted_ids().len(),
        42,
        "every entry is still submitted"
    );
    assert_eq!(client.read_backs(), vec![refused.to_owned()]);
}

#[tokio::test]
async fn every_refused_identifier_is_recorded_and_read_back() {
    let client = RecordingRegistryClient::refusing(vec![
        "gts.cf.core.oagw.upstream.v1~",
        "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1",
    ]);

    let error: ProvisioningError = provision(client.as_ref())
        .await
        .expect_err("two refusals fail the phase");

    let ProvisioningError::EntriesFailed {
        failed,
        identifiers,
        ..
    } = error
    else {
        panic!("expected EntriesFailed");
    };
    assert_eq!(failed, 2);
    assert!(
        identifiers.contains("gts.cf.core.oagw.upstream.v1~")
            && identifiers.contains("gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1"),
        "both failing identifiers are named: {identifiers}"
    );
    assert_eq!(client.read_backs().len(), 2);
}

#[tokio::test]
async fn a_catastrophic_sdk_failure_fails_immediately_without_a_retry() {
    let client = RecordingRegistryClient::catastrophic("backend unavailable");

    let error: ProvisioningError = provision(client.as_ref())
        .await
        .expect_err("catastrophic failure fails the phase");

    match error {
        ProvisioningError::Registry(message) => {
            assert!(!message.is_empty(), "the SDK failure is carried: {message}");
        }
        other => panic!("expected Registry, got {other:?}"),
    }

    assert_eq!(client.register_calls(), 1, "no retry, no partial re-issue");
    assert!(client.submitted_ids().is_empty());
    assert!(client.read_backs().is_empty());
}

#[tokio::test]
async fn an_identical_content_rerun_succeeds_against_a_seeded_registry() {
    let entities = batch();
    let held = entities.iter().map(entity_id).collect::<Vec<_>>();
    let client = RecordingRegistryClient::pre_seeded(held);

    let outcome = provision(client.as_ref()).await.expect("re-run succeeds");
    assert_eq!(outcome.succeeded, 42);
}

#[tokio::test]
async fn the_provisioning_error_names_every_failing_identifier() {
    let client = RecordingRegistryClient::refusing(vec![
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1",
        "gts.cf.core.errors.err.v1~cf.oagw.alias.conflict.v1",
        "gts.cf.core.oagw.route.v1~",
    ]);

    let error: ProvisioningError = provision(client.as_ref())
        .await
        .expect_err("three refusals fail the phase");

    let rendered = error.to_string();
    assert!(
        rendered.contains("3 of 42"),
        "the error reports the failure ratio: {rendered}"
    );
    assert!(rendered.contains("cf.oagw.route.not_found.v1"));
    assert!(rendered.contains("cf.oagw.alias.conflict.v1"));
    assert!(rendered.contains("cf.core.oagw.route.v1~"));
}

#[tokio::test]
async fn provisioning_success_is_reproducible_across_runs() {
    let first = RecordingRegistryClient::accepting();
    let second = RecordingRegistryClient::accepting();

    let a = provision(first.as_ref()).await.expect("first run");
    let b = provision(second.as_ref()).await.expect("second run");

    assert_eq!(a, b, "the same frozen inputs produce the same outcome");
    assert_eq!(first.submitted_ids(), second.submitted_ids());
}

/// The 21 distinct error identifiers the 22 `ErrorKind` variants map onto:
/// 19 carry the DESIGN §3.3 catalogue rows, `RouteError` and `ValidationError`
/// share one identifier, and the two §1.5-added management-conflict variants
/// take the last two. Ordered as the catalogue restates it.
const ERROR_IDENTIFIERS: [&str; 21] = [
    ERR_VALIDATION,
    ERR_MISSING_TARGET_HOST,
    ERR_INVALID_TARGET_HOST,
    ERR_UNKNOWN_TARGET_HOST,
    ERR_AUTH_FAILED,
    ERR_ROUTE_NOT_FOUND,
    ERR_PLUGIN_IN_USE,
    ERR_ALIAS_CONFLICT,
    ERR_MATCH_CONFLICT,
    ERR_PAYLOAD_TOO_LARGE,
    ERR_RATE_LIMIT_EXCEEDED,
    ERR_SECRET_NOT_FOUND,
    ERR_PROTOCOL_ERROR,
    ERR_DOWNSTREAM_ERROR,
    ERR_STREAM_ABORTED,
    ERR_LINK_UNAVAILABLE,
    ERR_CIRCUIT_BREAKER_OPEN,
    ERR_PLUGIN_NOT_FOUND,
    ERR_TIMEOUT_CONNECTION,
    ERR_TIMEOUT_REQUEST,
    ERR_TIMEOUT_IDLE,
];

/// A real, in-process types-registry client over the in-memory repository —
/// the client shape the `ClientHub` supplies at runtime — together with the
/// service handle the types-registry gear drives through its ready commit.
fn local_registry() -> (TypesRegistryLocalClient, Arc<TypesRegistryService>) {
    let repo = Arc::new(InMemoryGtsRepository::new(
        TypesRegistryConfig::default().to_gts_config(),
    ));
    let service = Arc::new(TypesRegistryService::new(
        repo,
        TypesRegistryConfig::default(),
    ));
    (TypesRegistryLocalClient::new(Arc::clone(&service)), service)
}

/// Resolves one provisioned entity back through the registry, by kind, and
/// hands the stored content back for the byte-identical comparison.
async fn resolved_content(client: &TypesRegistryLocalClient, id: &str) -> Value {
    if id.ends_with('~') {
        client
            .get_type_schema(id)
            .await
            .unwrap_or_else(|e| panic!("{id} resolves as a base type schema: {e}"))
            .raw_schema
    } else {
        client
            .get_instance(id)
            .await
            .unwrap_or_else(|e| panic!("{id} resolves as an instance: {e}"))
            .object
    }
}

/// After the ready commit the registry holds, every provisioned entry
/// resolves back through it with the content that was submitted.
#[tokio::test]
async fn every_provisioned_entry_resolves_back_through_a_real_registry() {
    let (client, service) = local_registry();

    let outcome = provision(&client)
        .await
        .expect("the catalogue provisions against a real registry");
    assert_eq!(outcome.succeeded, outcome.total, "every entry is accepted");

    // The startup order the runtime drives: the types-registry gear commits
    // the configuration phase to ready in its own post_init, which validates
    // everything the batch held.
    service
        .switch_to_ready()
        .expect("the provisioned catalogue validates in full");
    assert!(service.is_ready(), "the registry reports readiness");

    let entities = batch();
    assert_eq!(entities.len(), outcome.total);
    for entity in &entities {
        let id = entity_id(entity);
        let stored = resolved_content(&client, &id).await;
        assert_eq!(stored, *entity, "{id} round-trips byte-identical");
    }
}

/// The 21 error identifiers resolve as instances after provisioning, the two
/// §1.5-added management-conflict identifiers among them, and the two bare
/// CORS problem types stay outside the catalogue.
#[tokio::test]
async fn all_twenty_one_error_identifiers_resolve_after_provisioning() {
    let (client, service) = local_registry();
    provision(&client)
        .await
        .expect("the catalogue provisions");
    service
        .switch_to_ready()
        .expect("the provisioned catalogue validates in full");

    assert_eq!(
        ERROR_IDENTIFIERS.len(),
        21,
        "22 variants over 21 identifiers"
    );
    for identifier in ERROR_IDENTIFIERS {
        client
            .get_instance(identifier)
            .await
            .unwrap_or_else(|e| panic!("{identifier} resolves: {e}"));
    }
    assert!(
        ERROR_IDENTIFIERS.contains(&ERR_ALIAS_CONFLICT) && ERROR_IDENTIFIERS.contains(&ERR_MATCH_CONFLICT),
        "the two §1.5-added management-conflict identifiers are among them"
    );

    let ids: HashSet<String> = batch().iter().map(entity_id).collect();
    for cors in [ERR_CORS_ORIGIN_NOT_ALLOWED, ERR_CORS_METHOD_NOT_ALLOWED] {
        assert!(
            !ids.contains(cors),
            "{cors} is a bare problem type, not a catalogue row"
        );
    }
}

/// Re-registering an entry with byte-identical content does not fail startup:
/// the second pass over a registry that already holds the catalogue answers
/// accepted for every entry, not a conflict.
#[tokio::test]
async fn an_identical_rerun_succeeds_against_a_real_registry() {
    let (client, service) = local_registry();

    let first = provision(&client)
        .await
        .expect("the first startup provisions the catalogue");
    service
        .switch_to_ready()
        .expect("the provisioned catalogue validates in full");

    let second = provision(&client)
        .await
        .expect("an identical re-registration does not fail startup");
    assert_eq!(second, first);
    assert_eq!(second.succeeded, second.total);
}

#[tokio::test]
#[tracing_test::traced_test]
async fn a_per_entry_refusal_is_logged_with_its_identifier() {
    let refused = "gts.cf.core.errors.err.v1~cf.oagw.alias.conflict.v1";
    let client = RecordingRegistryClient::refusing(vec![refused]);

    let error = provision(client.as_ref())
        .await
        .expect_err("one refusal fails the phase");
    assert!(
        matches!(error, ProvisioningError::EntriesFailed { .. }),
        "the phase fails with the per-entry error: {error}"
    );

    assert!(
        logs_contain(refused),
        "the failing identifier reaches the ERROR log"
    );
    assert!(
        logs_contain("readiness stays withheld"),
        "the withheld readiness reaches the ERROR log"
    );
}
