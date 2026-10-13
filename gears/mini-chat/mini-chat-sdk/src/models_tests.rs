#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use serde_json::json;

fn minimal_entry() -> serde_json::Value {
    json!({
        "id": "m1",
        "provider_model_id": "gpt-x",
        "display_name": "M1",
        "provider_id": "openai",
        "provider_display_name": "OpenAI",
        "tier": "Premium",
        "context_window": 1000,
        "max_output_tokens": 100,
        "max_input_tokens": 0,
        "input_tokens_credit_multiplier_micro": 1_000_000,
        "output_tokens_credit_multiplier_micro": 2_000_000,
        "max_num_results": 5,
        "general_config": { "type": "", "available_from": "1970-01-01T00:00:00Z", "max_file_size_mb": 25,
            "api_params": { "stop": [] }, "features": {}, "tool_support": { "web_search": true }, "supported_endpoints": {} }
    })
}

#[test]
fn catalog_entry_defaults() {
    let e: ModelCatalogEntry = serde_json::from_value(minimal_entry()).unwrap();
    assert_eq!(e.tier, ModelTier::Premium);
    assert!(!e.enabled, "enabled defaults to false");
    assert_eq!(e.max_tool_calls, 2);
    assert_eq!(e.web_search_context_size, WebSearchContextSize::Low);
    assert_eq!(e.estimation_budgets, EstimationBudgets::default());
    assert!(e.preference.is_none());
    assert!(e.tool_support().web_search);
    assert!(!e.supports_vision());
}

#[test]
fn catalog_entry_requires_multipliers() {
    let mut v = minimal_entry();
    v.as_object_mut()
        .unwrap()
        .remove("input_tokens_credit_multiplier_micro");
    assert!(serde_json::from_value::<ModelCatalogEntry>(v).is_err());
}

#[test]
fn partial_estimation_budgets_take_defaults() {
    let mut v = minimal_entry();
    v["estimation_budgets"] = json!({ "bytes_per_token_conservative": 3 });
    let e: ModelCatalogEntry = serde_json::from_value(v).unwrap();
    assert_eq!(e.estimation_budgets.bytes_per_token_conservative, 3);
    assert_eq!(e.estimation_budgets.fixed_overhead_tokens, 100);
}

#[test]
fn kill_switches_require_every_field() {
    assert!(serde_json::from_value::<KillSwitches>(json!({"disable_web_search": true})).is_err());
    let ks: KillSwitches = serde_json::from_value(json!({
        "disable_premium_tier": false, "force_standard_tier": false, "disable_web_search": true,
        "disable_file_search": false, "disable_images": false, "disable_code_interpreter": false
    }))
    .unwrap();
    assert!(ks.disable_web_search);
}

#[test]
fn default_model_algorithm() {
    let mut a: ModelCatalogEntry = serde_json::from_value(minimal_entry()).unwrap();
    a.id = "a".into();
    a.enabled = true;
    let mut b = a.clone();
    b.id = "b".into();
    b.preference = Some(ModelPreference {
        is_default: true,
        sort_order: 1,
    });
    let mut c = a.clone();
    c.id = "c".into();
    c.enabled = false;
    c.preference = Some(ModelPreference {
        is_default: true,
        sort_order: 0,
    });
    let snap = PolicySnapshot {
        policy_version: 1,
        model_catalog: vec![c, a, b],
        kill_switches: KillSwitches::default(),
    };
    assert_eq!(snap.default_model().unwrap().id, "b");
    let mut snap2 = snap;
    snap2.model_catalog.retain(|m| m.id != "b");
    assert_eq!(snap2.default_model().unwrap().id, "a");
}

#[test]
fn usage_event_system_task_omits_user_and_turn() {
    let ev = UsageEvent {
        tenant_id: Uuid::nil(),
        user_id: None,
        chat_id: Some(Uuid::nil()),
        turn_id: None,
        request_id: Uuid::nil(),
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
        timestamp: OffsetDateTime::UNIX_EPOCH,
        requester_type: "system".into(),
        dedupe_key: "k".into(),
        system_task_type: Some("thread_summary_update".into()),
    };
    let v = serde_json::to_value(&ev).unwrap();
    assert!(v.get("user_id").is_none());
    assert!(v.get("turn_id").is_none());
    assert!(v["usage"].is_null());
    assert_eq!(v["timestamp"], "1970-01-01T00:00:00Z");
    let back: UsageEvent = serde_json::from_value(v).unwrap();
    assert_eq!(back, ev);
}

#[test]
fn audit_envelope_roundtrip() {
    let ev = AuditEvent::Mutation(TurnMutationAuditEvent {
        event_type: "turn_delete".into(),
        tenant_id: Uuid::nil(),
        actor_user_id: Uuid::nil(),
        chat_id: Uuid::nil(),
        original_request_id: None,
        new_request_id: None,
        request_id: Some(Uuid::nil()),
        timestamp: OffsetDateTime::UNIX_EPOCH,
    });
    let s = serde_json::to_string(&ev).unwrap();
    assert!(s.contains("\"kind\":\"mutation\""));
    assert_eq!(serde_json::from_str::<AuditEvent>(&s).unwrap(), ev);
}
