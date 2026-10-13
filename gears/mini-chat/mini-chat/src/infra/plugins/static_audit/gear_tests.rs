#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Tests of the gear registration part of
//! `cpt-cf-mini-chat-algo-gear-foundation-static-audit-plugin`
//! (step `inst-register-audit-instance`).

use std::sync::Arc;

use mini_chat_sdk::{
    MiniChatAuditEvent, MiniChatAuditPluginClientV1, MiniChatAuditPluginSpecV1,
    TurnMutationAuditEvent,
};
use serde_json::json;
use time::OffsetDateTime;
use toolkit::Gear;
use toolkit::client_hub::{ClientHub, ClientScope};
use uuid::Uuid;

use super::StaticAuditPlugin;
use crate::infra::audit_gateway::AuditGateway;
use crate::infra::test_support::{FakeTypesRegistry, FixedConfig, gear_ctx};

const GEAR: &str = "static-mini-chat-audit-plugin";

fn expected_instance_id() -> String {
    format!(
        "{}cf.core._.static_mini_chat_audit.v1",
        MiniChatAuditPluginSpecV1::gts_type_id()
    )
}

fn event() -> MiniChatAuditEvent {
    MiniChatAuditEvent::Mutation(TurnMutationAuditEvent::TurnDelete {
        tenant_id: Uuid::from_u128(0x7E),
        actor_user_id: Uuid::from_u128(0x55),
        chat_id: Uuid::from_u128(0xC1),
        request_id: Uuid::from_u128(0x91),
        timestamp: OffsetDateTime::UNIX_EPOCH,
    })
}

#[test]
fn the_gear_is_named_after_the_bundled_plugin() {
    assert_eq!(StaticAuditPlugin::MODULE_NAME, GEAR);
}

#[tokio::test]
async fn init_registers_the_gts_instance_with_vendor_and_priority() {
    let registry = FakeTypesRegistry::new();
    let hub = Arc::new(ClientHub::new());
    let ctx = gear_ctx(
        GEAR,
        FixedConfig::with_section(GEAR, json!({ "vendor": "acme", "priority": 7 })),
        &hub,
        &registry,
    );

    StaticAuditPlugin::default().init(&ctx).await.expect("init");

    assert_eq!(registry.instance_ids(), vec![expected_instance_id()]);
    let object = registry.instance_object(&expected_instance_id()).unwrap();
    assert_eq!(object["id"], expected_instance_id());
    assert_eq!(object["vendor"], "acme");
    assert_eq!(object["priority"], 7);
}

#[tokio::test]
async fn init_publishes_the_client_scoped_to_the_instance() {
    let registry = FakeTypesRegistry::new();
    let hub = Arc::new(ClientHub::new());
    let ctx = gear_ctx(GEAR, FixedConfig::default(), &hub, &registry);

    StaticAuditPlugin::default().init(&ctx).await.expect("init");

    let client = hub
        .try_get_scoped::<dyn MiniChatAuditPluginClientV1>(&ClientScope::gts_id(
            &expected_instance_id(),
        ))
        .expect("scoped client");
    client.emit(event()).await.expect("emit");
    assert!(
        hub.try_get::<dyn MiniChatAuditPluginClientV1>().is_none(),
        "the client is registered under the instance scope only"
    );
}

#[tokio::test]
async fn an_absent_section_registers_the_defaults() {
    let registry = FakeTypesRegistry::new();
    let hub = Arc::new(ClientHub::new());
    let ctx = gear_ctx(GEAR, FixedConfig::default(), &hub, &registry);

    StaticAuditPlugin::default().init(&ctx).await.expect("init");

    let object = registry.instance_object(&expected_instance_id()).unwrap();
    assert_eq!(object["vendor"], "constructorfabric");
    assert_eq!(object["priority"], 100);
}

#[tokio::test]
async fn an_invalid_section_fails_init_and_registers_nothing() {
    let registry = FakeTypesRegistry::new();
    let hub = Arc::new(ClientHub::new());
    let ctx = gear_ctx(
        GEAR,
        FixedConfig::with_section(GEAR, json!({ "enabeld": true })),
        &hub,
        &registry,
    );

    let err = StaticAuditPlugin::default()
        .init(&ctx)
        .await
        .expect_err("unknown key");

    assert!(err.to_string().contains("enabeld"), "{err}");
    assert!(registry.instance_ids().is_empty());
}

#[tokio::test]
async fn a_registry_failure_fails_init_without_publishing_a_client() {
    let registry = FakeTypesRegistry::new();
    registry.fail_register("registry down");
    let hub = Arc::new(ClientHub::new());
    let ctx = gear_ctx(GEAR, FixedConfig::default(), &hub, &registry);

    StaticAuditPlugin::default()
        .init(&ctx)
        .await
        .expect_err("registration fails");

    assert!(
        hub.try_get_scoped::<dyn MiniChatAuditPluginClientV1>(&ClientScope::gts_id(
            &expected_instance_id()
        ))
        .is_none()
    );
}

#[tokio::test]
async fn the_registered_plugin_is_found_by_the_audit_gateway() {
    let registry = FakeTypesRegistry::new();
    let hub = Arc::new(ClientHub::new());
    let ctx = gear_ctx(GEAR, FixedConfig::default(), &hub, &registry);
    StaticAuditPlugin::default().init(&ctx).await.expect("init");

    AuditGateway::new(&hub, "constructorfabric")
        .emit(event())
        .await
        .expect("the gateway resolves the bundled plugin and emits");
}
