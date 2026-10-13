#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use mini_chat_sdk::{EstimationBudgets, ModelGeneralConfig, ModelTier, ModelToolSupport};

fn model(window: u32, max_in: u32) -> ModelCatalogEntry {
    ModelCatalogEntry {
        id: "m".into(),
        provider_model_id: "m".into(),
        display_name: "m".into(),
        description: String::new(),
        icon: String::new(),
        provider_id: "p".into(),
        provider_display_name: String::new(),
        tier: ModelTier::Standard,
        enabled: true,
        multimodal_capabilities: vec![],
        context_window: window,
        max_output_tokens: 100,
        max_input_tokens: max_in,
        input_tokens_credit_multiplier_micro: 1,
        output_tokens_credit_multiplier_micro: 1,
        multiplier_display: String::new(),
        estimation_budgets: EstimationBudgets {
            bytes_per_token_conservative: 1,
            fixed_overhead_tokens: 0,
            safety_margin_pct: 0,
            image_token_budget: 10,
            tool_surcharge_tokens: 5,
            web_search_surcharge_tokens: 5,
            code_interpreter_surcharge_tokens: 5,
            minimal_generation_floor: 1,
        },
        max_num_results: 5,
        web_search_context_size: mini_chat_sdk::WebSearchContextSize::Low,
        max_tool_calls: 2,
        general_config: ModelGeneralConfig {
            config_type: String::new(),
            available_from: String::new(),
            max_file_size_mb: 25,
            api_params: mini_chat_sdk::ApiParams::default(),
            features: mini_chat_sdk::ModelFeatures::default(),
            tool_support: ModelToolSupport::default(),
            supported_endpoints: mini_chat_sdk::SupportedEndpoints::default(),
        },
        preference: None,
        system_prompt: "SYS".into(),
        thread_summary_prompt: String::new(),
    }
}

fn h(role: InputRole, s: &str) -> HistoryMessage {
    HistoryMessage {
        role,
        content: s.into(),
    }
}

fn inputs<'a>(
    m: &'a ModelCatalogEntry,
    history: Vec<HistoryMessage>,
    summary: Option<&'a str>,
) -> ContextInputs<'a> {
    ContextInputs {
        model: m,
        max_output_tokens_applied: 100,
        tools: ToolPlan::default(),
        web_search_guard: "WEB",
        file_search_guard: "FILE",
        summary,
        history,
        user_text: "question",
        image_file_ids: vec![],
    }
}

#[test]
fn order_and_no_truncation() {
    let m = model(10_000, 0);
    let p = assemble(inputs(
        &m,
        vec![h(InputRole::User, "q1"), h(InputRole::Assistant, "a1")],
        Some("SUM"),
    ))
    .unwrap();
    assert_eq!(p.instructions, "SYS");
    assert!(p.summary_included);
    assert!(p.input[0].text.starts_with(SUMMARY_PREAMBLE));
    assert!(p.input[0].text.ends_with("SUM"));
    assert_eq!(p.input[1].text, "q1");
    assert_eq!(p.input[2].text, "a1");
    assert_eq!(p.input[3].text, "question");
    assert!(!p.messages_truncated);
    assert_eq!(p.effective_budget, 9_900);
}

#[test]
fn oldest_whole_turns_dropped_first() {
    // budget: 150 - 100 out = 50 tokens; mandatory = 3 (SYS) + 8 (question) = 11; 39 left
    let m = model(150, 0);
    let hist = vec![
        h(InputRole::User, &"x".repeat(20)),
        h(InputRole::Assistant, &"y".repeat(20)),
        h(InputRole::User, &"z".repeat(10)),
        h(InputRole::Assistant, &"w".repeat(10)),
    ];
    let p = assemble(inputs(&m, hist, None)).unwrap();
    // newest two (20 tokens) fit; then "y" (20) does not fit -> dropped with older
    assert_eq!(p.input.len(), 3);
    assert_eq!(p.input[0].text, "z".repeat(10));
    assert!(p.messages_truncated);
}

#[test]
fn leading_assistant_dropped() {
    let m = model(150, 0);
    let hist = vec![
        h(InputRole::User, &"x".repeat(30)),
        h(InputRole::Assistant, &"y".repeat(10)),
        h(InputRole::User, "q2"),
    ];
    let p = assemble(inputs(&m, hist, None)).unwrap();
    assert_eq!(
        p.input.len(),
        2,
        "assistant without its question is dropped: {:?}",
        p.input
    );
    assert_eq!(p.input[0].text, "q2");
    assert!(p.messages_truncated);
}

#[test]
fn summary_dropped_when_not_fitting() {
    let m = model(150, 0);
    let long = "s".repeat(200);
    let p = assemble(inputs(&m, vec![], Some(&long))).unwrap();
    assert!(!p.summary_included);
    assert_eq!(p.input.len(), 1);
}

#[test]
fn mandatory_overflow_is_rejected() {
    let m = model(110, 0);
    let mut i = inputs(&m, vec![], None);
    i.user_text = "this question is too long for ten tokens";
    assert!(matches!(
        assemble(i),
        Err(DomainError::OutOfRange {
            reason: "CONTEXT_BUDGET_EXCEEDED",
            ..
        })
    ));
    let m2 = model(100, 0);
    assert!(
        assemble(inputs(&m2, vec![], None)).is_err(),
        "max_output >= window"
    );
}

#[test]
fn max_input_tokens_caps_budget_and_guards_follow_tools() {
    let m = model(10_000, 500);
    let mut i = inputs(&m, vec![], None);
    i.tools = ToolPlan {
        file_search: true,
        web_search: true,
        code_interpreter: false,
    };
    let p = assemble(i).unwrap();
    assert_eq!(p.effective_budget, 500);
    assert_eq!(p.instructions, "SYS\n\nFILE\n\nWEB");
}
