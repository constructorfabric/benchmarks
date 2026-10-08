use super::models::{ModelCatalogEntry, ModelTier, PolicySnapshot, UsageEvent, UsageTokens, KillSwitches};
use time::OffsetDateTime;
use uuid::Uuid;

const ENTRY_YAML: &str = r#"
id: "gpt-4.1"
provider_model_id: "gpt-4.1"
display_name: "GPT-4.1"
description: "Most capable model"
provider_id: "azure_openai"
provider_display_name: "Azure OpenAI"
icon: ""
tier: Premium
enabled: true
multimodal_capabilities: ["VISION_INPUT"]
context_window: 1047576
max_output_tokens: 32768
max_input_tokens: 1047576
input_tokens_credit_multiplier_micro: 3000000
output_tokens_credit_multiplier_micro: 15000000
multiplier_display: "3x"
estimation_budgets:
  bytes_per_token_conservative: 4
  fixed_overhead_tokens: 100
  safety_margin_pct: 10
  image_token_budget: 1000
  tool_surcharge_tokens: 500
  web_search_surcharge_tokens: 500
  code_interpreter_surcharge_tokens: 1000
  minimal_generation_floor: 50
max_num_results: 5
web_search_context_size: low
max_tool_calls: 10
general_config:
  type: ""
  available_from: "1970-01-01T00:00:00Z"
  max_file_size_mb: 25
  api_params:
    temperature: 0.7
    stop: []
  features:
    streaming: true
  tool_support:
    web_search: true
    file_search: true
    code_interpreter: true
preference:
  is_default: true
  sort_order: 0
"#;

#[test]
fn catalog_entry_from_config_yaml() {
    let e: ModelCatalogEntry = serde_saphyr::from_str(ENTRY_YAML).unwrap();
    assert_eq!(e.tier, ModelTier::Premium);
    assert!(e.enabled);
    assert!(e.supports_vision());
    assert!(e.is_default());
    assert!(e.tool_support().web_search);
    assert_eq!(e.max_tool_calls, 10);
    assert_eq!(e.general_config.api_params.temperature, Some(0.7));
    assert_eq!(e.general_config.api_params.top_p, None);
}

#[test]
fn enabled_defaults_to_false_and_lowercase_tier() {
    let e: ModelCatalogEntry = serde_json::from_value(serde_json::json!({
        "id": "m", "provider_model_id": "m", "display_name": "M", "provider_id": "p",
        "tier": "standard", "context_window": 10, "max_output_tokens": 5,
        "input_tokens_credit_multiplier_micro": 1, "output_tokens_credit_multiplier_micro": 1
    }))
    .unwrap();
    assert!(!e.enabled);
    assert_eq!(e.tier, ModelTier::Standard);
    assert_eq!(e.estimation_budgets.bytes_per_token_conservative, 4);
    assert_eq!(e.web_search_context_size, "low");
}

#[test]
fn default_model_prefers_enabled_is_default() {
    let mut a: ModelCatalogEntry = serde_saphyr::from_str(ENTRY_YAML).unwrap();
    a.id = "a".to_owned();
    a.preference = None;
    let mut b = a.clone();
    b.id = "b".to_owned();
    b.preference = Some(super::models::ModelPreference { is_default: true, sort_order: 0 });
    let mut c = a.clone();
    c.id = "c".to_owned();
    c.enabled = false;
    let snap = PolicySnapshot { policy_version: 1, model_catalog: vec![c, a, b], kill_switches: KillSwitches::default() };
    assert_eq!(snap.default_model().unwrap().id, "b");
    assert!(snap.find_enabled("c").is_none());
}

#[test]
fn usage_event_user_turn_omits_system_fields() {
    let ev = UsageEvent {
        tenant_id: Uuid::nil(), user_id: Some(Uuid::nil()), chat_id: Uuid::nil(), turn_id: Some(Uuid::nil()),
        request_id: Uuid::nil(), effective_model: "m".into(), selected_model: "m".into(),
        terminal_state: "completed".into(), billing_outcome: "completed".into(),
        usage: Some(UsageTokens::default()), actual_credits_micro: 0, settlement_method: "actual".into(),
        policy_version_applied: 1, web_search_calls: 0, code_interpreter_calls: 0, file_search_calls: 0,
        timestamp: OffsetDateTime::UNIX_EPOCH, requester_type: "user".into(), dedupe_key: "k".into(),
        system_task_type: None,
    };
    let v = serde_json::to_value(&ev).unwrap();
    assert!(v.get("system_task_type").is_none());
    assert!(v.get("user_id").is_some());
}
