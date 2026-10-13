#![allow(clippy::unwrap_used)]

use mini_chat_sdk::EstimationBudgets;

use super::{BudgetInputs, HistoryMessage, SUMMARY_PREAMBLE, assemble, summary_trigger, system_prompt_with_guards};
use crate::domain::error::DomainError;
use crate::infra::llm::Role;

fn budget(window: i64) -> BudgetInputs {
    BudgetInputs {
        context_window: window,
        max_input_tokens: 0,
        max_output_tokens_applied: 100,
        surcharges: 0,
        budgets: EstimationBudgets {
            bytes_per_token_conservative: 1,
            fixed_overhead_tokens: 0,
            safety_margin_pct: 0,
            ..EstimationBudgets::default()
        },
    }
}

fn msg(role: Role, n: usize) -> HistoryMessage {
    HistoryMessage { role, text: "x".repeat(n) }
}

#[test]
fn keeps_everything_when_it_fits() {
    let recent = vec![msg(Role::User, 10), msg(Role::Assistant, 10)];
    let p = assemble(&budget(1000), "sys", "hello", 0, None, &recent).unwrap();
    assert_eq!(p.history.len(), 2);
    assert!(!p.messages_truncated);
    assert_eq!(p.assembled_tokens, 3 + 5 + 20);
    assert_eq!(p.effective_budget, 900);
}

#[test]
fn drops_oldest_whole_turns_first() {
    // budget 200 - 100 output = 100 tokens; mandatory 10
    let recent = vec![
        msg(Role::User, 30),
        msg(Role::Assistant, 30),
        msg(Role::User, 20),
        msg(Role::Assistant, 25),
    ];
    let p = assemble(&budget(200), "sys", "1234567", 0, None, &recent).unwrap();
    // newest-first fits 25 + 20 + 30 = 75 <= 90, next 30 does not → leading assistant dropped
    assert!(p.messages_truncated);
    assert_eq!(p.history.len(), 2);
    assert_eq!(p.history[0].role, Role::User);
    // deterministic
    let q = assemble(&budget(200), "sys", "1234567", 0, None, &recent).unwrap();
    assert_eq!(p, q);
}

#[test]
fn summary_kept_when_it_fits_and_dropped_otherwise() {
    let p = assemble(&budget(1000), "s", "u", 0, Some("short"), &[]).unwrap();
    assert!(p.summary_applied.is_some());
    assert!(p.history[0].text.starts_with(SUMMARY_PREAMBLE));
    let p = assemble(&budget(250), "s", "u", 0, Some(&"y".repeat(500)), &[]).unwrap();
    assert!(p.summary_applied.is_none());
}

#[test]
fn mandatory_overflow_and_window_errors() {
    let e = assemble(&budget(150), "s", &"u".repeat(60), 0, None, &[]).unwrap_err();
    assert!(matches!(e, DomainError::ContextBudgetExceeded(_)));
    let mut b = budget(100);
    b.max_output_tokens_applied = 100;
    assert!(matches!(assemble(&b, "s", "u", 0, None, &[]), Err(DomainError::ContextBudgetExceeded(_))));
    let mut b = budget(1000);
    b.surcharges = 900;
    assert!(matches!(assemble(&b, "s", "u", 0, None, &[]), Err(DomainError::ContextBudgetExceeded(_))));
    let mut b = budget(5000);
    b.budgets.image_token_budget = 1000;
    assert!(matches!(assemble(&b, "s", "u", 5, None, &[]), Err(DomainError::ContextBudgetExceeded(_))));
}

#[test]
fn input_limit_uses_max_input_tokens() {
    let mut b = budget(1000);
    b.max_input_tokens = 300;
    assert_eq!(b.input_limit(), 300);
}

#[test]
fn trigger_rules() {
    let mut p = assemble(&budget(1000), "s", &"u".repeat(800), 0, None, &[]).unwrap();
    assert!(summary_trigger(&p, false, 80));
    assert!(!summary_trigger(&p, true, 80));
    p.messages_truncated = true;
    assert!(summary_trigger(&p, true, 80));
    let small = assemble(&budget(1000), "s", "u", 0, None, &[]).unwrap();
    assert!(!summary_trigger(&small, false, 80));
}

#[test]
fn guards_are_appended() {
    assert_eq!(system_prompt_with_guards("base", &["g1", "g2"]), "base\n\ng1\n\ng2");
    assert_eq!(system_prompt_with_guards("", &["g"]), "g");
}
