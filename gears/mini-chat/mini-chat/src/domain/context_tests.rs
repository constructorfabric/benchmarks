use super::*;

fn model(window: u32, max_in: u32) -> ModelCatalogEntry {
    serde_json::from_value(serde_json::json!({
        "id": "m", "provider_model_id": "m", "display_name": "m", "provider_id": "p",
        "tier": "standard", "enabled": true, "context_window": window,
        "max_output_tokens": 100, "max_input_tokens": max_in,
        "input_tokens_credit_multiplier_micro": 1, "output_tokens_credit_multiplier_micro": 1,
        "estimation_budgets": {"bytes_per_token_conservative": 1, "fixed_overhead_tokens": 10, "safety_margin_pct": 0,
            "image_token_budget": 50, "tool_surcharge_tokens": 0, "web_search_surcharge_tokens": 0,
            "code_interpreter_surcharge_tokens": 0}
    }))
    .expect("model")
}

fn msg(role: HistoryRole, n: usize) -> HistoryMessage {
    HistoryMessage {
        role,
        content: "x".repeat(n),
    }
}

#[test]
fn keeps_everything_within_budget() {
    let m = model(1000, 0);
    let plan = assemble(ContextInput {
        model: &m,
        max_output_tokens_applied: 100,
        instructions: "sys".to_owned(),
        summary: Some("S".to_owned()),
        history: vec![msg(HistoryRole::User, 10), msg(HistoryRole::Assistant, 10)],
        user_text: "hello",
        image_count: 0,
        surcharges: 0,
    })
    .expect("fits");
    assert_eq!(plan.history.len(), 2);
    assert!(plan.summary_message.as_deref().is_some_and(|s| s.starts_with(SUMMARY_PREAMBLE)));
    assert!(!plan.messages_truncated);
    assert_eq!(plan.effective_budget, 900);
}

#[test]
fn drops_oldest_whole_turns_first() {
    // budget = 300 - 100 - 10 = 190; mandatory = 3 + 5 = 8
    let m = model(300, 0);
    let history = vec![
        msg(HistoryRole::User, 60),
        msg(HistoryRole::Assistant, 60),
        msg(HistoryRole::User, 50),
        msg(HistoryRole::Assistant, 60),
    ];
    let plan = assemble(ContextInput {
        model: &m,
        max_output_tokens_applied: 100,
        instructions: "sys".to_owned(),
        summary: None,
        history,
        user_text: "hello",
        image_count: 0,
        surcharges: 0,
    })
    .expect("fits");
    assert!(plan.messages_truncated);
    assert_eq!(plan.history.len(), 2, "only the last whole turn is kept");
    assert_eq!(plan.history[0].role, HistoryRole::User);
}

#[test]
fn summary_dropped_when_it_does_not_fit() {
    let m = model(300, 0);
    let plan = assemble(ContextInput {
        model: &m,
        max_output_tokens_applied: 100,
        instructions: "sys".to_owned(),
        summary: Some("y".repeat(500)),
        history: Vec::new(),
        user_text: "hello",
        image_count: 0,
        surcharges: 0,
    })
    .expect("fits");
    assert!(plan.summary_message.is_none());
}

#[test]
fn mandatory_items_over_budget_rejected() {
    let m = model(300, 0);
    let err = assemble(ContextInput {
        model: &m,
        max_output_tokens_applied: 100,
        instructions: "sys".to_owned(),
        summary: None,
        history: Vec::new(),
        user_text: &"z".repeat(500),
        image_count: 0,
        surcharges: 0,
    })
    .expect_err("too long");
    assert!(matches!(err, DomainError::ContextBudgetExceeded));
    let err = assemble(ContextInput {
        model: &m,
        max_output_tokens_applied: 300,
        instructions: String::new(),
        summary: None,
        history: Vec::new(),
        user_text: "a",
        image_count: 0,
        surcharges: 0,
    })
    .expect_err("output >= window");
    assert!(matches!(err, DomainError::ContextBudgetExceeded));
}

#[test]
fn max_input_tokens_caps_budget_and_trigger_threshold() {
    let m = model(1000, 200);
    assert_eq!(effective_budget(&m, 100), 200);
    let plan = assemble(ContextInput {
        model: &m,
        max_output_tokens_applied: 100,
        instructions: "s".repeat(100),
        summary: None,
        history: vec![msg(HistoryRole::User, 70)],
        user_text: "u",
        image_count: 0,
        surcharges: 0,
    })
    .expect("fits");
    assert!(summary_trigger(&plan, false, 80));
    assert!(!summary_trigger(&plan, true, 80));
}

#[test]
fn deterministic_for_identical_inputs() {
    let m = model(300, 0);
    let mk = || ContextInput {
        model: &m,
        max_output_tokens_applied: 100,
        instructions: "sys".to_owned(),
        summary: None,
        history: vec![msg(HistoryRole::User, 100), msg(HistoryRole::Assistant, 100)],
        user_text: "q",
        image_count: 1,
        surcharges: 5,
    };
    assert_eq!(assemble(mk()).expect("a"), assemble(mk()).expect("b"));
}
