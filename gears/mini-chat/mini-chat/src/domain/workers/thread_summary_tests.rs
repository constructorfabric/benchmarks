#![allow(clippy::unwrap_used)]

use serde_json::json;

use super::{SummaryInput, build_user_prompt, fit_messages, parse_summary};

fn msgs(n: usize, len: usize) -> Vec<SummaryInput> {
    (0..n)
        .map(|i| SummaryInput { role: if i % 2 == 0 { "user".into() } else { "assistant".into() }, content: "x".repeat(len) })
        .collect()
}

#[test]
fn prompt_format() {
    let m = vec![
        SummaryInput { role: "user".into(), content: "hello".into() },
        SummaryInput { role: "assistant".into(), content: "abcdefghij".into() },
    ];
    let p = build_user_prompt(None, &m, 5);
    assert!(p.starts_with("Summarize the following conversation:\n\nUser: hello\n\nAssistant: abcde..."));
    assert!(p.contains("<analysis>"));
    let p = build_user_prompt(Some("OLD"), &m, 0);
    assert!(p.contains("<existing_summary>\nOLD\n</existing_summary>"));
    assert!(p.contains("New messages to incorporate:\n\nUser: hello\n\nAssistant: abcdefghij"));
}

#[test]
fn parsing_keeps_only_summary_block() {
    assert_eq!(parse_summary("<analysis>think</analysis>\n<summary>\nA\n\n\n\nB\n</summary>"), "A\n\nB");
    assert_eq!(parse_summary("plain text"), "plain text");
    assert_eq!(parse_summary("<analysis>only analysis</analysis>"), "");
    assert_eq!(parse_summary("<analysis>unterminated"), "");
    assert_eq!(parse_summary("<summary>no close"), "no close");
}

#[test]
fn fitting_drops_oldest_but_keeps_two() {
    let model: mini_chat_sdk::ModelCatalogEntry = serde_json::from_value(json!({
        "id": "m", "provider_model_id": "m", "display_name": "m", "provider_id": "p", "tier": "standard",
        "context_window": 1200, "max_output_tokens": 200, "max_input_tokens": 0,
        "input_tokens_credit_multiplier_micro": 1, "output_tokens_credit_multiplier_micro": 1, "max_num_results": 1,
        "general_config": {}
    })).unwrap();
    let out = fit_messages(&model, "sys", None, msgs(10, 400), 0);
    assert!(out.len() < 10 && out.len() >= 2);
    let out = fit_messages(&model, "sys", None, msgs(3, 100_000), 0);
    assert_eq!(out.len(), 2);
}
