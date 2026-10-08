#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::unreadable_literal,
    clippy::cognitive_complexity
)]

use serde_json::json;
use uuid::Uuid;

use super::*;

fn dev_yaml_catalog() -> Vec<serde_json::Value> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../config/mini-chat.yaml"
    );
    let text = std::fs::read_to_string(path).expect("read config/mini-chat.yaml");
    let root: serde_json::Value = serde_saphyr::from_str(&text).expect("parse yaml");
    root["gears"]["static-mini-chat-model-policy-plugin"]["config"]["model_catalog"]
        .as_array()
        .expect("model_catalog array")
        .clone()
}

fn minimal_entry() -> serde_json::Value {
    json!({
        "id": "m1",
        "provider_model_id": "m1-prov",
        "display_name": "M1",
        "provider_id": "openai",
        "provider_display_name": "OpenAI",
        "tier": "standard",
        "context_window": 128000,
        "max_output_tokens": 4096,
        "max_input_tokens": 120000,
        "input_tokens_credit_multiplier_micro": 1000000,
        "output_tokens_credit_multiplier_micro": 3000000,
        "max_num_results": 5,
        "general_config": {
            "type": "model.general.v1",
            "available_from": "2026-01-01T00:00:00Z",
            "max_file_size_mb": 25,
            "api_params": { "stop": [] },
            "features": { "streaming": true, "structured_output": false },
            "tool_support": {
                "web_search": true, "file_search": true, "image_generation": false,
                "code_interpreter": false, "mcp": false
            },
            "supported_endpoints": {
                "chat_completions": false, "responses": true, "embeddings": false,
                "image_generation": false, "audio_speech_generation": false,
                "audio_transcription": false, "audio_translation": false
            }
        }
    })
}

#[test]
fn catalog_entry_from_dev_yaml() {
    let entries = dev_yaml_catalog();
    assert!(!entries.is_empty());
    let parsed: Vec<ModelCatalogEntry> = entries
        .into_iter()
        .map(|v| serde_json::from_value(v).expect("catalog entry deserializes"))
        .collect();
    let gpt41 = parsed.iter().find(|e| e.id == "gpt-4.1").expect("gpt-4.1");
    assert_eq!(gpt41.tier, ModelTier::Premium);
    assert_eq!(gpt41.estimation_budgets.bytes_per_token_conservative, 4);
    assert!(gpt41.general_config.tool_support.code_interpreter);
    assert!(gpt41.enabled);
    assert_eq!(gpt41.max_tool_calls, 10);
    assert_eq!(
        gpt41.preference,
        Some(ModelPreference {
            is_default: true,
            sort_order: 0
        })
    );
    let mini = parsed
        .iter()
        .find(|e| e.id == "gpt-4.1-mini")
        .expect("gpt-4.1-mini");
    assert_eq!(mini.tier, ModelTier::Standard);
}

#[test]
fn tier_accepts_both_casings() {
    for raw in ["\"premium\"", "\"Premium\""] {
        let t: ModelTier = serde_json::from_str(raw).unwrap();
        assert_eq!(t, ModelTier::Premium);
    }
    for raw in ["\"standard\"", "\"Standard\""] {
        let t: ModelTier = serde_json::from_str(raw).unwrap();
        assert_eq!(t, ModelTier::Standard);
    }
    assert_eq!(
        serde_json::to_string(&ModelTier::Premium).unwrap(),
        "\"premium\""
    );
    assert!(serde_json::from_str::<ModelTier>("\"gold\"").is_err());
}

#[test]
fn catalog_entry_defaults() {
    let e: ModelCatalogEntry = serde_json::from_value(minimal_entry()).unwrap();
    assert!(!e.enabled);
    assert_eq!(e.max_tool_calls, 2);
    assert_eq!(e.web_search_context_size, "low");
    assert_eq!(e.description, "");
    assert_eq!(e.icon, "");
    assert_eq!(e.multiplier_display, "");
    assert_eq!(e.system_prompt, "");
    assert_eq!(e.thread_summary_prompt, "");
    assert!(e.multimodal_capabilities.is_empty());
    assert_eq!(e.preference, None);
    assert_eq!(e.general_config.r#type, "model.general.v1");
    assert_eq!(e.general_config.api_params.temperature, None);
    assert_eq!(e.general_config.api_params.extra_body, None);
    assert_eq!(e.general_config.api_params.reasoning_effort, None);
    let b = e.estimation_budgets;
    assert_eq!(b, EstimationBudgets::default());
    assert_eq!(b.bytes_per_token_conservative, 4);
    assert_eq!(b.fixed_overhead_tokens, 100);
    assert_eq!(b.safety_margin_pct, 10);
    assert_eq!(b.image_token_budget, 1000);
    assert_eq!(b.tool_surcharge_tokens, 500);
    assert_eq!(b.web_search_surcharge_tokens, 500);
    assert_eq!(b.code_interpreter_surcharge_tokens, 1000);
    assert_eq!(b.minimal_generation_floor, 50);
}

#[test]
fn catalog_entry_partial_budgets_and_unknown_keys() {
    let mut v = minimal_entry();
    v["estimation_budgets"] = json!({ "bytes_per_token_conservative": 3 });
    v["some_future_field"] = json!(true);
    let e: ModelCatalogEntry = serde_json::from_value(v).unwrap();
    assert_eq!(e.estimation_budgets.bytes_per_token_conservative, 3);
    assert_eq!(e.estimation_budgets.fixed_overhead_tokens, 100);
}

#[test]
fn catalog_entry_requires_general_config() {
    let mut v = minimal_entry();
    v.as_object_mut().unwrap().remove("general_config");
    assert!(serde_json::from_value::<ModelCatalogEntry>(v).is_err());
}

#[test]
fn catalog_entry_accepts_empty_general_config() {
    let mut v = minimal_entry();
    v["general_config"] = json!({});
    let e: ModelCatalogEntry = serde_json::from_value(v).unwrap();
    let g = &e.general_config;
    assert_eq!(g.r#type, "");
    assert_eq!(g.available_from, "");
    assert_eq!(g.max_file_size_mb, 0, "0 = no per-model upload cap");
    assert!(g.api_params.stop.is_empty());
    assert_eq!(g.api_params.temperature, None);
    assert!(!g.features.streaming && !g.features.structured_output);
    assert_eq!(g.tool_support, ModelToolSupport::default());
    assert!(!g.tool_support.web_search && !g.tool_support.mcp);
    assert_eq!(g.supported_endpoints, SupportedEndpoints::default());
    assert!(!g.supported_endpoints.responses);
}

#[test]
fn catalog_entry_accepts_partial_nested_general_config() {
    let mut v = minimal_entry();
    v["general_config"] = json!({
        "api_params": { "temperature": 0.5 },
        "tool_support": { "file_search": true },
        "supported_endpoints": { "responses": true },
        "features": { "streaming": true }
    });
    let e: ModelCatalogEntry = serde_json::from_value(v).unwrap();
    let g = &e.general_config;
    assert_eq!(g.api_params.temperature, Some(0.5));
    assert!(g.api_params.stop.is_empty());
    assert!(g.tool_support.file_search && !g.tool_support.web_search && !g.tool_support.mcp);
    assert!(g.supported_endpoints.responses && !g.supported_endpoints.embeddings);
    assert!(g.features.streaming && !g.features.structured_output);
}

fn system_usage_event() -> UsageEvent {
    UsageEvent {
        tenant_id: Uuid::new_v4(),
        user_id: None,
        chat_id: Uuid::new_v4(),
        turn_id: None,
        request_id: Uuid::new_v4(),
        effective_model: "gpt-4.1-mini".to_owned(),
        selected_model: "gpt-4.1-mini".to_owned(),
        terminal_state: TerminalState::Completed,
        billing_outcome: BillingOutcome::SystemTask,
        usage: Some(UsageTokens {
            input_tokens: 5000,
            output_tokens: 200,
            cache_read_input_tokens: 4000,
            cache_write_input_tokens: 0,
            reasoning_tokens: 0,
        }),
        actual_credits_micro: 0,
        settlement_method: SettlementMethod::None,
        policy_version_applied: 0,
        web_search_calls: 0,
        code_interpreter_calls: 0,
        file_search_calls: 0,
        timestamp: time::OffsetDateTime::UNIX_EPOCH,
        requester_type: RequesterType::System,
        dedupe_key: "t/thread_summary_update/r".to_owned(),
        system_task_type: Some("thread_summary_update".to_owned()),
    }
}

#[test]
fn usage_event_omits_user_and_turn_for_system() {
    let ev = system_usage_event();
    let v = serde_json::to_value(&ev).unwrap();
    let obj = v.as_object().unwrap();
    assert!(!obj.contains_key("user_id"));
    assert!(!obj.contains_key("turn_id"));
    assert!(obj.contains_key("usage"));
    assert_eq!(v["billing_outcome"], "system_task");
    assert_eq!(v["settlement_method"], "none");
    assert_eq!(v["requester_type"], "system");
    assert_eq!(v["terminal_state"], "completed");
    assert_eq!(v["system_task_type"], "thread_summary_update");
    assert_eq!(v["timestamp"], "1970-01-01T00:00:00Z");
    let back: UsageEvent = serde_json::from_value(v).unwrap();
    assert_eq!(back, ev);
}

#[test]
fn usage_event_serializes_null_usage_and_user_fields() {
    let mut ev = system_usage_event();
    ev.usage = None;
    ev.user_id = Some(Uuid::new_v4());
    ev.turn_id = Some(Uuid::new_v4());
    ev.system_task_type = None;
    ev.requester_type = RequesterType::User;
    let v = serde_json::to_value(&ev).unwrap();
    assert!(v["usage"].is_null());
    assert!(v.as_object().unwrap().contains_key("usage"));
    assert!(v["user_id"].is_string());
    assert!(v["turn_id"].is_string());
    assert!(!v.as_object().unwrap().contains_key("system_task_type"));
}

#[test]
fn kill_switches_require_all_fields() {
    let full = json!({
        "disable_premium_tier": false,
        "force_standard_tier": false,
        "disable_web_search": true,
        "disable_file_search": false,
        "disable_images": false,
        "disable_code_interpreter": false
    });
    let ks: KillSwitches = serde_json::from_value(full.clone()).unwrap();
    assert!(ks.disable_web_search);
    for key in full.as_object().unwrap().keys() {
        let mut partial = full.clone();
        partial.as_object_mut().unwrap().remove(key);
        assert!(
            serde_json::from_value::<KillSwitches>(partial).is_err(),
            "missing {key} must fail"
        );
    }
}

#[test]
fn audit_events_round_trip_untagged() {
    let mutation = MiniChatAuditEvent::Mutation(TurnMutationAuditEvent {
        event_type: TurnMutationAuditEventType::TurnDelete,
        actor_user_id: Uuid::new_v4(),
        tenant_id: Uuid::new_v4(),
        chat_id: Uuid::new_v4(),
        original_request_id: None,
        new_request_id: None,
        request_id: Some(Uuid::new_v4()),
        timestamp: time::OffsetDateTime::UNIX_EPOCH,
    });
    let v = serde_json::to_value(&mutation).unwrap();
    assert_eq!(v["event_type"], "turn_delete");
    assert!(!v.as_object().unwrap().contains_key("new_request_id"));
    let back: MiniChatAuditEvent = serde_json::from_value(v).unwrap();
    assert_eq!(back, mutation);

    let turn = MiniChatAuditEvent::Turn(TurnAuditEvent {
        event_type: TurnAuditEventType::TurnCompleted,
        tenant_id: Uuid::new_v4(),
        user_id: Uuid::new_v4(),
        chat_id: Uuid::new_v4(),
        turn_id: Uuid::new_v4(),
        request_id: Uuid::new_v4(),
        selected_model: "gpt-4.1".to_owned(),
        effective_model: "gpt-4.1-mini".to_owned(),
        terminal_state: TerminalState::Completed,
        error_code: None,
        usage: Some(UsageTokens::default()),
        latency_ms: AuditLatency {
            ttft_ms: Some(10),
            total_ms: 20,
        },
        tool_calls: AuditToolCalls {
            web_search_calls: 1,
            file_search_calls: 0,
        },
        policy_decisions: AuditPolicyDecisions {
            quota: AuditQuotaDecision {
                decision: "downgrade".to_owned(),
                downgrade_from: Some("gpt-4.1".to_owned()),
                downgrade_reason: Some("premium_quota_exhausted".to_owned()),
            },
            license: None,
        },
        prompt: String::new(),
        response: String::new(),
        attachments: Vec::new(),
        quota_scope: None,
        trace_id: None,
        timestamp: time::OffsetDateTime::UNIX_EPOCH,
    });
    let v = serde_json::to_value(&turn).unwrap();
    assert_eq!(v["event_type"], "turn_completed");
    assert!(v["policy_decisions"]["license"].is_null());
    assert!(v["quota_scope"].is_null());
    assert_eq!(v["prompt"], "");
    let back: MiniChatAuditEvent = serde_json::from_value(v).unwrap();
    assert_eq!(back, turn);
}
