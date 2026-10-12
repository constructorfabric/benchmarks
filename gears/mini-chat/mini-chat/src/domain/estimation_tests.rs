#![allow(clippy::unwrap_used, clippy::expect_used)]

use mini_chat_sdk::{EstimationBudgets, KillSwitches, ModelCatalogEntry};
use serde_json::json;

use super::{
    ReserveInputs, ToolFacts, ToolSet, candidate_reserve, estimated_text_tokens, tools_for,
};

fn entry(web_search: bool) -> ModelCatalogEntry {
    serde_json::from_value(json!({
        "id": "m", "provider_model_id": "pm", "display_name": "M", "provider_id": "p",
        "provider_display_name": "P", "tier": "premium", "enabled": true,
        "context_window": 128_000, "max_output_tokens": 4096, "max_input_tokens": 0,
        "input_tokens_credit_multiplier_micro": 1_000_000,
        "output_tokens_credit_multiplier_micro": 3_000_000,
        "max_num_results": 5,
        "general_config": {
            "type": "", "available_from": "1970-01-01T00:00:00Z", "max_file_size_mb": 25,
            "api_params": { "stop": [] },
            "features": { "streaming": true, "structured_output": true },
            "tool_support": { "web_search": web_search, "file_search": true,
                "image_generation": false, "code_interpreter": true, "mcp": false },
            "supported_endpoints": {}
        }
    }))
    .unwrap()
}

#[test]
fn five_bytes_estimate_to_113_tokens() {
    assert_eq!(estimated_text_tokens(5, &EstimationBudgets::default()), 113);
}

#[test]
fn the_estimate_follows_the_budgets_of_the_model() {
    let budgets = EstimationBudgets {
        bytes_per_token_conservative: 3,
        fixed_overhead_tokens: 10,
        safety_margin_pct: 20,
        ..EstimationBudgets::default()
    };
    // ceil(10 / 3) + 10 = 14; ceil(14 * 120 / 100) = 17
    assert_eq!(estimated_text_tokens(10, &budgets), 17);
}

#[test]
fn an_empty_message_costs_the_fixed_overhead_with_the_margin() {
    assert_eq!(estimated_text_tokens(0, &EstimationBudgets::default()), 110);
}

#[test]
fn a_zero_bytes_per_token_is_clamped_to_one() {
    let budgets = EstimationBudgets {
        bytes_per_token_conservative: 0,
        fixed_overhead_tokens: 0,
        safety_margin_pct: 0,
        ..EstimationBudgets::default()
    };
    assert_eq!(estimated_text_tokens(7, &budgets), 7);
}

#[test]
fn the_reserve_of_hello_matches_the_design_derivation() {
    let inputs = ReserveInputs {
        content_bytes: 5,
        prior_context_tokens: 0,
        image_count: 0,
        streaming_max_output_tokens: 32_768,
    };

    let reserve = candidate_reserve(&entry(true), &inputs, ToolSet::default()).unwrap();

    assert_eq!(reserve.estimated_input_tokens, 113);
    assert_eq!(reserve.max_output_tokens_applied, 4096);
    assert_eq!(reserve.reserve_tokens, 4209);
    assert_eq!(reserve.reserved_credits_micro, 12_401);
}

#[test]
fn prior_context_images_and_tools_enter_the_reserve() {
    let inputs = ReserveInputs {
        content_bytes: 5,
        prior_context_tokens: 15,
        image_count: 2,
        streaming_max_output_tokens: 1000,
    };
    let tools = ToolSet {
        file_search: true,
        web_search: true,
        code_interpreter: true,
    };

    let reserve = candidate_reserve(&entry(true), &inputs, tools).unwrap();

    assert_eq!(
        reserve.estimated_input_tokens,
        113 + 15 + 2 * 1000 + 500 + 500 + 1000
    );
    assert_eq!(reserve.max_output_tokens_applied, 1000);
}

#[test]
fn tools_are_gated_by_the_model_support_and_the_kill_switches() {
    let facts = ToolFacts {
        web_search_requested: true,
        ready_documents: true,
        ready_code_interpreter_files: true,
    };

    let supported = tools_for(&entry(true), KillSwitches::default(), facts);
    let no_web = tools_for(&entry(false), KillSwitches::default(), facts);
    let switched = tools_for(
        &entry(true),
        KillSwitches {
            disable_file_search: true,
            disable_code_interpreter: true,
            ..KillSwitches::default()
        },
        facts,
    );

    assert_eq!(
        supported.feature_label(),
        "file_search+web_search+code_interpreter"
    );
    assert!(!no_web.web_search);
    assert!(switched.web_search && !switched.file_search && !switched.code_interpreter);
    assert_eq!(ToolSet::default().feature_label(), "none");
}

#[test]
fn a_zero_multiplier_makes_the_reserve_uncomputable() {
    let mut zero = entry(true);
    zero.input_tokens_credit_multiplier_micro = 0;
    let inputs = ReserveInputs {
        content_bytes: 5,
        prior_context_tokens: 0,
        image_count: 0,
        streaming_max_output_tokens: 4096,
    };

    assert!(candidate_reserve(&zero, &inputs, ToolSet::default()).is_err());
}
