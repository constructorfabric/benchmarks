//! Tests for context assembly, truncation, and the summary trigger.

use super::*;

fn model() -> ModelCatalogEntry {
    crate::test_support::catalog_entry("m", mini_chat_sdk::ModelTier::Standard, true, true)
}

fn msg(role: &str, content: &str) -> ContextMessage {
    ContextMessage {
        role: role.to_owned(),
        content: content.to_owned(),
    }
}

fn input(system: &str, summary: Option<(&str, u32)>, recent: Vec<ContextMessage>, user: &str) -> AssemblyInput {
    AssemblyInput {
        system_prompt: system.to_owned(),
        summary: summary.map(|(t, e)| (t.to_owned(), e)),
        recent,
        user_message: user.to_owned(),
        image_count: 0,
    }
}

#[test]
fn mandatory_items_never_truncated() {
    let m = model();
    // Budget: context_window 128000 - max_output 4096 = 123904, minus
    // fixed overhead 100 => plenty for a small request.
    let plan = assemble(&m, 4096, 0, input("sys", None, vec![msg("user", "old")], "hi"))
        .expect("fits");
    assert_eq!(plan.messages.len(), 1);
    assert!(!plan.messages_truncated);
}

#[test]
fn context_budget_exceeded_when_mandatory_too_large() {
    let mut m = model();
    m.context_window = 200;
    m.estimation_budgets.fixed_overhead_tokens = 10;
    // Fits: mandatory = ~24 tokens, budget = 200 - 40 - 10 = 150.
    let ok = assemble(&m, 40, 0, input("sys", None, vec![], "hi"));
    assert!(ok.is_ok(), "mandatory items fit: {ok:?}");
    // A 1000-byte system prompt alone (~286 estimated tokens) blows the
    // budget (150).
    let big = "s".repeat(1000);
    let err = assemble(&m, 40, 0, input(&big, None, vec![], "hi"));
    assert!(matches!(err, Err(DomainError::ContextBudgetExceeded)), "{err:?}");
}

#[test]
fn summary_dropped_when_it_does_not_fit() {
    let mut m = model();
    m.context_window = 2000;
    let big_summary = "s".repeat(4000);
    let plan = assemble(
        &m,
        1000,
        0,
        input("sys", Some((&big_summary, 250)), vec![msg("user", "old")], "hi"),
    )
    .expect("fits without the summary");
    assert!(plan.summary.is_none());
}

#[test]
fn oldest_turns_dropped_first() {
    let mut m = model();
    m.context_window = 4096;
    let recent: Vec<ContextMessage> = (0..20)
        .map(|i| msg(if i % 2 == 0 { "user" } else { "assistant" }, &"x".repeat(100)))
        .collect();
    let plan = assemble(&m, 2000, 0, input("sys", None, recent, "hi")).expect("fits");
    assert!(plan.messages_truncated);
    // The kept messages are the newest; the kept range never starts with an
    // assistant message.
    assert_ne!(plan.messages.first().map(|m| m.role.as_str()), Some("assistant"));
}

#[test]
fn determinism() {
    let m = model();
    let make = || {
        assemble(
            &m,
            4096,
            0,
            input(
                "sys",
                Some(("summary", 10)),
                vec![msg("user", "a"), msg("assistant", "b")],
                "hi",
            ),
        )
        .expect("fits")
    };
    let (a, b) = (make(), make());
    assert_eq!(a.messages, b.messages);
    assert_eq!(a.assembled_context_tokens, b.assembled_context_tokens);
}

#[test]
fn trigger_rules() {
    // Disabled worker: never.
    assert!(!summary_trigger(false, false, true, 1000, 1000, 80));
    // Existing summary without truncation: no trigger.
    assert!(!summary_trigger(true, true, false, 10_000, 10_000, 80));
    // Truncation always triggers.
    assert!(summary_trigger(true, true, true, 0, 1000, 80));
    assert!(summary_trigger(true, false, true, 0, 1000, 80));
    // Proactive: no summary and assembled >= threshold of the budget.
    assert!(summary_trigger(true, false, false, 800, 1000, 80));
    assert!(!summary_trigger(true, false, false, 799, 1000, 80));
}
