#![allow(clippy::unwrap_used, clippy::expect_used)]

use mini_chat_sdk::{
    MiniChatAuditEvent, MiniChatAuditPluginClientV1, PolicyDecisions, QuotaPolicyDecision,
    ToolCalls, TurnAuditEvent,
};
use time::OffsetDateTime;
use uuid::Uuid;

use super::config::StaticAuditPluginConfig;
use super::service::StaticAuditService;

fn event() -> MiniChatAuditEvent {
    MiniChatAuditEvent::Turn(TurnAuditEvent {
        event_type: "turn_completed".to_owned(),
        tenant_id: Uuid::new_v4(),
        chat_id: Uuid::new_v4(),
        turn_id: Uuid::new_v4(),
        request_id: Uuid::new_v4(),
        requester_type: "user".to_owned(),
        actor_user_id: None,
        selected_model: "m".to_owned(),
        effective_model: "m".to_owned(),
        terminal_state: "completed".to_owned(),
        error_code: None,
        usage: None,
        latency_ms: None,
        tool_calls: ToolCalls::default(),
        policy_decisions: PolicyDecisions {
            quota: QuotaPolicyDecision {
                decision: "allow".to_owned(),
                downgrade_from: None,
                downgrade_reason: None,
            },
            license: None,
            quota_scope: None,
        },
        prompt: None,
        response: None,
        attachments: vec![],
        trace_id: None,
        timestamp: OffsetDateTime::now_utc(),
    })
}

#[test]
fn defaults_are_enabled_constructorfabric_100() {
    let cfg = StaticAuditPluginConfig::default();
    assert_eq!(cfg.vendor, "constructorfabric");
    assert_eq!(cfg.priority, 100);
    assert!(cfg.enabled);
}

#[test]
fn config_rejects_unknown_key() {
    assert!(
        serde_json::from_value::<StaticAuditPluginConfig>(serde_json::json!({"bogus": 1})).is_err()
    );
}

#[tokio::test]
async fn disabled_plugin_accepts_and_drops() {
    let cfg = StaticAuditPluginConfig {
        enabled: false,
        ..StaticAuditPluginConfig::default()
    };
    let svc = StaticAuditService::from_config(&cfg);
    assert!(!svc.is_enabled());
    svc.emit(event()).await.unwrap();
}

#[tokio::test]
async fn enabled_plugin_accepts() {
    let svc = StaticAuditService::from_config(&StaticAuditPluginConfig::default());
    assert!(svc.is_enabled());
    svc.emit(event()).await.unwrap();
}
