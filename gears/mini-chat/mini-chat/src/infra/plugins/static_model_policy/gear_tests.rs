//! Plugin gear tests: config loading, instance registration in types-registry and the scoped
//! client in the `ClientHub` (algorithm `static-policy-plugin`, steps 1 and 6).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use mini_chat_sdk::MiniChatModelPolicyPluginClientV1;
use serde_json::{Value, json};
use toolkit::Gear;
use toolkit::client_hub::{ClientHub, ClientScope};
use types_registry_sdk::TypesRegistryClient;
use types_registry_sdk::testing::MockTypesRegistryClient;

use super::StaticMiniChatModelPolicyPlugin;
use crate::test_support::{RecordingRegistry, USER_A1, entry_json, gear_ctx};

const GEAR_NAME: &str = "static-mini-chat-model-policy-plugin";
const SPEC_TYPE: &str =
    "gts.cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.model_policy_plugin.v1~";

fn hub_with_registry() -> (Arc<ClientHub>, Arc<RecordingRegistry>) {
    let registry = RecordingRegistry::new(MockTypesRegistryClient::new());
    let hub = Arc::new(ClientHub::new());
    let dynamic: Arc<dyn TypesRegistryClient> = registry.clone();
    hub.register::<dyn TypesRegistryClient>(dynamic);
    (hub, registry)
}

async fn init_with(
    config: Option<Value>,
) -> (anyhow::Result<()>, Arc<ClientHub>, Arc<RecordingRegistry>) {
    let (hub, registry) = hub_with_registry();
    let ctx = gear_ctx(GEAR_NAME, config, Arc::clone(&hub), None);
    let plugin = StaticMiniChatModelPolicyPlugin::default();
    let outcome = plugin.init(&ctx).await;
    (outcome, hub, registry)
}

#[test]
fn the_gear_is_named_after_its_config_section() {
    assert_eq!(StaticMiniChatModelPolicyPlugin::MODULE_NAME, GEAR_NAME);
}

#[tokio::test]
async fn an_absent_section_registers_a_default_instance_and_a_scoped_client() {
    let (outcome, hub, registry) = init_with(None).await;
    outcome.expect("init succeeds without a config section");

    let registered = registry.registered();
    assert_eq!(registered.len(), 1);
    let instance = &registered[0];
    assert_eq!(instance["vendor"], "constructorfabric");
    assert_eq!(instance["priority"], 100);
    let id = instance["id"].as_str().unwrap();
    assert!(id.starts_with(SPEC_TYPE), "{id}");

    let client = hub
        .get_scoped::<dyn MiniChatModelPolicyPluginClientV1>(&ClientScope::gts_id(id))
        .expect("scoped client registered under the instance id");
    assert_eq!(client.get_current_policy_version(USER_A1).await.unwrap(), 1);
    assert!(
        client
            .get_policy_snapshot(USER_A1, 1)
            .await
            .unwrap()
            .model_catalog
            .is_empty()
    );
}

#[tokio::test]
async fn the_configured_vendor_and_priority_are_registered() {
    let config = json!({ "vendor": "acme", "priority": 7, "model_catalog": [] });
    let (outcome, _hub, registry) = init_with(Some(config)).await;
    outcome.expect("init");
    let instance = &registry.registered()[0];
    assert_eq!(instance["vendor"], "acme");
    assert_eq!(instance["priority"], 7);
}

#[tokio::test]
async fn the_configured_catalog_is_served_through_the_registered_client() {
    let config = json!({ "model_catalog": [entry_json("gpt-on", "premium", true)] });
    let (outcome, hub, registry) = init_with(Some(config)).await;
    outcome.expect("init");
    let id = registry.registered()[0]["id"].as_str().unwrap().to_owned();
    let client = hub
        .get_scoped::<dyn MiniChatModelPolicyPluginClientV1>(&ClientScope::gts_id(&id))
        .unwrap();
    let snapshot = client.get_policy_snapshot(USER_A1, 1).await.unwrap();
    assert_eq!(snapshot.model_catalog.len(), 1);
    assert_eq!(snapshot.model_catalog[0].id, "gpt-on");
}

#[tokio::test]
async fn an_out_of_range_multiplier_fails_init_before_anything_is_registered() {
    let mut bad = entry_json("gpt-bad", "standard", true);
    bad["input_tokens_credit_multiplier_micro"] = json!(0);
    let (outcome, hub, registry) = init_with(Some(json!({ "model_catalog": [bad] }))).await;
    assert!(outcome.is_err());
    assert!(
        registry.registered().is_empty(),
        "nothing may be registered for a failed init"
    );
    assert!(
        hub.get_scoped::<dyn MiniChatModelPolicyPluginClientV1>(&ClientScope::gts_id(SPEC_TYPE))
            .is_err()
    );
}

#[tokio::test]
async fn an_unknown_key_or_a_missing_catalog_fails_init() {
    for config in [
        json!({ "model_catalog": [], "bogus_key": 1 }),
        json!({ "vendor": "acme" }),
        json!({ "model_catalog": [], "kill_switches": { "disable_everything": true } }),
    ] {
        let (outcome, _hub, registry) = init_with(Some(config.clone())).await;
        assert!(outcome.is_err(), "{config}");
        assert!(registry.registered().is_empty(), "{config}");
    }
}

#[tokio::test]
async fn a_missing_types_registry_client_fails_init() {
    let hub = Arc::new(ClientHub::new());
    let ctx = gear_ctx(GEAR_NAME, None, hub, None);
    let plugin = StaticMiniChatModelPolicyPlugin::default();
    assert!(plugin.init(&ctx).await.is_err());
}

#[tokio::test]
async fn the_service_is_available_after_init() {
    let (hub, _registry) = hub_with_registry();
    let ctx = gear_ctx(GEAR_NAME, None, hub, None);
    let plugin = StaticMiniChatModelPolicyPlugin::default();
    assert!(plugin.service().is_none());
    plugin.init(&ctx).await.expect("init");
    assert!(plugin.service().is_some());
}
