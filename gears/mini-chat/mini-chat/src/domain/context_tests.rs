use super::*;

fn model(context_window: u32, max_input: u32) -> ModelCatalogEntry {
    let mut m: ModelCatalogEntry = serde_json::from_value(serde_json::json!({
        "id": "m",
        "provider_model_id": "m",
        "display_name": "M",
        "provider_id": "openai",
        "provider_display_name": "OpenAI",
        "tier": "standard",
        "enabled": true,
        "context_window": context_window,
        "max_output_tokens": 100,
        "max_input_tokens": max_input,
        "input_tokens_credit_multiplier_micro": 1_000_000,
        "output_tokens_credit_multiplier_micro": 1_000_000,
        "max_num_results": 5,
        "general_config": {}
    }))
    .expect("catalog entry");
    m.system_prompt = "You are helpful.".to_owned();
    m
}

fn msg(role: Role, n: usize, tag: &str) -> HistoryMessage {
    HistoryMessage {
        role,
        text: format!("{tag}{}", "x".repeat(n)),
    }
}

fn inputs<'a>(
    m: &'a ModelCatalogEntry,
    history: Vec<HistoryMessage>,
    user: &'a str,
) -> ContextInputs<'a> {
    ContextInputs {
        model: m,
        guards: Vec::new(),
        summary: None,
        history,
        user_text: user,
        image_file_ids: Vec::new(),
        max_output_tokens_applied: 100,
        surcharges: 0,
    }
}

#[test]
fn input_limit_respects_max_input() {
    let m = model(4096, 3000);
    assert_eq!(input_limit(&m, 1024), 3000);
    let m = model(4096, 0);
    assert_eq!(input_limit(&m, 1024), 3072);
}

#[test]
fn instructions_join_prompt_and_guards() {
    let m = model(4096, 0);
    let s = instructions(&m, &["guard one".to_owned(), "  ".to_owned()]);
    assert_eq!(s, "You are helpful.\n\nguard one");
}

#[test]
fn everything_fits() {
    let m = model(100_000, 0);
    let history = vec![msg(Role::User, 10, "q1"), msg(Role::Assistant, 10, "a1")];
    let plan = assemble(inputs(&m, history, "now")).unwrap();
    assert!(!plan.messages_truncated);
    assert_eq!(plan.messages.len(), 3);
    assert_eq!(plan.messages.last().unwrap().text, "now");
    assert_eq!(plan.instructions, "You are helpful.");
    assert_eq!(plan.effective_budget, 100_000 - 100);
}

#[test]
fn oldest_messages_dropped_first_without_leading_answer() {
    // budget = 2000 - 100 - 100 (fixed overhead) = 1800 tokens
    let m = model(2_000, 0);
    let history = vec![
        msg(Role::User, 2_000, "q1"),
        msg(Role::Assistant, 2_000, "a1"),
        msg(Role::User, 1_000, "q2"),
        msg(Role::Assistant, 1_000, "a2"),
    ];
    let plan = assemble(inputs(&m, history, "now")).unwrap();
    assert!(plan.messages_truncated);
    let texts: Vec<&str> = plan.messages.iter().map(|m| &m.text[..2]).collect();
    assert_eq!(texts.first(), Some(&"q2"));
    assert!(!texts.contains(&"a1"));
    assert!(plan.assembled_tokens <= 1_800);
}

#[test]
fn truncation_is_deterministic() {
    let m = model(3_000, 0);
    let make = || {
        (0..10)
            .map(|i| {
                msg(
                    if i % 2 == 0 {
                        Role::User
                    } else {
                        Role::Assistant
                    },
                    900,
                    &format!("{i:02}"),
                )
            })
            .collect::<Vec<_>>()
    };
    let a = assemble(inputs(&m, make(), "now")).unwrap();
    let b = assemble(inputs(&m, make(), "now")).unwrap();
    let ta: Vec<_> = a.messages.iter().map(|m| m.text.clone()).collect();
    let tb: Vec<_> = b.messages.iter().map(|m| m.text.clone()).collect();
    assert_eq!(ta, tb);
    assert_eq!(a.assembled_tokens, b.assembled_tokens);
}

#[test]
fn mandatory_items_over_budget_are_rejected() {
    let m = model(1_000, 0);
    let user = "u".repeat(4_000);
    let err = assemble(inputs(&m, Vec::new(), &user)).unwrap_err();
    assert!(matches!(err, DomainError::ContextBudgetExceeded));
}

#[test]
fn output_reservation_over_window_is_rejected() {
    let m = model(100, 0);
    let mut i = inputs(&m, Vec::new(), "hi");
    i.max_output_tokens_applied = 100;
    assert!(matches!(
        assemble(i).unwrap_err(),
        DomainError::ContextBudgetExceeded
    ));
}

#[test]
fn surcharges_reduce_the_budget() {
    let m = model(2_000, 0);
    let mut i = inputs(&m, Vec::new(), "hi");
    i.surcharges = 1_800;
    assert!(matches!(
        assemble(i).unwrap_err(),
        DomainError::ContextBudgetExceeded
    ));
}

#[test]
fn summary_is_first_user_message_with_preamble() {
    let m = model(100_000, 0);
    let mut i = inputs(
        &m,
        vec![msg(Role::User, 5, "q"), msg(Role::Assistant, 5, "a")],
        "now",
    );
    i.summary = Some("the summary".to_owned());
    let plan = assemble(i).unwrap();
    let first = &plan.messages[0];
    assert_eq!(first.role, Role::User);
    assert!(first.text.starts_with(SUMMARY_PREAMBLE));
    assert!(first.text.ends_with("the summary"));
    assert!(plan.summary_tokens.unwrap() > 0);
    assert_eq!(plan.messages.len(), 4);
}

#[test]
fn summary_dropped_only_when_it_does_not_fit() {
    let m = model(1_200, 0);
    let mut i = inputs(&m, Vec::new(), "now");
    i.summary = Some("s".repeat(10_000));
    let plan = assemble(i).unwrap();
    assert!(plan.summary_tokens.is_none());
    assert_eq!(plan.messages.len(), 1);
}

#[test]
fn images_count_toward_mandatory_budget() {
    let m = model(1_500, 0);
    let mut i = inputs(&m, Vec::new(), "look");
    i.image_file_ids = vec!["file-a".to_owned(), "file-b".to_owned()];
    // 2 * image_token_budget (1000) exceeds the budget
    assert!(matches!(
        assemble(i).unwrap_err(),
        DomainError::ContextBudgetExceeded
    ));
    let big = model(100_000, 0);
    let mut i = inputs(&big, Vec::new(), "look");
    i.image_file_ids = vec!["file-a".to_owned()];
    let plan = assemble(i).unwrap();
    assert_eq!(plan.messages[0].image_file_ids, vec!["file-a".to_owned()]);
}
