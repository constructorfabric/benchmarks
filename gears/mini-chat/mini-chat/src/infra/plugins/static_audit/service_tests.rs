#![allow(clippy::unwrap_used, clippy::expect_used)]

use mini_chat_sdk::{MiniChatAuditEvent, MiniChatAuditPluginClientV1, TurnMutationAuditEvent};
use serde_json::json;
use uuid::Uuid;

use super::config::StaticAuditConfig;
use super::service::StaticAuditService;

fn event() -> MiniChatAuditEvent {
    MiniChatAuditEvent::Mutation(TurnMutationAuditEvent::Delete {
        tenant_id: Uuid::nil(),
        actor_user_id: Uuid::nil(),
        chat_id: Uuid::nil(),
        request_id: Uuid::nil(),
        timestamp: "2026-10-04T00:00:00Z".to_owned(),
    })
}

#[test]
fn static_audit_config_defaults_and_unknown_key_rejected() {
    let cfg = StaticAuditConfig::default();
    assert!(cfg.enabled);
    assert_eq!(cfg.vendor, "constructorfabric");
    assert_eq!(cfg.priority, 100);

    let err = serde_json::from_value::<StaticAuditConfig>(json!({ "enable": false })).unwrap_err();
    assert!(err.to_string().contains("unknown field"), "{err}");
}

#[tokio::test]
async fn static_audit_enabled_emit_ok() {
    let svc = StaticAuditService::from_config(&StaticAuditConfig::default());
    svc.emit(event()).await.unwrap();
}

#[tokio::test]
async fn static_audit_disabled_still_ok() {
    let cfg: StaticAuditConfig = serde_json::from_value(json!({ "enabled": false })).unwrap();
    let svc = StaticAuditService::from_config(&cfg);
    svc.emit(event()).await.unwrap();
}
