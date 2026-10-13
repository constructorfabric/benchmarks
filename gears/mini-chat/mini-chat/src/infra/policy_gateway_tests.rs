#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Unit tests of the model policy gateway decision logic and of the
//! types-registry + `ClientHub` plugin locator.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use mini_chat_sdk::{
    MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginError,
    MiniChatModelPolicyPluginSpecV1, PolicySnapshot, PublishError, UsageEvent, UserLimits,
};
use serde_json::Value;
use time::OffsetDateTime;
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::gts::PluginV1;
use toolkit_canonical_errors::CanonicalError;
use types_registry_sdk::{
    GtsInstance, GtsTypeSchema, InstanceQuery, RegisterResult, TypeSchemaQuery, TypesRegistryClient,
};
use uuid::Uuid;

use super::{
    HubPluginLocator, ModelPolicyGateway, ModelPolicyPluginLocator, PluginLocator,
    PluginLookupError,
};
use crate::infra::plugins::static_model_policy::{
    StaticModelPolicyConfig, StaticModelPolicyService,
};

type PolicyClient = dyn MiniChatModelPolicyPluginClientV1;

const INSTANCE_ID: &str = "instance-1";

// ── Fakes ────────────────────────────────────────────────────────────────

/// Locator with a scripted lookup result and client; counts lookups.
struct FakeLocator {
    lookup: Mutex<Result<String, PluginLookupError>>,
    client: Mutex<Option<Arc<PolicyClient>>>,
    lookups: AtomicUsize,
}

impl FakeLocator {
    fn new(
        lookup: Result<String, PluginLookupError>,
        client: Option<Arc<PolicyClient>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            lookup: Mutex::new(lookup),
            client: Mutex::new(client),
            lookups: AtomicUsize::new(0),
        })
    }

    fn set_client(&self, client: Option<Arc<PolicyClient>>) {
        *self.client.lock().unwrap() = client;
    }

    fn lookups(&self) -> usize {
        self.lookups.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl PluginLocator<PolicyClient> for FakeLocator {
    async fn find_instance(&self) -> Result<String, PluginLookupError> {
        self.lookups.fetch_add(1, Ordering::SeqCst);
        self.lookup.lock().unwrap().clone()
    }

    fn client(&self, instance_id: &str) -> Option<Arc<PolicyClient>> {
        assert_eq!(instance_id, INSTANCE_ID);
        self.client.lock().unwrap().clone()
    }
}

/// Static plugin wrapper counting snapshot reads.
struct CountingPlugin {
    inner: StaticModelPolicyService,
    snapshots: AtomicUsize,
}

impl CountingPlugin {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: StaticModelPolicyService::new(StaticModelPolicyConfig::default()),
            snapshots: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for CountingPlugin {
    async fn get_current_policy_version(
        &self,
        user_id: Uuid,
    ) -> Result<u64, MiniChatModelPolicyPluginError> {
        self.inner.get_current_policy_version(user_id).await
    }

    async fn get_policy_snapshot(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError> {
        self.snapshots.fetch_add(1, Ordering::SeqCst);
        self.inner
            .get_policy_snapshot(user_id, policy_version)
            .await
    }

    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, MiniChatModelPolicyPluginError> {
        self.inner.get_user_limits(user_id, policy_version).await
    }

    async fn publish_usage(&self, _payload: UsageEvent) -> Result<(), PublishError> {
        Err(PublishError::Permanent(
            "rejected by test plugin".to_owned(),
        ))
    }
}

/// Registry fake answering `list_instances` from a fixed list.
struct FakeRegistry {
    instances: Result<Vec<GtsInstance>, String>,
}

fn unused() -> CanonicalError {
    CanonicalError::internal("not used by the plugin locator").create()
}

#[async_trait]
impl TypesRegistryClient for FakeRegistry {
    async fn register(&self, _entities: Vec<Value>) -> Result<Vec<RegisterResult>, CanonicalError> {
        Err(unused())
    }

    async fn register_type_schemas(
        &self,
        _type_schemas: Vec<Value>,
    ) -> Result<Vec<RegisterResult>, CanonicalError> {
        Err(unused())
    }

    async fn get_type_schema(&self, _type_id: &str) -> Result<GtsTypeSchema, CanonicalError> {
        Err(unused())
    }

    async fn get_type_schema_by_uuid(
        &self,
        _type_uuid: Uuid,
    ) -> Result<GtsTypeSchema, CanonicalError> {
        Err(unused())
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
        Err(unused())
    }

    async fn register_instances(
        &self,
        _instances: Vec<Value>,
    ) -> Result<Vec<RegisterResult>, CanonicalError> {
        Err(unused())
    }

    async fn get_instance(&self, _id: &str) -> Result<GtsInstance, CanonicalError> {
        Err(unused())
    }

    async fn get_instance_by_uuid(&self, _uuid: Uuid) -> Result<GtsInstance, CanonicalError> {
        Err(unused())
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
        self.instances
            .clone()
            .map_err(|msg| CanonicalError::internal(msg).create())
    }
}

fn type_schema(type_id: &str) -> Arc<GtsTypeSchema> {
    let parent = GtsTypeSchema::derive_parent_type_id(type_id).map(|p| type_schema(p.as_ref()));
    Arc::new(
        GtsTypeSchema::try_new(
            types_registry_sdk::GtsTypeId::new(type_id),
            serde_json::json!({}),
            None,
            parent,
        )
        .expect("synthetic type schema"),
    )
}

/// A registered model-policy plugin instance of `vendor` with `priority`.
fn policy_instance(segment: &str, vendor: &str, priority: i16) -> GtsInstance {
    let (id, object) =
        PluginV1::<MiniChatModelPolicyPluginSpecV1>::build_registration(segment, vendor, priority)
            .expect("registration payload");
    let schema = type_schema(<MiniChatModelPolicyPluginSpecV1 as gts::GtsSchema>::TYPE_ID);
    GtsInstance::try_new(id, object, None, schema).expect("synthetic instance")
}

fn hub_with_registry(instances: Result<Vec<GtsInstance>, String>) -> Arc<ClientHub> {
    let hub = Arc::new(ClientHub::new());
    let registry: Arc<dyn TypesRegistryClient> = Arc::new(FakeRegistry { instances });
    hub.register::<dyn TypesRegistryClient>(registry);
    hub
}

fn gateway(locator: &Arc<FakeLocator>) -> ModelPolicyGateway {
    let locator: Arc<ModelPolicyPluginLocator> = Arc::clone(locator) as _;
    ModelPolicyGateway::with_locator(locator)
}

fn user() -> Uuid {
    Uuid::from_u128(7)
}

fn usage_event() -> UsageEvent {
    let tenant_id = Uuid::from_u128(1);
    let turn_id = Uuid::from_u128(2);
    let request_id = Uuid::from_u128(3);
    UsageEvent {
        tenant_id,
        user_id: Some(user()),
        chat_id: None,
        turn_id: Some(turn_id),
        request_id,
        effective_model: "m".to_owned(),
        selected_model: "m".to_owned(),
        terminal_state: "completed".to_owned(),
        billing_outcome: "completed".to_owned(),
        usage: None,
        actual_credits_micro: 1,
        settlement_method: "actual".to_owned(),
        policy_version_applied: 1,
        web_search_calls: 0,
        code_interpreter_calls: 0,
        file_search_calls: 0,
        timestamp: OffsetDateTime::UNIX_EPOCH,
        requester_type: "user".to_owned(),
        dedupe_key: mini_chat_sdk::dedupe_key_for_turn(tenant_id, turn_id, request_id),
        system_task_type: None,
    }
}

// ── ModelPolicyGateway ───────────────────────────────────────────────────

#[tokio::test]
async fn gateway_forwards_every_call_and_caches_only_the_instance_id() {
    let plugin = CountingPlugin::new();
    let locator = FakeLocator::new(Ok(INSTANCE_ID.to_owned()), Some(plugin.clone() as _));
    let gw = gateway(&locator);

    for _ in 0..3 {
        let snapshot = gw.get_policy_snapshot(user(), 1).await.unwrap();
        assert_eq!(snapshot.policy_version, 1);
    }
    assert_eq!(gw.get_current_policy_version(user()).await.unwrap(), 1);
    let limits = gw.get_user_limits(user(), 1).await.unwrap();

    assert_eq!(limits.user_id, user());
    assert_eq!(
        plugin.snapshots.load(Ordering::SeqCst),
        3,
        "no snapshot caching"
    );
    assert_eq!(locator.lookups(), 1, "the found instance id is cached");
}

#[tokio::test]
async fn gateway_passes_plugin_errors_through() {
    let locator = FakeLocator::new(Ok(INSTANCE_ID.to_owned()), Some(CountingPlugin::new() as _));
    let gw = gateway(&locator);

    let err = gw.get_policy_snapshot(user(), 2).await.unwrap_err();
    assert_eq!(
        err,
        MiniChatModelPolicyPluginError::VersionNotFound { policy_version: 2 }
    );

    let err = gw.publish_usage(usage_event()).await.unwrap_err();
    assert!(matches!(err, PublishError::Permanent(_)));
}

#[tokio::test]
async fn gateway_not_registered_is_transient_and_not_cached() {
    let locator = FakeLocator::new(
        Err(PluginLookupError::NotRegistered {
            plugin: "model policy",
            vendor: "acme".to_owned(),
        }),
        None,
    );
    let gw = gateway(&locator);

    for _ in 0..2 {
        let err = gw.get_current_policy_version(user()).await.unwrap_err();
        assert!(err.is_transient());
        assert!(err.to_string().contains("vendor 'acme'"), "{err}");
    }
    assert_eq!(locator.lookups(), 2);
}

#[tokio::test]
async fn gateway_registry_unavailable_is_transient() {
    let locator = FakeLocator::new(
        Err(PluginLookupError::Unavailable("registry down".to_owned())),
        None,
    );
    let gw = gateway(&locator);

    let err = gw.get_user_limits(user(), 1).await.unwrap_err();

    assert_eq!(
        err,
        MiniChatModelPolicyPluginError::Transient("registry down".to_owned())
    );
}

#[tokio::test]
async fn gateway_invalid_instance_is_internal() {
    let locator = FakeLocator::new(
        Err(PluginLookupError::InvalidInstance(
            "bad instance".to_owned(),
        )),
        None,
    );
    let gw = gateway(&locator);

    let err = gw.get_policy_snapshot(user(), 1).await.unwrap_err();

    assert_eq!(
        err,
        MiniChatModelPolicyPluginError::Internal("bad instance".to_owned())
    );
}

#[tokio::test]
async fn gateway_missing_client_is_transient_and_resets_cached_instance() {
    let locator = FakeLocator::new(Ok(INSTANCE_ID.to_owned()), None);
    let gw = gateway(&locator);

    let err = gw.get_current_policy_version(user()).await.unwrap_err();
    assert!(err.is_transient());
    assert!(err.to_string().contains(INSTANCE_ID), "{err}");

    locator.set_client(Some(CountingPlugin::new() as _));
    assert_eq!(gw.get_current_policy_version(user()).await.unwrap(), 1);
    assert_eq!(locator.lookups(), 2, "the instance is looked up again");
}

#[tokio::test]
async fn gateway_publish_usage_resolution_failure_is_transient() {
    let locator = FakeLocator::new(
        Err(PluginLookupError::InvalidInstance(
            "bad instance".to_owned(),
        )),
        None,
    );
    let gw = gateway(&locator);

    let err = gw.publish_usage(usage_event()).await.unwrap_err();

    assert!(err.is_transient());
}

// ── HubPluginLocator ─────────────────────────────────────────────────────

fn hub_locator(
    hub: Arc<ClientHub>,
) -> HubPluginLocator<MiniChatModelPolicyPluginSpecV1, PolicyClient> {
    HubPluginLocator::new(hub, "acme", "model policy")
}

#[tokio::test]
async fn locator_without_registry_client_is_unavailable() {
    let locator = hub_locator(Arc::new(ClientHub::new()));

    let err = locator.find_instance().await.unwrap_err();

    assert!(matches!(err, PluginLookupError::Unavailable(_)), "{err}");
}

#[tokio::test]
async fn locator_registry_failure_is_unavailable() {
    let locator = hub_locator(hub_with_registry(Err("boom".to_owned())));

    let err = locator.find_instance().await.unwrap_err();

    assert!(matches!(err, PluginLookupError::Unavailable(_)), "{err}");
}

#[tokio::test]
async fn locator_without_vendor_instance_is_not_registered() {
    let instances = vec![policy_instance("cf.core.mini_chat.other.v1", "other", 1)];
    let locator = hub_locator(hub_with_registry(Ok(instances)));

    let err = locator.find_instance().await.unwrap_err();

    assert_eq!(
        err,
        PluginLookupError::NotRegistered {
            plugin: "model policy",
            vendor: "acme".to_owned()
        }
    );
}

#[tokio::test]
async fn locator_selects_lowest_priority_instance_of_vendor() {
    let preferred = policy_instance("cf.core.mini_chat.preferred.v1", "acme", 10);
    let instances = vec![
        policy_instance("cf.core.mini_chat.fallback.v1", "acme", 100),
        preferred.clone(),
        policy_instance("cf.core.mini_chat.other.v1", "other", 1),
    ];
    let locator = hub_locator(hub_with_registry(Ok(instances)));

    let id = locator.find_instance().await.unwrap();

    assert_eq!(id, preferred.id.as_ref());
}

#[tokio::test]
async fn gateway_over_hub_resolves_the_scoped_plugin_client() {
    let instance = policy_instance("cf.core.mini_chat.static.v1", "acme", 100);
    let hub = hub_with_registry(Ok(vec![instance.clone()]));
    let gw = ModelPolicyGateway::new(Arc::clone(&hub), "acme");

    let err = gw.get_current_policy_version(user()).await.unwrap_err();
    assert!(err.is_transient(), "client not registered yet: {err}");

    let plugin: Arc<PolicyClient> = CountingPlugin::new();
    hub.register_scoped::<PolicyClient>(ClientScope::gts_id(instance.id.as_ref()), plugin);

    assert_eq!(gw.get_current_policy_version(user()).await.unwrap(), 1);
}
