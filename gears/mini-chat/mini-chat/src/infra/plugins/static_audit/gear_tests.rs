//! Plugin gear tests: config loading, instance registration in types-registry and the scoped
//! client in the `ClientHub`.
//!
//! Slice S2: algorithm `static-audit-plugin`, steps `inst-static-audit-load`,
//! `inst-static-audit-register` and `inst-static-audit-return`; definition of done `audit`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use mini_chat_sdk::MiniChatAuditPluginClientV1;
use serde_json::{Value, json};
use toolkit::Gear;
use toolkit::client_hub::{ClientHub, ClientScope};
use types_registry_sdk::TypesRegistryClient;
use types_registry_sdk::testing::MockTypesRegistryClient;
use uuid::Uuid;

use super::StaticMiniChatAuditPlugin;
use crate::test_support::s2::turn_event;
use crate::test_support::{RecordingRegistry, gear_ctx};

const GEAR_NAME: &str = "static-mini-chat-audit-plugin";
const SPEC_TYPE: &str = "gts.cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.audit_plugin.v1~";

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
    let plugin = StaticMiniChatAuditPlugin::default();
    let outcome = plugin.init(&ctx).await;
    (outcome, hub, registry)
}

#[test]
fn the_gear_is_named_after_its_config_section() {
    assert_eq!(StaticMiniChatAuditPlugin::MODULE_NAME, GEAR_NAME);
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
        .get_scoped::<dyn MiniChatAuditPluginClientV1>(&ClientScope::gts_id(id))
        .expect("scoped client registered under the instance id");
    client
        .emit(mini_chat_sdk::AuditEvent::TurnCompleted(turn_event(
            Uuid::nil(),
        )))
        .await
        .expect("the static plugin accepts events");
}

#[tokio::test]
async fn the_configured_vendor_and_priority_are_registered() {
    let (outcome, _hub, registry) =
        init_with(Some(json!({ "vendor": "acme", "priority": 7 }))).await;
    outcome.expect("init");
    let instance = &registry.registered()[0];
    assert_eq!(instance["vendor"], "acme");
    assert_eq!(instance["priority"], 7);
}

#[tokio::test]
async fn a_disabled_plugin_still_registers_so_it_stays_selectable_but_silent() {
    let (outcome, hub, registry) = init_with(Some(json!({ "enabled": false }))).await;
    outcome.expect("init");
    let id = registry.registered()[0]["id"].as_str().unwrap().to_owned();
    assert!(
        hub.get_scoped::<dyn MiniChatAuditPluginClientV1>(&ClientScope::gts_id(&id))
            .is_ok()
    );
}

#[tokio::test]
async fn an_unknown_key_fails_init_before_anything_is_registered() {
    for config in [json!({ "bogus_key": 1 }), json!({ "model_catalog": [] })] {
        let (outcome, hub, registry) = init_with(Some(config.clone())).await;
        assert!(outcome.is_err(), "{config}");
        assert!(registry.registered().is_empty(), "{config}");
        assert!(
            hub.get_scoped::<dyn MiniChatAuditPluginClientV1>(&ClientScope::gts_id(SPEC_TYPE))
                .is_err()
        );
    }
}

#[tokio::test]
async fn init_without_a_types_registry_client_fails() {
    let hub = Arc::new(ClientHub::new());
    let ctx = gear_ctx(GEAR_NAME, None, hub, None);
    assert!(
        StaticMiniChatAuditPlugin::default()
            .init(&ctx)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn the_service_is_available_after_init() {
    let (hub, _registry) = hub_with_registry();
    let ctx = gear_ctx(GEAR_NAME, None, hub, None);
    let plugin = StaticMiniChatAuditPlugin::default();
    assert!(plugin.service().is_none());
    plugin.init(&ctx).await.expect("init");
    assert!(plugin.service().is_some());
}
