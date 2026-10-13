#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Tests of the audit gateway over the real adapters.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mini_chat_sdk::{
    AuditPluginError, MiniChatAuditEvent, MiniChatAuditPluginClientV1, MiniChatAuditPluginSpecV1,
    TurnMutationAuditEvent,
};
use serde_json::json;
use time::OffsetDateTime;
use toolkit::client_hub::{ClientHub, ClientScope};
use types_registry_sdk::TypesRegistryClient;
use uuid::Uuid;

use super::{AUDIT_EMIT_TIMEOUT, AuditGateway, AuditGatewayError};
use crate::infra::plugins::static_audit::{StaticAuditConfig, StaticAuditService};
use crate::infra::test_support::FakeTypesRegistry;

const VENDOR: &str = "constructorfabric";

fn event() -> MiniChatAuditEvent {
    MiniChatAuditEvent::Mutation(TurnMutationAuditEvent::TurnDelete {
        tenant_id: Uuid::from_u128(0x7E),
        actor_user_id: Uuid::from_u128(0x55),
        chat_id: Uuid::from_u128(0xC1),
        request_id: Uuid::from_u128(0x91),
        timestamp: OffsetDateTime::UNIX_EPOCH,
    })
}

struct HangingPlugin;

#[async_trait]
impl MiniChatAuditPluginClientV1 for HangingPlugin {
    async fn emit(&self, _event: MiniChatAuditEvent) -> Result<(), AuditPluginError> {
        std::future::pending().await
    }
}

struct FailingPlugin;

#[async_trait]
impl MiniChatAuditPluginClientV1 for FailingPlugin {
    async fn emit(&self, _event: MiniChatAuditEvent) -> Result<(), AuditPluginError> {
        Err(AuditPluginError::Transient("sink down".to_owned()))
    }
}

fn fixture_with(client: Arc<dyn MiniChatAuditPluginClientV1>) -> Arc<ClientHub> {
    let registry = FakeTypesRegistry::new();
    let hub = Arc::new(ClientHub::new());
    hub.register::<dyn TypesRegistryClient>(Arc::clone(&registry) as Arc<dyn TypesRegistryClient>);
    let id = format!(
        "{}cf.core._.audit.v1",
        MiniChatAuditPluginSpecV1::gts_type_id()
    );
    registry.add_instance(&id, json!({ "id": id, "vendor": VENDOR, "priority": 100 }));
    hub.register_scoped::<dyn MiniChatAuditPluginClientV1>(ClientScope::gts_id(&id), client);
    hub
}

#[test]
fn emit_is_bounded_by_thirty_seconds() {
    assert_eq!(AUDIT_EMIT_TIMEOUT, Duration::from_secs(30));
}

#[tokio::test]
async fn emits_through_the_registered_plugin() {
    let hub = fixture_with(Arc::new(StaticAuditService::new(
        StaticAuditConfig::default(),
    )));

    AuditGateway::new(&hub, VENDOR)
        .emit(event())
        .await
        .expect("emitted");
}

#[tokio::test]
async fn no_registered_plugin_is_reported() {
    let registry = FakeTypesRegistry::new();
    let hub = Arc::new(ClientHub::new());
    hub.register::<dyn TypesRegistryClient>(registry as Arc<dyn TypesRegistryClient>);

    let err = AuditGateway::new(&hub, VENDOR)
        .emit(event())
        .await
        .expect_err("no plugin");

    assert_eq!(err, AuditGatewayError::NotRegistered);
}

#[tokio::test]
async fn a_plugin_error_is_passed_through() {
    let hub = fixture_with(Arc::new(FailingPlugin));

    let err = AuditGateway::new(&hub, VENDOR)
        .emit(event())
        .await
        .expect_err("plugin failure");

    assert_eq!(
        err,
        AuditGatewayError::Plugin(AuditPluginError::Transient("sink down".to_owned()))
    );
}

#[tokio::test]
async fn a_plugin_that_does_not_answer_in_time_is_a_plugin_timeout() {
    let hub = fixture_with(Arc::new(HangingPlugin));
    let gateway = AuditGateway::new(&hub, VENDOR).with_emit_timeout(Duration::from_millis(50));

    let err = tokio::time::timeout(Duration::from_secs(5), gateway.emit(event()))
        .await
        .expect("the gateway enforces its own bound")
        .expect_err("timeout");

    assert_eq!(
        err,
        AuditGatewayError::Plugin(AuditPluginError::PluginTimeout)
    );
}
