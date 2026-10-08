use serde_json::json;

use crate::models::{KillSwitches, ModelCatalogEntry, ModelTier, PolicySnapshot, UsageEvent};

#[test]
fn catalog_entry_parses_operator_yaml_shape_and_defaults() {
    let entry: ModelCatalogEntry = serde_json::from_value(json!({
        "id": "gpt-4.1",
        "provider_model_id": "gpt-4.1",
        "tier": "Premium",
        "enabled": true,
        "multimodal_capabilities": ["VISION_INPUT"],
        "unknown_key": "ignored",
        "general_config": {"type": "", "tool_support": {"web_search": true}},
        "preference": {"is_default": true, "sort_order": 0}
    }))
    .expect("parse entry");
    assert_eq!(entry.tier, ModelTier::Premium);
    assert!(entry.supports_vision());
    assert!(entry.is_default());
    assert!(entry.tool_support().web_search);
    assert!(!entry.tool_support().file_search);
    assert_eq!(entry.max_tool_calls, 2);
    assert_eq!(entry.estimation_budgets.bytes_per_token_conservative, 4);
    assert_eq!(entry.estimation_budgets.code_interpreter_surcharge_tokens, 1000);
}

#[test]
fn catalog_entry_enabled_defaults_to_false() {
    let entry: ModelCatalogEntry =
        serde_json::from_value(json!({"id": "m", "tier": "standard"})).expect("parse");
    assert!(!entry.enabled);
}

#[test]
fn kill_switches_require_every_field() {
    let err = serde_json::from_value::<KillSwitches>(json!({"disable_web_search": true}));
    assert!(err.is_err(), "missing switches must fail to deserialize");
}

fn entry(id: &str, enabled: bool, default: bool) -> ModelCatalogEntry {
    serde_json::from_value(json!({
        "id": id, "tier": "standard", "enabled": enabled,
        "preference": {"is_default": default, "sort_order": 0}
    }))
    .expect("entry")
}

#[test]
fn default_model_algorithm() {
    let snap = PolicySnapshot {
        policy_version: 1,
        model_catalog: vec![entry("a", false, true), entry("b", true, false), entry("c", true, true)],
        kill_switches: KillSwitches::default(),
    };
    assert_eq!(snap.default_model().map(|m| m.id.as_str()), Some("c"));
    let snap = PolicySnapshot {
        policy_version: 1,
        model_catalog: vec![entry("a", false, true), entry("b", true, false)],
        kill_switches: KillSwitches::default(),
    };
    assert_eq!(snap.default_model().map(|m| m.id.as_str()), Some("b"));
    let snap = PolicySnapshot {
        policy_version: 1,
        model_catalog: vec![entry("a", false, true)],
        kill_switches: KillSwitches::default(),
    };
    assert!(snap.default_model().is_none());
}

#[test]
fn usage_event_omits_absent_user_and_turn() {
    let ev = UsageEvent {
        tenant_id: uuid::Uuid::nil(),
        user_id: None,
        chat_id: uuid::Uuid::nil(),
        turn_id: None,
        request_id: uuid::Uuid::nil(),
        effective_model: "m".into(),
        selected_model: "m".into(),
        terminal_state: "completed".into(),
        billing_outcome: "system_task".into(),
        usage: None,
        actual_credits_micro: 0,
        settlement_method: "none".into(),
        policy_version_applied: 0,
        web_search_calls: 0,
        code_interpreter_calls: 0,
        file_search_calls: 0,
        timestamp: time::OffsetDateTime::UNIX_EPOCH,
        requester_type: "system".into(),
        dedupe_key: "k".into(),
        system_task_type: Some("thread_summary_update".into()),
    };
    let v = serde_json::to_value(&ev).expect("ser");
    assert!(v.get("user_id").is_none());
    assert!(v.get("turn_id").is_none());
    assert_eq!(v["system_task_type"], "thread_summary_update");
}
