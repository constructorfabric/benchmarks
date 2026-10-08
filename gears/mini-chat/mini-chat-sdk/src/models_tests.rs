use super::*;

const CATALOG_YAML: &str = r#"
- id: "gpt-4.1"
  provider_model_id: "gpt-4.1"
  display_name: "GPT-4.1"
  description: "Most capable model"
  provider_id: "azure_openai"
  provider_display_name: "Azure OpenAI"
  icon: ""
  tier: Premium
  enabled: true
  system_prompt: "You are a helpful assistant."
  thread_summary_prompt: ""
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
      top_p: 1.0
      frequency_penalty: 0.0
      presence_penalty: 0.0
      stop: []
    features:
      streaming: true
      structured_output: true
    tool_support:
      web_search: true
      file_search: true
      image_generation: false
      code_interpreter: true
      mcp: false
    supported_endpoints:
      chat_completions: true
      responses: true
      embeddings: false
      image_generation: false
      audio_speech_generation: false
      audio_transcription: false
      audio_translation: false
  preference:
    is_default: true
    sort_order: 0
- id: "minimal"
  provider_model_id: "m"
  display_name: "Minimal"
  provider_id: "p"
  tier: standard
  context_window: 4096
  max_output_tokens: 1024
  input_tokens_credit_multiplier_micro: 1
  output_tokens_credit_multiplier_micro: 1
"#;

#[test]
fn catalog_parses_operator_yaml_shape() {
    let entries: Vec<ModelCatalogEntry> =
        serde_saphyr::from_str(CATALOG_YAML).expect("catalog yaml parses");
    assert_eq!(entries.len(), 2);
    let premium = &entries[0];
    assert_eq!(premium.tier, ModelTier::Premium);
    assert!(premium.enabled);
    assert!(premium.supports_vision());
    assert!(premium.is_default());
    assert_eq!(premium.max_tool_calls, 10);
    assert!(premium.general_config.tool_support.web_search);
    assert_eq!(premium.general_config.api_params.temperature, Some(0.7));

    let minimal = &entries[1];
    assert_eq!(minimal.tier, ModelTier::Standard);
    assert!(!minimal.enabled, "enabled defaults to false");
    assert_eq!(minimal.max_tool_calls, 2, "max_tool_calls defaults to 2");
    assert_eq!(minimal.web_search_context_size, WebSearchContextSize::Low);
    assert_eq!(minimal.estimation_budgets, EstimationBudgets::default());
    assert!(!minimal.supports_vision());
    assert!(!minimal.is_default());
}

#[test]
fn tier_serializes_lowercase() {
    assert_eq!(
        serde_json::to_string(&ModelTier::Premium).expect("ser"),
        "\"premium\""
    );
    assert_eq!(ModelTier::Standard.as_str(), "standard");
}

#[test]
fn usage_event_omits_absent_user_and_turn() {
    let ev = UsageEvent {
        tenant_id: Uuid::nil(),
        user_id: None,
        chat_id: Uuid::nil(),
        turn_id: None,
        request_id: Uuid::nil(),
        effective_model: "m".to_owned(),
        selected_model: "m".to_owned(),
        terminal_state: "completed".to_owned(),
        billing_outcome: "system_task".to_owned(),
        usage: None,
        actual_credits_micro: 0,
        settlement_method: "none".to_owned(),
        policy_version_applied: 0,
        web_search_calls: 0,
        code_interpreter_calls: 0,
        file_search_calls: 0,
        timestamp: OffsetDateTime::UNIX_EPOCH,
        requester_type: "system".to_owned(),
        dedupe_key: "k".to_owned(),
        system_task_type: Some("thread_summary_update".to_owned()),
    };
    let v = serde_json::to_value(&ev).expect("ser");
    assert!(v.get("user_id").is_none());
    assert!(v.get("turn_id").is_none());
    assert!(v["usage"].is_null());
    let back: UsageEvent = serde_json::from_value(v).expect("de");
    assert_eq!(back, ev);
}

#[test]
fn audit_event_round_trips() {
    let ev = AuditEvent::TurnMutation(TurnMutationAuditEvent {
        event_type: "turn_retry".to_owned(),
        timestamp: OffsetDateTime::UNIX_EPOCH,
        tenant_id: Uuid::nil(),
        actor_user_id: Uuid::nil(),
        chat_id: Uuid::nil(),
        original_request_id: Some(Uuid::nil()),
        new_request_id: Some(Uuid::nil()),
        request_id: None,
    });
    let s = serde_json::to_string(&ev).expect("ser");
    let back: AuditEvent = serde_json::from_str(&s).expect("de");
    assert_eq!(back.event_type(), "turn_retry");
}
