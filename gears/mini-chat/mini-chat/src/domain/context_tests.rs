use mini_chat_sdk::EstimationBudgets;

use super::*;

fn msg(role: &str, len: usize) -> HistoryMessage {
    HistoryMessage { role: role.into(), content: "x".repeat(len) }
}

fn input<'a>(b: &'a EstimationBudgets, history: Vec<HistoryMessage>, summary: Option<&'a str>) -> ContextInput<'a> {
    ContextInput {
        instructions: "sys".into(),
        summary,
        history,
        user_message: "hello",
        image_count: 0,
        budgets: b,
        context_window: 2000,
        max_input_tokens: 0,
        max_output_tokens_applied: 1000,
        surcharges: 0,
    }
}

#[test]
fn keeps_everything_when_it_fits() {
    let b = EstimationBudgets::default();
    let plan = assemble(input(&b, vec![msg("user", 40), msg("assistant", 40)], Some("sum"))).unwrap();
    assert_eq!(plan.messages.len(), 2);
    assert!(plan.summary_message.as_deref().unwrap().starts_with(SUMMARY_PREAMBLE));
    assert!(!plan.messages_truncated);
    assert_eq!(plan.effective_budget, 1000);
}

#[test]
fn drops_oldest_whole_turns_first() {
    let b = EstimationBudgets::default();
    // budget = 1000 - 100 = 900; each 1200-byte message ~ 330 tokens
    let history = vec![msg("user", 1200), msg("assistant", 1200), msg("user", 1200), msg("assistant", 1200)];
    let plan = assemble(input(&b, history, None)).unwrap();
    assert!(plan.messages_truncated);
    assert_eq!(plan.messages.first().map(|m| m.role.as_str()), Some("user"));
    assert!(plan.messages.len() < 4);
}

#[test]
fn summary_dropped_when_it_does_not_fit() {
    let b = EstimationBudgets::default();
    let big = "s".repeat(10_000);
    let plan = assemble(input(&b, vec![], Some(&big))).unwrap();
    assert!(plan.summary_message.is_none());
}

#[test]
fn mandatory_overflow_is_rejected() {
    let b = EstimationBudgets::default();
    let long = "y".repeat(100_000);
    let mut i = input(&b, vec![], None);
    i.user_message = &long;
    assert!(matches!(assemble(i), Err(DomainError::ContextBudgetExceeded)));
    let mut i = input(&b, vec![], None);
    i.max_output_tokens_applied = 2000;
    assert!(matches!(assemble(i), Err(DomainError::ContextBudgetExceeded)));
}

#[test]
fn trigger_rules() {
    let b = EstimationBudgets::default();
    let plan = assemble(input(&b, vec![msg("user", 3000)], None)).unwrap();
    assert!(summary_trigger(&plan, false, 80));
    assert!(!summary_trigger(&plan, true, 80));
    let small = assemble(input(&b, vec![], None)).unwrap();
    assert!(!summary_trigger(&small, false, 80));
}

#[test]
fn deterministic() {
    let b = EstimationBudgets::default();
    let h = vec![msg("user", 900), msg("assistant", 1500), msg("user", 300)];
    assert_eq!(assemble(input(&b, h.clone(), None)).unwrap(), assemble(input(&b, h, None)).unwrap());
}
