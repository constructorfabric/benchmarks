#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use mini_chat_sdk::{
    AuditPluginError, MiniChatAuditEvent, MiniChatAuditPluginClientV1, MiniChatAuditPluginSpecV1,
    TurnMutationAuditEvent, TurnMutationAuditEventType,
};
use toolkit::client_hub::{ClientHub, ClientScope};
use types_registry_sdk::testing::MockTypesRegistryClient;
use uuid::Uuid;

use super::*;
use crate::domain::ports::{AuditPort, AuditResolution};
use crate::infra::gateways::test_helpers::{hub_with_registry, plugin_instance};

#[derive(Default)]
struct FakeAudit {
    events: Mutex<Vec<MiniChatAuditEvent>>,
    fail: Option<AuditPluginError>,
}

#[async_trait]
impl MiniChatAuditPluginClientV1 for FakeAudit {
    async fn emit(&self, event: MiniChatAuditEvent) -> Result<(), AuditPluginError> {
        if let Some(e) = &self.fail {
            return Err(e.clone());
        }
        self.events.lock().unwrap().push(event);
        Ok(())
    }
}

fn event() -> MiniChatAuditEvent {
    MiniChatAuditEvent::Mutation(TurnMutationAuditEvent {
        event_type: TurnMutationAuditEventType::TurnDelete,
        actor_user_id: Uuid::new_v4(),
        tenant_id: Uuid::new_v4(),
        chat_id: Uuid::new_v4(),
        original_request_id: None,
        new_request_id: None,
        request_id: Some(Uuid::new_v4()),
        timestamp: time::OffsetDateTime::UNIX_EPOCH,
    })
}

const SEGMENT: &str = "cf.test.audit_fake.plugin.v1";

fn registry_with_plugin(vendor: &str) -> (String, Arc<MockTypesRegistryClient>) {
    let (id, inst) = plugin_instance::<MiniChatAuditPluginSpecV1>(SEGMENT, vendor, 100);
    (
        id,
        Arc::new(MockTypesRegistryClient::new().with_instances([inst])),
    )
}

fn register_client(hub: &ClientHub, id: &str, client: Arc<FakeAudit>) {
    let api: Arc<dyn MiniChatAuditPluginClientV1> = client;
    hub.register_scoped::<dyn MiniChatAuditPluginClientV1>(ClientScope::gts_id(id), api);
}

#[tokio::test]
async fn no_plugin_is_not_cached() {
    let registry = Arc::new(MockTypesRegistryClient::new());
    let gw = AuditGateway::new(hub_with_registry(&registry), "constructorfabric");
    assert!(matches!(
        gw.emit(event()).await,
        Ok(AuditResolution::NoPlugin)
    ));
    assert!(matches!(
        gw.emit(event()).await,
        Ok(AuditResolution::NoPlugin)
    ));
    assert_eq!(
        registry.list_instance_calls(),
        2,
        "no-plugin must not be cached"
    );
}

#[tokio::test]
async fn vendor_mismatch_is_no_plugin() {
    let (_id, registry) = registry_with_plugin("other-vendor");
    let gw = AuditGateway::new(hub_with_registry(&registry), "constructorfabric");
    assert!(matches!(
        gw.emit(event()).await,
        Ok(AuditResolution::NoPlugin)
    ));
}

#[tokio::test]
async fn delivers_and_caches_found_instance() {
    let (id, registry) = registry_with_plugin("constructorfabric");
    let hub = hub_with_registry(&registry);
    let fake = Arc::new(FakeAudit::default());
    register_client(&hub, &id, fake.clone());
    let gw = AuditGateway::new(hub, "constructorfabric");
    assert!(matches!(
        gw.emit(event()).await,
        Ok(AuditResolution::Delivered)
    ));
    assert!(matches!(
        gw.emit(event()).await,
        Ok(AuditResolution::Delivered)
    ));
    assert_eq!(fake.events.lock().unwrap().len(), 2);
    assert_eq!(
        registry.list_instance_calls(),
        1,
        "found instance id is cached"
    );
}

#[tokio::test]
async fn missing_client_resets_cache_and_is_transient() {
    let (id, registry) = registry_with_plugin("constructorfabric");
    let hub = hub_with_registry(&registry);
    let gw = AuditGateway::new(hub.clone(), "constructorfabric");
    assert!(matches!(
        gw.emit(event()).await,
        Err(AuditPluginError::Transient(_))
    ));
    assert!(matches!(
        gw.emit(event()).await,
        Err(AuditPluginError::Transient(_))
    ));
    assert_eq!(
        registry.list_instance_calls(),
        2,
        "cache reset after missing client"
    );

    let fake = Arc::new(FakeAudit::default());
    register_client(&hub, &id, fake.clone());
    assert!(matches!(
        gw.emit(event()).await,
        Ok(AuditResolution::Delivered)
    ));
    assert_eq!(fake.events.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn registry_unavailable_is_transient() {
    let gw = AuditGateway::new(Arc::new(ClientHub::new()), "constructorfabric");
    assert!(matches!(
        gw.emit(event()).await,
        Err(AuditPluginError::Transient(_))
    ));
}

#[tokio::test]
async fn registry_list_error_is_transient() {
    let registry = Arc::new(
        MockTypesRegistryClient::new()
            .with_list_error(types_registry_sdk::testing::internal("boom")),
    );
    let gw = AuditGateway::new(hub_with_registry(&registry), "constructorfabric");
    assert!(matches!(
        gw.emit(event()).await,
        Err(AuditPluginError::Transient(_))
    ));
}

#[tokio::test]
async fn plugin_error_passes_through() {
    let (id, registry) = registry_with_plugin("constructorfabric");
    let hub = hub_with_registry(&registry);
    let fake = Arc::new(FakeAudit {
        events: Mutex::default(),
        fail: Some(AuditPluginError::Permanent("bad".to_owned())),
    });
    register_client(&hub, &id, fake);
    let gw = AuditGateway::new(hub, "constructorfabric");
    assert_eq!(
        gw.emit(event()).await.unwrap_err(),
        AuditPluginError::Permanent("bad".to_owned())
    );
}
