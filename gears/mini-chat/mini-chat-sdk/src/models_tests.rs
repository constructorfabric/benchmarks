use super::*;
use crate::usage::{UsageEvent, UsageTokens};
use serde_json::json;

fn catalog_entry_json() -> serde_json::Value {
    // Shape of an entry in config/mini-chat.yaml (tier written as `Premium`).
    json!({
        "id": "gpt-4.1",
        "provider_model_id": "gpt-4.1",
        "display_name": "GPT-4.1",
        "description": "Most capable model",
        "provider_id": "azure_openai",
        "provider_display_name": "Azure OpenAI",
        "icon": "",
        "tier": "Premium",
        "enabled": true,
        "system_prompt": "You are helpful.",
        "thread_summary_prompt": "",
        "multimodal_capabilities": ["VISION_INPUT"],
        "context_window": 1_047_576,
        "max_output_tokens": 32768,
        "max_input_tokens": 1_047_576,
        "input_tokens_credit_multiplier_micro": 3_000_000,
        "output_tokens_credit_multiplier_micro": 15_000_000,
        "multiplier_display": "3x",
        "estimation_budgets": {"bytes_per_token_conservative": 4, "fixed_overhead_tokens": 100},
        "max_num_results": 5,
        "web_search_context_size": "low",
        "max_tool_calls": 10,
        "general_config": {
            "type": "",
            "available_from": "1970-01-01T00:00:00Z",
            "max_file_size_mb": 25,
            "api_params": {"temperature": 0.7, "top_p": 1.0, "stop": []},
            "features": {"streaming": true, "structured_output": true},
            "tool_support": {"web_search": true, "file_search": true, "image_generation": false,
                             "code_interpreter": true, "mcp": false},
            "supported_endpoints": {"chat_completions": true, "responses": true}
        },
        "preference": {"is_default": true, "sort_order": 0}
    })
}

#[test]
fn catalog_entry_parses_config_shape() {
    let entry: ModelCatalogEntry = serde_json::from_value(catalog_entry_json()).unwrap();
    assert_eq!(entry.tier, ModelTier::Premium);
    assert!(entry.supports_vision());
    assert!(entry.is_default());
    assert_eq!(entry.max_tool_calls, 10);
    assert_eq!(entry.estimation_budgets.safety_margin_pct, 10, "missing budget fields default");
    assert!(entry.general_config.tool_support.web_search);
    assert_eq!(entry.general_config.api_params.temperature, Some(0.7));
    assert_eq!(entry.general_config.api_params.presence_penalty, None);
}

#[test]
fn catalog_entry_defaults() {
    let entry: ModelCatalogEntry = serde_json::from_value(json!({
        "id": "m", "provider_model_id": "m", "display_name": "M", "provider_id": "p", "tier": "standard"
    }))
    .unwrap();
    assert!(!entry.enabled, "enabled defaults to false");
    assert_eq!(entry.max_tool_calls, 2);
    assert_eq!(entry.web_search_context_size, WebSearchContextSize::Low);
    assert!(entry.preference.is_none());
    assert_eq!(serde_json::to_value(entry.tier).unwrap(), json!("standard"));
}

#[test]
fn unknown_tier_is_rejected() {
    let err = serde_json::from_value::<ModelTier>(json!("gold")).unwrap_err();
    assert!(err.to_string().contains("unknown model tier"));
}

#[test]
fn kill_switches_require_every_field() {
    assert!(serde_json::from_value::<KillSwitches>(json!({"disable_images": true})).is_err());
    let ks: KillSwitches = serde_json::from_value(json!({
        "disable_premium_tier": false, "force_standard_tier": false, "disable_web_search": true,
        "disable_file_search": false, "disable_images": false, "disable_code_interpreter": false
    }))
    .unwrap();
    assert!(ks.disable_web_search);
}

#[test]
fn default_model_algorithm() {
    let mut a: ModelCatalogEntry = serde_json::from_value(catalog_entry_json()).unwrap();
    a.id = "a".into();
    a.preference = None;
    let mut b = a.clone();
    b.id = "b".into();
    b.preference = Some(ModelPreference { is_default: true, sort_order: 1 });
    let mut c = a.clone();
    c.id = "c".into();
    c.enabled = false;
    let snapshot = PolicySnapshot {
        policy_version: 1,
        model_catalog: vec![c.clone(), a.clone(), b],
        kill_switches: KillSwitches::default(),
    };
    assert_eq!(snapshot.default_model().unwrap().id, "b");
    let no_default = PolicySnapshot { model_catalog: vec![c.clone(), a], ..snapshot };
    assert_eq!(no_default.default_model().unwrap().id, "a");
    let none_enabled = PolicySnapshot { model_catalog: vec![c], ..snapshot };
    assert!(none_enabled.default_model().is_none());
    assert!(snapshot.enabled_model("c").is_none());
    assert!(snapshot.model("c").is_some());
}

#[test]
fn usage_event_wire_shape() {
    let tenant = Uuid::new_v4();
    let event = UsageEvent {
        tenant_id: tenant,
        user_id: None,
        chat_id: Uuid::new_v4(),
        turn_id: None,
        request_id: Uuid::new_v4(),
        effective_model: "gpt-4.1-mini".into(),
        selected_model: "gpt-4.1-mini".into(),
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
    let v = serde_json::to_value(&event).unwrap();
    assert!(v.get("user_id").is_none(), "user_id absent for system tasks");
    assert!(v.get("turn_id").is_none(), "turn_id absent for system tasks");
    assert_eq!(v["usage"], serde_json::Value::Null);
    assert_eq!(v["system_task_type"], json!("thread_summary_update"));
    assert_eq!(v["timestamp"], json!("1970-01-01T00:00:00Z"));
    let back: UsageEvent = serde_json::from_value(v).unwrap();
    assert_eq!(back, event);

    let user_event = UsageEvent {
        user_id: Some(Uuid::nil()),
        turn_id: Some(Uuid::nil()),
        usage: Some(UsageTokens { input_tokens: 1, ..UsageTokens::default() }),
        system_task_type: None,
        ..event
    };
    let v = serde_json::to_value(&user_event).unwrap();
    assert!(v.get("system_task_type").is_none());
    assert_eq!(v["usage"]["input_tokens"], json!(1));
}

#[test]
fn gts_spec_ids() {
    use crate::gts::{MiniChatAuditPluginSpecV1, MiniChatModelPolicyPluginSpecV1};
    assert_eq!(
        MiniChatModelPolicyPluginSpecV1::gts_type_id().to_string(),
        "gts.cf.toolkit.plugins.plugin.v1~cf.core.mini_chat_model_policy.plugin.v1~"
    );
    assert_eq!(
        MiniChatAuditPluginSpecV1::gts_type_id().to_string(),
        "gts.cf.toolkit.plugins.plugin.v1~cf.core.mini_chat_audit.plugin.v1~"
    );
}
