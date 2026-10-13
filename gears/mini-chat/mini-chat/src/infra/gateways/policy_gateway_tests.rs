#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use mini_chat_sdk::{
    BillingOutcome, MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginSpecV1,
    PublishError, RequesterType, SettlementMethod, TerminalState, UsageEvent,
};
use toolkit::client_hub::{ClientHub, ClientScope};
use types_registry_sdk::testing::MockTypesRegistryClient;
use uuid::Uuid;

use super::*;
use crate::domain::error::DomainError;
use crate::domain::ports::PolicyPort;
use crate::infra::gateways::test_helpers::{hub_with_registry, plugin_instance};
use crate::infra::plugins::static_model_policy::StaticModelPolicyService;
use crate::infra::plugins::static_model_policy::config::StaticModelPolicyConfig;

const SEGMENT: &str = "cf.test.policy_static.plugin.v1";

fn setup(vendor: &str, with_client: bool) -> (Arc<MockTypesRegistryClient>, Arc<ClientHub>) {
    let (id, inst) = plugin_instance::<MiniChatModelPolicyPluginSpecV1>(SEGMENT, vendor, 100);
    let registry = Arc::new(MockTypesRegistryClient::new().with_instances([inst]));
    let hub = hub_with_registry(&registry);
    if with_client {
        let svc =
            StaticModelPolicyService::from_config(&StaticModelPolicyConfig::default()).unwrap();
        let api: Arc<dyn MiniChatModelPolicyPluginClientV1> = Arc::new(svc);
        hub.register_scoped::<dyn MiniChatModelPolicyPluginClientV1>(ClientScope::gts_id(&id), api);
    }
    (registry, hub)
}

fn usage() -> UsageEvent {
    UsageEvent {
        tenant_id: Uuid::new_v4(),
        user_id: Some(Uuid::new_v4()),
        chat_id: Uuid::new_v4(),
        turn_id: Some(Uuid::new_v4()),
        request_id: Uuid::new_v4(),
        effective_model: "m".to_owned(),
        selected_model: "m".to_owned(),
        terminal_state: TerminalState::Completed,
        billing_outcome: BillingOutcome::Completed,
        usage: None,
        actual_credits_micro: 0,
        settlement_method: SettlementMethod::Actual,
        policy_version_applied: 1,
        web_search_calls: 0,
        code_interpreter_calls: 0,
        file_search_calls: 0,
        timestamp: time::OffsetDateTime::UNIX_EPOCH,
        requester_type: RequesterType::User,
        dedupe_key: "k".to_owned(),
        system_task_type: None,
    }
}

#[tokio::test]
async fn resolves_plugin_by_vendor_and_reads_policy() {
    let (registry, hub) = setup("constructorfabric", true);
    let gw = PolicyGateway::new(hub, "constructorfabric");
    let user = Uuid::new_v4();
    let snap = gw.current_snapshot(user).await.unwrap();
    assert_eq!(snap.policy_version, 1);
    let snap = gw.snapshot_for_version(user, 1).await.unwrap();
    assert_eq!(snap.policy_version, 1);
    let limits = gw.user_limits(user, 1).await.unwrap();
    assert_eq!(limits.standard.limit_daily_credits_micro, 100_000_000);
    gw.publish_usage(usage()).await.unwrap();
    assert_eq!(
        registry.list_instance_calls(),
        1,
        "selected instance is cached"
    );
}

#[tokio::test]
async fn no_plugin_for_vendor_is_plugin_unavailable() {
    let (_registry, hub) = setup("other-vendor", true);
    let gw = PolicyGateway::new(hub, "constructorfabric");
    assert!(matches!(
        gw.current_snapshot(Uuid::new_v4()).await,
        Err(DomainError::PluginUnavailable(_))
    ));
    assert!(matches!(
        gw.publish_usage(usage()).await,
        Err(PublishError::Transient(_))
    ));
}

#[tokio::test]
async fn missing_client_resets_selection() {
    let (registry, hub) = setup("constructorfabric", false);
    let gw = PolicyGateway::new(hub, "constructorfabric");
    assert!(matches!(
        gw.current_snapshot(Uuid::new_v4()).await,
        Err(DomainError::PluginUnavailable(_))
    ));
    assert!(matches!(
        gw.user_limits(Uuid::new_v4(), 1).await,
        Err(DomainError::PluginUnavailable(_))
    ));
    assert_eq!(registry.list_instance_calls(), 2);
}

#[tokio::test]
async fn registry_unavailable_is_plugin_unavailable() {
    let gw = PolicyGateway::new(Arc::new(ClientHub::new()), "constructorfabric");
    assert!(matches!(
        gw.snapshot_for_version(Uuid::new_v4(), 1).await,
        Err(DomainError::PluginUnavailable(_))
    ));
}
