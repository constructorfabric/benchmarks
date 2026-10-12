#![allow(clippy::unwrap_used, clippy::expect_used)]

use mini_chat_sdk::ModelCatalogEntry;
use serde_json::json;

use super::{ContextInputs, assemble, check_input_length, effective_input_limit};
use crate::domain::error::DomainError;
use crate::domain::estimation::ToolSet;
use crate::domain::llm::{InputMessage, InputRole};

fn entry(window: u32, max_output: u32, max_input: u32) -> ModelCatalogEntry {
    serde_json::from_value(json!({
        "id": "m", "provider_model_id": "pm", "display_name": "M", "provider_id": "p",
        "provider_display_name": "P", "tier": "premium", "enabled": true,
        "context_window": window, "max_output_tokens": max_output, "max_input_tokens": max_input,
        "input_tokens_credit_multiplier_micro": 1_000_000,
        "output_tokens_credit_multiplier_micro": 3_000_000,
        "max_num_results": 5,
        "general_config": {
            "type": "", "available_from": "1970-01-01T00:00:00Z", "max_file_size_mb": 25,
            "api_params": { "stop": [] },
            "features": { "streaming": true, "structured_output": true },
            "tool_support": { "web_search": true, "file_search": true,
                "image_generation": false, "code_interpreter": true, "mcp": false },
            "supported_endpoints": {}
        }
    }))
    .unwrap()
}

fn message(role: InputRole, marker: &str, bytes: usize) -> InputMessage {
    InputMessage {
        role,
        text: format!("{marker}{}", "x".repeat(bytes - marker.len())),
        image_file_ids: Vec::new(),
    }
}

fn inputs(current: &str, history: Vec<InputMessage>) -> ContextInputs<'_> {
    ContextInputs {
        system_prompt: "You are a helpful assistant.",
        tools: ToolSet::default(),
        max_output_tokens_applied: 1000,
        current_text: current,
        image_count: 0,
        summary: None,
        history,
    }
}

fn turns() -> Vec<InputMessage> {
    vec![
        message(InputRole::User, "Q-ONE", 4000),
        message(InputRole::Assistant, "A-ONE", 4000),
        message(InputRole::User, "Q-TWO", 4000),
        message(InputRole::Assistant, "A-TWO", 4000),
        message(InputRole::User, "Q-THREE", 4000),
        message(InputRole::Assistant, "A-THREE", 4000),
    ]
}

#[test]
fn the_input_limit_is_the_smaller_of_the_input_cap_and_the_free_window() {
    assert_eq!(
        effective_input_limit(&entry(4000, 1000, 0), 1000),
        Some(3000)
    );
    assert_eq!(
        effective_input_limit(&entry(4000, 1000, 2000), 1000),
        Some(2000)
    );
    assert_eq!(effective_input_limit(&entry(4096, 4096, 0), 4096), None);
}

#[test]
fn input_too_long_compares_the_estimate_with_max_input_tokens() {
    let limited = entry(128_000, 4096, 2000);

    assert!(check_input_length(&limited, 3000).is_ok());
    assert!(matches!(
        check_input_length(&limited, 7000),
        Err(DomainError::InputTooLong)
    ));
    assert!(check_input_length(&entry(128_000, 4096, 0), 1_000_000).is_ok());
}

#[test]
fn the_oldest_whole_turns_are_dropped_first() {
    let plan = assemble(&entry(4000, 1000, 0), inputs("Q-NOW", turns())).unwrap();

    let kept: Vec<&str> = plan
        .history
        .iter()
        .map(|m| &m.text[..7.min(m.text.len())])
        .collect();
    assert_eq!(plan.history.len(), 2, "{kept:?}");
    assert!(plan.history[0].text.starts_with("Q-THREE"));
    assert!(plan.history[1].text.starts_with("A-THREE"));
    assert!(plan.messages_truncated);
}

#[test]
fn an_answer_is_never_kept_without_its_question() {
    let big = "q".repeat(4000);
    let history = turns()[..4].to_vec();

    let plan = assemble(&entry(4000, 1000, 0), inputs(&big, history)).unwrap();

    assert!(plan.history.is_empty(), "{:?}", plan.history);
    assert!(plan.messages_truncated);
}

#[test]
fn mandatory_items_that_do_not_fit_are_refused() {
    let content = "x".repeat(6800);

    let result = assemble(&entry(128_000, 4096, 2000), inputs(&content, Vec::new()));

    assert!(matches!(result, Err(DomainError::ContextBudgetExceeded)));
}

#[test]
fn a_tool_surcharge_shrinks_the_budget() {
    let content = "x".repeat(4800);
    let model = entry(128_000, 4096, 2000);
    let mut with_tool = inputs(&content, Vec::new());
    with_tool.tools.web_search = true;

    assert!(assemble(&model, inputs(&content, Vec::new())).is_ok());
    assert!(matches!(
        assemble(&model, with_tool),
        Err(DomainError::ContextBudgetExceeded)
    ));
}

#[test]
fn the_summary_is_kept_when_it_fits_and_dropped_otherwise() {
    let mut fits = inputs("Q-NOW", Vec::new());
    fits.summary = Some("short summary");
    let mut too_big_text = String::new();
    too_big_text.push_str(&"s".repeat(20_000));
    let mut too_big = inputs("Q-NOW", Vec::new());
    too_big.summary = Some(&too_big_text);

    let kept = assemble(&entry(4000, 1000, 0), fits).unwrap();
    let dropped = assemble(&entry(4000, 1000, 0), too_big).unwrap();

    assert!(kept.summary_message.unwrap().ends_with("short summary"));
    assert!(dropped.summary_message.is_none());
}

#[test]
fn a_history_that_fits_is_kept_complete() {
    let history = vec![
        message(InputRole::User, "Q-ONE", 5),
        message(InputRole::Assistant, "A-ONE", 5),
    ];

    let plan = assemble(&entry(128_000, 4096, 0), inputs("Q-NOW", history.clone())).unwrap();

    assert_eq!(plan.history, history);
    assert!(!plan.messages_truncated);
}
