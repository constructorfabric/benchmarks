#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{Value, json};
use time::OffsetDateTime;
use uuid::Uuid;

use super::{
    BillingOutcome, EstimationBudgets, KillSwitches, ModelCatalogEntry, ModelTier, RequesterType,
    SettlementMethod, UsageEvent, UsageTerminalState, WebSearchContextSize,
};

fn entry_json() -> Value {
    json!({
        "id": "gpt-4.1",
        "provider_model_id": "gpt-4.1",
        "display_name": "GPT-4.1",
        "provider_id": "azure_openai",
        "provider_display_name": "Azure OpenAI",
        "tier": "Premium",
        "context_window": 1_047_576,
        "max_output_tokens": 32_768,
        "max_input_tokens": 0,
        "input_tokens_credit_multiplier_micro": 3_000_000,
        "output_tokens_credit_multiplier_micro": 15_000_000,
        "max_num_results": 5,
        "general_config": {
            "type": "",
            "available_from": "1970-01-01T00:00:00Z",
            "max_file_size_mb": 25,
            "api_params": { "stop": [] },
            "features": { "streaming": true, "structured_output": true },
            "tool_support": {
                "web_search": true,
                "file_search": true,
                "image_generation": false,
                "code_interpreter": true,
                "mcp": false
            },
            "supported_endpoints": { "responses": true }
        }
    })
}

fn parse_entry(value: Value) -> Result<ModelCatalogEntry, serde_json::Error> {
    serde_json::from_value(value)
}

#[test]
fn tier_accepts_both_capitalisations_and_serializes_in_lower_case() {
    for (text, tier) in [
        ("Premium", ModelTier::Premium),
        ("premium", ModelTier::Premium),
        ("Standard", ModelTier::Standard),
        ("standard", ModelTier::Standard),
    ] {
        assert_eq!(
            serde_json::from_value::<ModelTier>(json!(text)).unwrap(),
            tier
        );
    }
    assert_eq!(
        serde_json::to_value(ModelTier::Premium).unwrap(),
        json!("premium")
    );
    assert_eq!(ModelTier::Standard.as_str(), "standard");
    assert!(serde_json::from_value::<ModelTier>(json!("gold")).is_err());
}

#[test]
fn catalog_entry_defaults_apply_to_the_optional_fields() {
    let entry = parse_entry(entry_json()).unwrap();

    assert!(!entry.enabled, "`enabled` defaults to false");
    assert_eq!(entry.max_tool_calls, 2);
    assert_eq!(entry.web_search_context_size, WebSearchContextSize::Low);
    assert_eq!(entry.estimation_budgets, EstimationBudgets::default());
    assert!(entry.description.is_empty());
    assert!(entry.multimodal_capabilities.is_empty());
    assert!(entry.preference.is_none());
    assert!(!entry.is_default());
    assert!(!entry.general_config.supported_endpoints.chat_completions);
    assert!(entry.general_config.supported_endpoints.responses);
}

#[test]
fn catalog_entry_requires_its_mandatory_fields() {
    for field in [
        "id",
        "provider_model_id",
        "display_name",
        "provider_id",
        "provider_display_name",
        "tier",
        "context_window",
        "max_output_tokens",
        "max_input_tokens",
        "input_tokens_credit_multiplier_micro",
        "output_tokens_credit_multiplier_micro",
        "max_num_results",
        "general_config",
    ] {
        let mut value = entry_json();
        value.as_object_mut().unwrap().remove(field);
        assert!(parse_entry(value).is_err(), "`{field}` must be required");
    }
}

#[test]
fn nested_catalog_types_ignore_unknown_keys() {
    let mut value = entry_json();
    value["unknown"] = json!(1);
    value["estimation_budgets"] = json!({ "unknown": 1, "fixed_overhead_tokens": 7 });
    value["general_config"]["unknown"] = json!(true);

    let entry = parse_entry(value).unwrap();

    assert_eq!(entry.estimation_budgets.fixed_overhead_tokens, 7);
    assert_eq!(entry.estimation_budgets.bytes_per_token_conservative, 4);
}

#[test]
fn the_default_flag_comes_from_the_preference() {
    let mut value = entry_json();
    value["preference"] = json!({ "is_default": true, "sort_order": 3 });

    assert!(parse_entry(value).unwrap().is_default());
}

#[test]
fn kill_switches_require_every_field() {
    let complete = json!({
        "disable_premium_tier": false,
        "force_standard_tier": false,
        "disable_web_search": false,
        "disable_file_search": false,
        "disable_images": false,
        "disable_code_interpreter": false
    });
    assert!(serde_json::from_value::<KillSwitches>(complete.clone()).is_ok());

    for field in complete.as_object().unwrap().keys() {
        let mut partial = complete.clone();
        partial.as_object_mut().unwrap().remove(field);
        assert!(
            serde_json::from_value::<KillSwitches>(partial).is_err(),
            "`{field}` must be required"
        );
    }
}

fn system_usage_event() -> UsageEvent {
    UsageEvent {
        tenant_id: Uuid::new_v4(),
        user_id: None,
        chat_id: Uuid::new_v4(),
        turn_id: None,
        request_id: Uuid::new_v4(),
        effective_model: "m".to_owned(),
        selected_model: "m".to_owned(),
        terminal_state: UsageTerminalState::Completed,
        billing_outcome: BillingOutcome::SystemTask,
        usage: None,
        actual_credits_micro: 0,
        settlement_method: SettlementMethod::None,
        policy_version_applied: 0,
        web_search_calls: 0,
        code_interpreter_calls: 0,
        file_search_calls: 0,
        timestamp: OffsetDateTime::UNIX_EPOCH,
        requester_type: RequesterType::System,
        dedupe_key: "tenant/thread_summary_update/request".to_owned(),
        system_task_type: Some("thread_summary_update".to_owned()),
    }
}

#[test]
fn system_task_usage_events_omit_user_and_turn_and_round_trip() {
    let event = system_usage_event();

    let json = serde_json::to_value(&event).unwrap();

    let object = json.as_object().unwrap();
    assert!(!object.contains_key("user_id"));
    assert!(!object.contains_key("turn_id"));
    assert_eq!(json["usage"], Value::Null);
    assert_eq!(json["billing_outcome"], json!("system_task"));
    assert_eq!(json["settlement_method"], json!("none"));
    assert_eq!(json["requester_type"], json!("system"));
    assert_eq!(json["timestamp"], json!("1970-01-01T00:00:00Z"));
    assert_eq!(serde_json::from_value::<UsageEvent>(json).unwrap(), event);
}

#[test]
fn user_turn_usage_events_omit_the_system_task_type() {
    let mut event = system_usage_event();
    event.user_id = Some(Uuid::new_v4());
    event.turn_id = Some(Uuid::new_v4());
    event.system_task_type = None;
    event.requester_type = RequesterType::User;

    let json = serde_json::to_value(&event).unwrap();

    assert!(json["user_id"].is_string());
    assert!(json["turn_id"].is_string());
    assert!(!json.as_object().unwrap().contains_key("system_task_type"));
}
