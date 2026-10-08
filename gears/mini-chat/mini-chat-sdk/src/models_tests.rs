use super::*;

fn entry_yaml_like() -> serde_json::Value {
    serde_json::json!({
        "id": "gpt-4.1",
        "provider_model_id": "gpt-4.1",
        "display_name": "GPT-4.1",
        "provider_id": "azure_openai",
        "tier": "Premium",
        "enabled": true,
        "context_window": 1000,
        "max_output_tokens": 100,
        "max_input_tokens": 900,
        "input_tokens_credit_multiplier_micro": 3_000_000,
        "output_tokens_credit_multiplier_micro": 15_000_000,
        "max_num_results": 5,
        "multimodal_capabilities": ["VISION_INPUT"],
        "general_config": {"type": "", "available_from": "1970-01-01T00:00:00Z", "max_file_size_mb": 25,
            "api_params": {"temperature": 0.7, "stop": []},
            "tool_support": {"web_search": true}},
        "preference": {"is_default": true, "sort_order": 0}
    })
}

#[test]
fn catalog_entry_accepts_capitalized_tier_and_defaults() {
    let e: ModelCatalogEntry = serde_json::from_value(entry_yaml_like()).unwrap();
    assert_eq!(e.tier, ModelTier::Premium);
    assert_eq!(e.max_tool_calls, 2);
    assert_eq!(e.web_search_context_size, WebSearchContextSize::Low);
    assert_eq!(e.estimation_budgets, EstimationBudgets::default());
    assert!(e.supports_vision());
    assert!(e.is_default());
    assert!(e.general_config.tool_support.web_search);
    assert_eq!(e.general_config.api_params.top_p, None);
}

#[test]
fn tier_serializes_lowercase() {
    assert_eq!(serde_json::to_value(ModelTier::Standard).unwrap(), "standard");
    let t: ModelTier = serde_json::from_value(serde_json::json!("standard")).unwrap();
    assert_eq!(t, ModelTier::Standard);
    assert!(serde_json::from_value::<ModelTier>(serde_json::json!("gold")).is_err());
}

#[test]
fn kill_switches_require_every_field() {
    let partial = serde_json::json!({"disable_premium_tier": true});
    assert!(serde_json::from_value::<KillSwitches>(partial).is_err());
}

#[test]
fn usage_event_omits_user_fields_for_system_tasks() {
    let ev = crate::usage::UsageEvent {
        tenant_id: Uuid::nil(),
        user_id: None,
        chat_id: Uuid::nil(),
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
        timestamp: time::OffsetDateTime::UNIX_EPOCH,
        requester_type: "system".into(),
        dedupe_key: "k".into(),
        system_task_type: Some("thread_summary_update".into()),
    };
    let v = serde_json::to_value(&ev).unwrap();
    assert!(v.get("user_id").is_none());
    assert!(v.get("turn_id").is_none());
    assert!(v.get("usage").unwrap().is_null());
    assert_eq!(v["system_task_type"], "thread_summary_update");
}

#[test]
fn dedupe_keys_use_simple_hex() {
    let t = Uuid::parse_str("f47ac10b-58cc-4372-a567-0e02b2c3d479").unwrap();
    let k = crate::usage::turn_dedupe_key(t, t, t);
    assert_eq!(k.split('/').count(), 3);
    assert!(!k.contains('-'));
    assert!(k.starts_with("f47ac10b58cc4372a5670e02b2c3d479/"));
}
