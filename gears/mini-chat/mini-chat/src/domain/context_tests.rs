use super::*;
use mini_chat_sdk::ModelCatalogEntry;

fn model(context_window: u32, max_input: u32) -> ModelCatalogEntry {
    serde_json::from_value(serde_json::json!({
        "id": "m", "provider_model_id": "m", "display_name": "M", "provider_id": "p", "tier": "standard",
        "enabled": true, "context_window": context_window, "max_output_tokens": 100, "max_input_tokens": max_input,
        "input_tokens_credit_multiplier_micro": 1, "output_tokens_credit_multiplier_micro": 1,
        "estimation_budgets": {"bytes_per_token_conservative": 4, "fixed_overhead_tokens": 10, "safety_margin_pct": 0,
            "image_token_budget": 50, "tool_surcharge_tokens": 0, "web_search_surcharge_tokens": 0,
            "code_interpreter_surcharge_tokens": 0, "minimal_generation_floor": 1}
    }))
    .unwrap()
}

fn msg(role: &str, n: usize) -> HistoryMessage {
    HistoryMessage { role: role.into(), content: "x".repeat(n) }
}

#[test]
fn keeps_everything_when_it_fits() {
    let m = model(10_000, 0);
    let recent = vec![msg("user", 40), msg("assistant", 40)];
    let p = plan(&ContextInput {
        model: &m,
        max_output_tokens_applied: 100,
        tool_surcharge_tokens: 0,
        instructions: "sys",
        summary: Some("old facts"),
        recent: &recent,
        user_message: "hi",
        images: 0,
    })
    .unwrap();
    assert_eq!(p.history.len(), 2);
    assert!(!p.messages_truncated);
    assert!(p.summary_block.as_deref().unwrap().starts_with(SUMMARY_PREAMBLE));
    assert_eq!(p.effective_budget, 9_900);
}

#[test]
fn drops_oldest_whole_turns_first() {
    let m = model(1000, 300);
    // budget = min(300, 900) - 10 = 290 ; each 400-byte message = 110 tokens
    let recent = vec![msg("user", 400), msg("assistant", 400), msg("user", 400), msg("assistant", 400)];
    let p = plan(&ContextInput {
        model: &m,
        max_output_tokens_applied: 100,
        tool_surcharge_tokens: 0,
        instructions: "",
        summary: None,
        recent: &recent,
        user_message: "q",
        images: 0,
    })
    .unwrap();
    assert!(p.messages_truncated);
    // never starts with an assistant message
    assert!(p.history.first().is_none_or(|m| m.role == "user"));
    assert!(p.assembled_tokens <= 290);
}

#[test]
fn mandatory_items_over_budget_is_rejected() {
    let m = model(1000, 100);
    let err = plan(&ContextInput {
        model: &m,
        max_output_tokens_applied: 100,
        tool_surcharge_tokens: 0,
        instructions: "",
        summary: None,
        recent: &[],
        user_message: &"y".repeat(2000),
        images: 0,
    })
    .unwrap_err();
    assert!(matches!(err, DomainError::ContextBudgetExceeded(_)));
}

#[test]
fn output_cap_at_or_above_window_is_rejected() {
    let m = model(100, 0);
    let err = plan(&ContextInput {
        model: &m,
        max_output_tokens_applied: 100,
        tool_surcharge_tokens: 0,
        instructions: "",
        summary: None,
        recent: &[],
        user_message: "a",
        images: 0,
    })
    .unwrap_err();
    assert!(matches!(err, DomainError::ContextBudgetExceeded(_)));
}

#[test]
fn summary_dropped_when_it_does_not_fit() {
    let m = model(1000, 200);
    let big = "s".repeat(4000);
    let p = plan(&ContextInput {
        model: &m,
        max_output_tokens_applied: 100,
        tool_surcharge_tokens: 0,
        instructions: "",
        summary: Some(&big),
        recent: &[],
        user_message: "a",
        images: 0,
    })
    .unwrap();
    assert!(p.summary_block.is_none());
    assert_eq!(p.summary_tokens, 0);
}

#[test]
fn deterministic() {
    let m = model(1000, 300);
    let recent = vec![msg("user", 100), msg("assistant", 900), msg("user", 50)];
    let input = ContextInput {
        model: &m,
        max_output_tokens_applied: 100,
        tool_surcharge_tokens: 20,
        instructions: "sys",
        summary: Some("sum"),
        recent: &recent,
        user_message: "now",
        images: 1,
    };
    assert_eq!(plan(&input).unwrap(), plan(&input).unwrap());
}
