#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::json;
use uuid::Uuid;

use crate::events::{MiniChatAuditEvent, UsageEvent};
use crate::models::{ModelCatalogEntry, ModelTier, PolicySnapshot};

fn entry(id: &str, tier: &str, enabled: bool, is_default: bool) -> serde_json::Value {
    json!({
        "id": id, "provider_model_id": id, "display_name": id, "provider_id": "p",
        "tier": tier, "enabled": enabled, "context_window": 1000, "max_output_tokens": 100,
        "max_input_tokens": 0, "input_tokens_credit_multiplier_micro": 1,
        "output_tokens_credit_multiplier_micro": 1, "max_num_results": 5,
        "general_config": {}, "preference": {"is_default": is_default, "sort_order": 0}
    })
}

#[test]
fn tier_accepts_capitalized_and_lowercase() {
    let a: ModelCatalogEntry = serde_json::from_value(entry("a", "Premium", true, false)).unwrap();
    let b: ModelCatalogEntry = serde_json::from_value(entry("b", "standard", true, false)).unwrap();
    assert_eq!(a.tier, ModelTier::Premium);
    assert_eq!(b.tier, ModelTier::Standard);
    assert_eq!(a.max_tool_calls, 2);
    assert_eq!(a.estimation_budgets.bytes_per_token_conservative, 4);
}

#[test]
fn default_model_prefers_enabled_is_default_then_first_enabled() {
    let snap: PolicySnapshot = serde_json::from_value(json!({
        "policy_version": 1,
        "model_catalog": [entry("x", "premium", false, true), entry("y", "premium", true, false), entry("z", "standard", true, true)],
        "kill_switches": {"disable_premium_tier": false, "force_standard_tier": false, "disable_web_search": false,
            "disable_file_search": false, "disable_images": false, "disable_code_interpreter": false}
    })).unwrap();
    assert_eq!(snap.default_model().unwrap().id, "z");
    let snap2 = PolicySnapshot { model_catalog: snap.model_catalog[..2].to_vec(), ..snap.clone() };
    assert_eq!(snap2.default_model().unwrap().id, "y");
    assert!(snap.find_enabled("x").is_none());
    assert!(snap.find("x").is_some());
}

#[test]
fn kill_switches_require_every_field() {
    let r: Result<crate::models::KillSwitches, _> = serde_json::from_value(json!({"disable_images": true}));
    assert!(r.is_err());
}

#[test]
fn usage_event_omits_user_and_turn_for_system_tasks() {
    let ev = UsageEvent {
        tenant_id: Uuid::nil(), user_id: None, chat_id: Uuid::nil(), turn_id: None,
        request_id: Uuid::nil(), effective_model: "m".into(), selected_model: "m".into(),
        terminal_state: "completed".into(), billing_outcome: "system_task".into(), usage: None,
        actual_credits_micro: 0, settlement_method: "none".into(), policy_version_applied: 0,
        web_search_calls: 0, code_interpreter_calls: 0, file_search_calls: 0,
        timestamp: "t".into(), requester_type: "system".into(), dedupe_key: "k".into(),
        system_task_type: Some("thread_summary_update".into()),
    };
    let v = serde_json::to_value(&ev).unwrap();
    assert!(v.get("user_id").is_none());
    assert!(v.get("turn_id").is_none());
    assert!(v.get("usage").unwrap().is_null());
    assert_eq!(v["system_task_type"], "thread_summary_update");
}

#[test]
fn audit_event_untagged_roundtrip() {
    let m = json!({"event_type": "turn_delete", "tenant_id": Uuid::nil(), "actor_user_id": Uuid::nil(),
        "chat_id": Uuid::nil(), "request_id": Uuid::nil(), "timestamp": "t"});
    let ev: MiniChatAuditEvent = serde_json::from_value(m).unwrap();
    assert!(matches!(ev, MiniChatAuditEvent::Mutation(_)));
    assert_eq!(ev.event_type(), "turn_delete");
}
