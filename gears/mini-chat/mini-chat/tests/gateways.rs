//! Model-policy gateway and audit gateway: lazy plugin resolution through
//! types-registry + ClientHub (DESIGN 3.2 "Model policy gateway", "Audit plugin
//! and audit outbox", 5.6 handler outcomes).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::use_debug,
    clippy::unreadable_literal,
    clippy::needless_pass_by_value
)]

mod common;

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use common::{FakePolicyPlugin, USER_A, catalog_entry};
use mini_chat::domain::error::DomainError;
use mini_chat::infra::audit::{AuditDeliveryOutcome, AuditGateway};
use mini_chat::infra::model_policy::ModelPolicyGateway;
use mini_chat_sdk::{
    AuditEvent, AuditPluginError, MiniChatAuditPluginClientV1, MiniChatAuditPluginSpecV1,
    MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginSpecV1,
};
use serde_json::json;
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::gts::PluginV1;
use toolkit_canonical_errors::CanonicalError;
use types_registry_sdk::TypesRegistryClient;
use types_registry_sdk::testing::{MockTypesRegistryClient, make_test_instance};
use uuid::Uuid;

const VENDOR: &str = "constructorfabric";

fn model_policy_instance(
    segment: &str,
    vendor: &str,
    priority: i16,
) -> (String, serde_json::Value) {
    let (id, payload) =
        PluginV1::<MiniChatModelPolicyPluginSpecV1>::build_registration(segment, vendor, priority)
            .expect("registration payload");
    let id: &str = id.as_ref();
    (id.to_owned(), payload)
}

fn audit_instance(segment: &str, vendor: &str, priority: i16) -> (String, serde_json::Value) {
    let (id, payload) =
        PluginV1::<MiniChatAuditPluginSpecV1>::build_registration(segment, vendor, priority)
            .expect("registration payload");
    let id: &str = id.as_ref();
    (id.to_owned(), payload)
}

fn registry_with(instances: &[(String, serde_json::Value)]) -> Arc<MockTypesRegistryClient> {
    Arc::new(
        MockTypesRegistryClient::new().with_instances(
            instances
                .iter()
                .map(|(id, payload)| make_test_instance(id, payload.clone())),
        ),
    )
}

fn hub_with_registry(registry: Arc<MockTypesRegistryClient>) -> Arc<ClientHub> {
    let hub = Arc::new(ClientHub::new());
    let dyn_registry: Arc<dyn TypesRegistryClient> = registry;
    hub.register::<dyn TypesRegistryClient>(dyn_registry);
    hub
}

fn register_policy_client(hub: &ClientHub, instance_id: &str, client: Arc<FakePolicyPlugin>) {
    let client: Arc<dyn MiniChatModelPolicyPluginClientV1> = client;
    hub.register_scoped::<dyn MiniChatModelPolicyPluginClientV1>(
        ClientScope::gts_id(instance_id),
        client,
    );
}

// ═════════════════════════ model-policy gateway ═════════════════════════════════

#[tokio::test]
async fn policy_gateway_resolves_the_plugin_of_the_configured_vendor() {
    let (id, payload) =
        model_policy_instance("cf.core._.static_mini_chat_model_policy.v1", VENDOR, 100);
    let registry = registry_with(&[(id.clone(), payload)]);
    let hub = hub_with_registry(registry);
    let plugin = FakePolicyPlugin::new(vec![catalog_entry("gpt-a", "Standard", true)]);
    register_policy_client(&hub, &id, plugin.clone());

    let gateway = ModelPolicyGateway::new(hub, VENDOR);
    let snapshot = gateway.current_snapshot(USER_A).await.expect("snapshot");

    assert_eq!(snapshot.policy_version, 1);
    assert_eq!(snapshot.model_catalog.len(), 1);
    assert_eq!(snapshot.model_catalog[0].id, "gpt-a");
    assert_eq!(
        plugin.seen_users.lock().unwrap().as_slice(),
        &[USER_A, USER_A]
    );
    assert_eq!(plugin.version_calls.load(Ordering::SeqCst), 1);
    assert_eq!(plugin.snapshot_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn policy_gateway_prefers_the_lowest_priority_value() {
    let (id_low, payload_low) = model_policy_instance("cf.core._.low.v1", VENDOR, 10);
    let (id_high, payload_high) = model_policy_instance("cf.core._.high.v1", VENDOR, 100);
    let registry = registry_with(&[
        (id_high.clone(), payload_high),
        (id_low.clone(), payload_low),
    ]);
    let hub = hub_with_registry(registry);
    let preferred = FakePolicyPlugin::new(vec![catalog_entry("preferred", "Standard", true)]);
    let other = FakePolicyPlugin::new(vec![catalog_entry("other", "Standard", true)]);
    register_policy_client(&hub, &id_low, preferred);
    register_policy_client(&hub, &id_high, other);

    let gateway = ModelPolicyGateway::new(hub, VENDOR);
    let snapshot = gateway.current_snapshot(USER_A).await.unwrap();
    assert_eq!(snapshot.model_catalog[0].id, "preferred");
}

#[tokio::test]
async fn policy_gateway_ignores_plugins_of_other_vendors() {
    let (id, payload) = model_policy_instance("cf.core._.foreign.v1", "someone-else", 1);
    let registry = registry_with(&[(id.clone(), payload)]);
    let hub = hub_with_registry(registry);
    register_policy_client(&hub, &id, FakePolicyPlugin::new(vec![]));

    let gateway = ModelPolicyGateway::new(hub, VENDOR);
    let err = gateway
        .current_snapshot(USER_A)
        .await
        .expect_err("no plugin for vendor");
    assert!(matches!(err, DomainError::Internal { .. }), "{err:?}");
}

#[tokio::test]
async fn policy_gateway_without_any_registered_plugin_fails_with_internal() {
    let hub = hub_with_registry(registry_with(&[]));
    let gateway = ModelPolicyGateway::new(hub, VENDOR);
    let err = gateway
        .current_snapshot(USER_A)
        .await
        .expect_err("nothing registered");
    assert!(
        matches!(err, DomainError::Internal { .. }),
        "policy resolution failure -> 500 internal (DESIGN 3.3 table): {err:?}"
    );
}

#[tokio::test]
async fn policy_gateway_with_a_registered_instance_but_no_client_fails() {
    let (id, payload) = model_policy_instance("cf.core._.late.v1", VENDOR, 100);
    let hub = hub_with_registry(registry_with(&[(id, payload)]));
    let gateway = ModelPolicyGateway::new(hub, VENDOR);
    let err = gateway
        .current_snapshot(USER_A)
        .await
        .expect_err("client missing from ClientHub");
    assert!(matches!(err, DomainError::Internal { .. }), "{err:?}");
}

#[tokio::test]
async fn policy_gateway_without_a_types_registry_client_fails() {
    let gateway = ModelPolicyGateway::new(Arc::new(ClientHub::new()), VENDOR);
    let err = gateway
        .current_snapshot(USER_A)
        .await
        .expect_err("no registry");
    assert!(matches!(err, DomainError::Internal { .. }), "{err:?}");
}

#[tokio::test]
async fn policy_gateway_resolves_the_instance_once_and_reuses_it() {
    let (id, payload) = model_policy_instance("cf.core._.cached.v1", VENDOR, 100);
    let registry = registry_with(&[(id.clone(), payload)]);
    let hub = hub_with_registry(registry.clone());
    register_policy_client(&hub, &id, FakePolicyPlugin::new(vec![]));

    let gateway = ModelPolicyGateway::new(hub, VENDOR);
    gateway.current_snapshot(USER_A).await.unwrap();
    gateway.current_snapshot(USER_A).await.unwrap();
    gateway.user_limits(USER_A, 1).await.unwrap();

    assert_eq!(
        registry.list_instance_calls(),
        1,
        "the resolved instance id is cached; types-registry is asked once"
    );
}

#[tokio::test]
async fn policy_gateway_passes_user_limits_through() {
    let (id, payload) = model_policy_instance("cf.core._.limits.v1", VENDOR, 100);
    let hub = hub_with_registry(registry_with(&[(id.clone(), payload)]));
    let plugin = FakePolicyPlugin::new(vec![]);
    register_policy_client(&hub, &id, plugin.clone());

    let gateway = ModelPolicyGateway::new(hub, VENDOR);
    let limits = gateway.user_limits(USER_A, 1).await.expect("limits");
    assert_eq!(limits.user_id, USER_A);
    assert_eq!(limits.policy_version, 1);
    assert_eq!(plugin.limits_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn policy_gateway_plugin_failure_is_internal_and_does_not_leak_the_cause() {
    let (id, payload) = model_policy_instance("cf.core._.failing.v1", VENDOR, 100);
    let hub = hub_with_registry(registry_with(&[(id.clone(), payload)]));
    let plugin = FakePolicyPlugin::new(vec![]);
    plugin.fail_with("upstream-ccm-token-secret");
    register_policy_client(&hub, &id, plugin);

    let gateway = ModelPolicyGateway::new(hub, VENDOR);
    let err = gateway
        .current_snapshot(USER_A)
        .await
        .expect_err("plugin fails");
    assert!(matches!(err, DomainError::Internal { .. }), "{err:?}");
    assert!(
        !err.to_string().contains("upstream-ccm-token-secret"),
        "Display of a domain error never carries plugin internals: {err}"
    );
}

#[tokio::test]
async fn policy_gateway_from_client_bypasses_types_registry() {
    let plugin = FakePolicyPlugin::new(vec![catalog_entry("direct", "Premium", true)]);
    let gateway = ModelPolicyGateway::from_client(plugin);
    let snapshot = gateway.current_snapshot(USER_A).await.unwrap();
    assert_eq!(snapshot.model_catalog[0].id, "direct");
}

// ═══════════════════════════════ audit gateway ═════════════════════════════════

#[derive(Clone, Copy)]
enum AuditScript {
    Ok,
    Transient,
    Timeout,
    Permanent,
}

struct ScriptedAuditPlugin {
    script: Mutex<AuditScript>,
    received: AtomicUsize,
}

impl ScriptedAuditPlugin {
    fn new(script: AuditScript) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script),
            received: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl MiniChatAuditPluginClientV1 for ScriptedAuditPlugin {
    async fn emit(&self, _event: AuditEvent) -> Result<(), AuditPluginError> {
        self.received.fetch_add(1, Ordering::SeqCst);
        match *self.script.lock().unwrap() {
            AuditScript::Ok => Ok(()),
            AuditScript::Transient => Err(AuditPluginError::Transient("busy".to_owned())),
            AuditScript::Timeout => Err(AuditPluginError::PluginTimeout),
            AuditScript::Permanent => Err(AuditPluginError::Permanent("rejected".to_owned())),
        }
    }
}

fn audit_event() -> AuditEvent {
    serde_json::from_value(json!({
        "event_type": "turn_delete",
        "tenant_id": Uuid::new_v4(),
        "actor_user_id": USER_A,
        "chat_id": Uuid::new_v4(),
        "request_id": Uuid::new_v4(),
        "timestamp": "2026-03-02T10:00:00Z",
    }))
    .expect("audit event")
}

fn audit_hub(
    script: AuditScript,
) -> (
    Arc<ClientHub>,
    Arc<ScriptedAuditPlugin>,
    Arc<MockTypesRegistryClient>,
) {
    let (id, payload) = audit_instance("cf.core._.static_mini_chat_audit.v1", VENDOR, 100);
    let registry = registry_with(&[(id.clone(), payload)]);
    let hub = hub_with_registry(registry.clone());
    let plugin = ScriptedAuditPlugin::new(script);
    let client: Arc<dyn MiniChatAuditPluginClientV1> = plugin.clone();
    hub.register_scoped::<dyn MiniChatAuditPluginClientV1>(ClientScope::gts_id(&id), client);
    (hub, plugin, registry)
}

#[tokio::test]
async fn audit_event_is_delivered_to_the_resolved_plugin() {
    let (hub, plugin, _registry) = audit_hub(AuditScript::Ok);
    let gateway = AuditGateway::new(hub, VENDOR);
    assert_eq!(
        gateway.deliver(&audit_event()).await,
        AuditDeliveryOutcome::Delivered
    );
    assert_eq!(plugin.received.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn audit_event_is_acknowledged_and_dropped_when_no_plugin_is_registered() {
    let hub = hub_with_registry(registry_with(&[]));
    let gateway = AuditGateway::new(hub, VENDOR);
    assert_eq!(
        gateway.deliver(&audit_event()).await,
        AuditDeliveryOutcome::Dropped
    );
}

#[tokio::test]
async fn audit_no_plugin_result_is_not_cached() {
    // "The 'no plugin registered' result is not cached: every delivery looks
    // the plugin up again" (DESIGN 3.2).
    let registry = registry_with(&[]);
    let hub = hub_with_registry(registry.clone());
    let gateway = AuditGateway::new(hub, VENDOR);

    gateway.deliver(&audit_event()).await;
    gateway.deliver(&audit_event()).await;
    assert_eq!(registry.list_instance_calls(), 2);
}

#[tokio::test]
async fn audit_found_instance_id_is_cached() {
    let (hub, plugin, registry) = audit_hub(AuditScript::Ok);
    let gateway = AuditGateway::new(hub, VENDOR);
    for _ in 0..3 {
        assert_eq!(
            gateway.deliver(&audit_event()).await,
            AuditDeliveryOutcome::Delivered
        );
    }
    assert_eq!(plugin.received.load(Ordering::SeqCst), 3);
    assert_eq!(
        registry.list_instance_calls(),
        1,
        "a found instance id is cached"
    );
}

#[tokio::test]
async fn audit_resolution_error_is_retried() {
    let registry = Arc::new(
        MockTypesRegistryClient::new().with_list_error(
            CanonicalError::service_unavailable()
                .with_detail("registry down".to_owned())
                .create(),
        ),
    );
    let gateway = AuditGateway::new(hub_with_registry(registry), VENDOR);
    assert_eq!(
        gateway.deliver(&audit_event()).await,
        AuditDeliveryOutcome::Retry
    );
}

#[tokio::test]
async fn audit_missing_types_registry_client_is_retried() {
    let gateway = AuditGateway::new(Arc::new(ClientHub::new()), VENDOR);
    assert_eq!(
        gateway.deliver(&audit_event()).await,
        AuditDeliveryOutcome::Retry
    );
}

#[tokio::test]
async fn audit_instance_without_a_client_in_the_hub_is_retried_not_dropped() {
    let (id, payload) = audit_instance("cf.core._.not_yet.v1", VENDOR, 100);
    let hub = hub_with_registry(registry_with(&[(id, payload)]));
    let gateway = AuditGateway::new(hub, VENDOR);
    assert_eq!(
        gateway.deliver(&audit_event()).await,
        AuditDeliveryOutcome::Retry
    );
}

#[tokio::test]
async fn audit_transient_plugin_errors_are_retried() {
    for script in [AuditScript::Transient, AuditScript::Timeout] {
        let (hub, _plugin, _registry) = audit_hub(script);
        let gateway = AuditGateway::new(hub, VENDOR);
        assert_eq!(
            gateway.deliver(&audit_event()).await,
            AuditDeliveryOutcome::Retry
        );
    }
}

#[tokio::test]
async fn audit_permanent_plugin_error_is_rejected() {
    let (hub, _plugin, _registry) = audit_hub(AuditScript::Permanent);
    let gateway = AuditGateway::new(hub, VENDOR);
    assert_eq!(
        gateway.deliver(&audit_event()).await,
        AuditDeliveryOutcome::Reject
    );
}
