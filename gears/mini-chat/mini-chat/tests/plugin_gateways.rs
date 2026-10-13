#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Lazy plugin resolution through the types registry (DESIGN section 3.2): the model policy
//! gateway and the audit gateway pick the plugin instance of the configured vendor with the lowest
//! priority and follow the audit delivery rules of ADR-0009.

use std::sync::Arc;

use async_trait::async_trait;
use mini_chat::domain::error::DomainError;
use mini_chat::domain::ports::ModelPolicyGateway;
use mini_chat::infra::audit_gateway::{AuditDelivery, AuditGateway};
use mini_chat::infra::model_policy_gateway::GtsModelPolicyGateway;
use mini_chat::infra::plugins::static_model_policy::{
    StaticModelPolicyConfig, StaticModelPolicyService,
};
use mini_chat::test_support::FakeTypesRegistry;
use mini_chat::{
    AuditPluginError, MiniChatAuditPluginClientV1, MiniChatAuditPluginSpecV1,
    MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginSpecV1, TurnAuditEvent,
    TurnMutationAuditEvent,
};
use mini_chat_sdk::{
    PolicyDecisions, RequesterType, ToolCalls, TurnAuditEventType, TurnLatency,
    TurnReplacementAudit, UsageTerminalState,
};
use serde_json::json;
use time::OffsetDateTime;
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::gts::PluginV1;
use types_registry_sdk::TypesRegistryClient;
use uuid::Uuid;

const VENDOR: &str = "acme";

fn new_hub() -> Arc<ClientHub> {
    let hub = Arc::new(ClientHub::default());
    hub.register::<dyn TypesRegistryClient>(Arc::new(FakeTypesRegistry::default()));
    hub
}

/// Registers a plugin instance in the types registry and returns its instance id.
async fn register_instance<P>(hub: &ClientHub, segment: &str, vendor: &str, priority: i16) -> String
where
    P: Default + for<'de> gts::GtsDeserialize<'de> + gts::GtsSerialize + gts::GtsSchema,
{
    let (id, json) = PluginV1::<P>::build_registration(segment, vendor, priority).unwrap();
    let registry = hub.get::<dyn TypesRegistryClient>().unwrap();
    for result in registry.register(vec![json]).await.unwrap() {
        assert!(result.is_ok());
    }
    id.to_string()
}

struct Audit {
    outcome: Result<(), AuditPluginError>,
}

#[async_trait]
impl MiniChatAuditPluginClientV1 for Audit {
    async fn emit_turn_audit(&self, _event: TurnAuditEvent) -> Result<(), AuditPluginError> {
        self.outcome.clone()
    }

    async fn emit_turn_mutation_audit(
        &self,
        _event: TurnMutationAuditEvent,
    ) -> Result<(), AuditPluginError> {
        self.outcome.clone()
    }
}

fn register_audit_client(hub: &ClientHub, instance: &str, outcome: Result<(), AuditPluginError>) {
    hub.register_scoped::<dyn MiniChatAuditPluginClientV1>(
        ClientScope::gts_id(instance),
        Arc::new(Audit { outcome }),
    );
}

fn turn_event() -> TurnAuditEvent {
    TurnAuditEvent {
        event_type: TurnAuditEventType::Completed,
        timestamp: OffsetDateTime::UNIX_EPOCH,
        tenant_id: Uuid::new_v4(),
        requester_type: RequesterType::User,
        user_id: Uuid::new_v4(),
        chat_id: Uuid::new_v4(),
        turn_id: Uuid::new_v4(),
        request_id: Uuid::new_v4(),
        selected_model: "m".to_owned(),
        effective_model: "m".to_owned(),
        terminal_state: UsageTerminalState::Completed,
        error_code: None,
        usage: None,
        latency: TurnLatency::default(),
        tool_calls: ToolCalls::default(),
        policy_decisions: PolicyDecisions::default(),
        prompt: String::new(),
        response: String::new(),
        trace_id: None,
    }
}

fn mutation_event() -> TurnMutationAuditEvent {
    TurnMutationAuditEvent::Retry(TurnReplacementAudit {
        timestamp: OffsetDateTime::UNIX_EPOCH,
        tenant_id: Uuid::new_v4(),
        actor_user_id: Uuid::new_v4(),
        chat_id: Uuid::new_v4(),
        original_request_id: Uuid::new_v4(),
        new_request_id: Uuid::new_v4(),
    })
}

#[tokio::test]
async fn without_an_audit_plugin_events_are_dropped_and_the_lookup_is_repeated() {
    let hub = new_hub();
    let gateway = AuditGateway::new(Arc::clone(&hub), VENDOR.to_owned());

    assert_eq!(
        gateway.emit_turn(turn_event()).await,
        AuditDelivery::Dropped
    );
    assert_eq!(
        gateway.emit_mutation(mutation_event()).await,
        AuditDelivery::Dropped
    );

    let instance =
        register_instance::<MiniChatAuditPluginSpecV1>(&hub, "acme.test._.audit.v1", VENDOR, 10)
            .await;
    register_audit_client(&hub, &instance, Ok(()));
    assert_eq!(
        gateway.emit_turn(turn_event()).await,
        AuditDelivery::Delivered,
        "a plugin registered later is used"
    );
}

#[tokio::test]
async fn an_audit_plugin_of_another_vendor_is_not_used() {
    let hub = new_hub();
    let other =
        register_instance::<MiniChatAuditPluginSpecV1>(&hub, "other.test._.audit.v1", "other", 1)
            .await;
    register_audit_client(&hub, &other, Ok(()));
    let gateway = AuditGateway::new(Arc::clone(&hub), VENDOR.to_owned());

    assert_eq!(
        gateway.emit_turn(turn_event()).await,
        AuditDelivery::Dropped
    );
}

#[tokio::test]
async fn plugin_failures_map_to_retry_and_reject() {
    let hub = new_hub();
    let instance =
        register_instance::<MiniChatAuditPluginSpecV1>(&hub, "acme.test._.audit.v1", VENDOR, 10)
            .await;
    let gateway = AuditGateway::new(Arc::clone(&hub), VENDOR.to_owned());

    register_audit_client(
        &hub,
        &instance,
        Err(AuditPluginError::Transient("busy".to_owned())),
    );
    assert_eq!(
        gateway.emit_turn(turn_event()).await,
        AuditDelivery::Retry("busy".to_owned())
    );

    register_audit_client(&hub, &instance, Err(AuditPluginError::PluginTimeout));
    assert!(matches!(
        gateway.emit_mutation(mutation_event()).await,
        AuditDelivery::Retry(_)
    ));

    register_audit_client(
        &hub,
        &instance,
        Err(AuditPluginError::Permanent("bad".to_owned())),
    );
    assert_eq!(
        gateway.emit_turn(turn_event()).await,
        AuditDelivery::Reject("bad".to_owned())
    );
}

#[tokio::test]
async fn a_resolved_instance_without_a_client_is_retried_and_the_cached_id_is_reset() {
    let hub = new_hub();
    let first =
        register_instance::<MiniChatAuditPluginSpecV1>(&hub, "acme.test._.first.v1", VENDOR, 10)
            .await;
    let gateway = AuditGateway::new(Arc::clone(&hub), VENDOR.to_owned());

    assert!(matches!(
        gateway.emit_turn(turn_event()).await,
        AuditDelivery::Retry(_)
    ));

    let better =
        register_instance::<MiniChatAuditPluginSpecV1>(&hub, "acme.test._.better.v1", VENDOR, 1)
            .await;
    register_audit_client(&hub, &better, Ok(()));
    register_audit_client(
        &hub,
        &first,
        Err(AuditPluginError::Permanent("never".to_owned())),
    );
    assert_eq!(
        gateway.emit_turn(turn_event()).await,
        AuditDelivery::Delivered,
        "the lowest priority instance wins after the reset"
    );
}

fn policy_service(model_id: &str) -> Arc<dyn MiniChatModelPolicyPluginClientV1> {
    let entry = json!({
        "id": model_id, "provider_model_id": model_id, "display_name": model_id,
        "provider_id": "openai", "provider_display_name": "OpenAI", "tier": "Standard",
        "enabled": true, "context_window": 1000, "max_output_tokens": 100, "max_input_tokens": 0,
        "input_tokens_credit_multiplier_micro": 1, "output_tokens_credit_multiplier_micro": 1,
        "max_num_results": 5,
        "general_config": {
            "type": "", "available_from": "", "max_file_size_mb": 1,
            "api_params": { "stop": [] },
            "features": { "streaming": true, "structured_output": false },
            "tool_support": {
                "web_search": false, "file_search": false, "image_generation": false,
                "code_interpreter": false, "mcp": false
            },
            "supported_endpoints": {}
        }
    });
    let config: StaticModelPolicyConfig =
        serde_json::from_value(json!({ "model_catalog": [entry] })).unwrap();
    Arc::new(StaticModelPolicyService::from_config(config))
}

fn register_policy_client(hub: &ClientHub, instance: &str, model_id: &str) {
    hub.register_scoped::<dyn MiniChatModelPolicyPluginClientV1>(
        ClientScope::gts_id(instance),
        policy_service(model_id),
    );
}

#[tokio::test]
async fn the_policy_gateway_reports_a_missing_plugin_as_unavailable() {
    let gateway = GtsModelPolicyGateway::new(new_hub(), VENDOR.to_owned());

    let outcome = gateway.current_snapshot(Uuid::new_v4()).await;

    assert!(
        matches!(outcome, Err(DomainError::PolicyUnavailable { .. })),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn the_policy_gateway_serves_the_snapshot_of_the_current_version() {
    let hub = new_hub();
    let instance = register_instance::<MiniChatModelPolicyPluginSpecV1>(
        &hub,
        "acme.test._.policy.v1",
        VENDOR,
        10,
    )
    .await;
    register_policy_client(&hub, &instance, "model-a");
    let gateway = GtsModelPolicyGateway::new(Arc::clone(&hub), VENDOR.to_owned());

    let snapshot = gateway.current_snapshot(Uuid::new_v4()).await.unwrap();

    assert_eq!(snapshot.policy_version, 1);
    assert_eq!(snapshot.model_catalog[0].id, "model-a");
}

#[tokio::test]
async fn the_policy_gateway_picks_the_configured_vendor_with_the_lowest_priority() {
    let hub = new_hub();
    let slow = register_instance::<MiniChatModelPolicyPluginSpecV1>(
        &hub,
        "acme.test._.slow.v1",
        VENDOR,
        50,
    )
    .await;
    let fast = register_instance::<MiniChatModelPolicyPluginSpecV1>(
        &hub,
        "acme.test._.fast.v1",
        VENDOR,
        5,
    )
    .await;
    let foreign = register_instance::<MiniChatModelPolicyPluginSpecV1>(
        &hub,
        "other.test._.policy.v1",
        "other",
        1,
    )
    .await;
    register_policy_client(&hub, &slow, "slow");
    register_policy_client(&hub, &fast, "fast");
    register_policy_client(&hub, &foreign, "foreign");
    let gateway = GtsModelPolicyGateway::new(Arc::clone(&hub), VENDOR.to_owned());

    let snapshot = gateway.current_snapshot(Uuid::new_v4()).await.unwrap();

    assert_eq!(snapshot.model_catalog[0].id, "fast");
}
