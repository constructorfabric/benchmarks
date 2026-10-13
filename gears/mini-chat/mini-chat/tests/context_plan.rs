//! Context window budget and truncation (DESIGN 2.2 "Context Window Budget",
//! 4 "Context Plan Truncation Algorithm", 5.5.4 token estimation): the pure
//! functions of `mini_chat::domain::context`.
//!
//! Public API under test (see `S3-test-interface.md` section 2.3):
//! `estimate_tokens`, `token_budget`, `check_input_length`, `plan_context`
//! with `ContextInput`, `ContextPlan`, `HistoryMessage`, `HistoryRole`,
//! `ToolSet`, `ContextError`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::use_debug,
    clippy::unreadable_literal,
    clippy::needless_pass_by_value
)]

mod common;

use mini_chat::domain::context::{
    ContextError, ContextInput, HistoryMessage, HistoryRole, ToolSet, check_input_length,
    estimate_tokens, plan_context, token_budget,
};
use mini_chat_sdk::{EstimationBudgets, ModelCatalogEntry};
use serde_json::json;

fn entry_with(patch: serde_json::Value) -> ModelCatalogEntry {
    let mut value = common::catalog_entry_json("m", "Standard", true);
    for (k, v) in patch.as_object().unwrap() {
        value[k] = v.clone();
    }
    serde_json::from_value(value).expect("catalog entry")
}

/// One token per byte, no overhead, no margin: sizes are the byte lengths.
fn unit_budgets() -> EstimationBudgets {
    EstimationBudgets {
        bytes_per_token_conservative: 1,
        fixed_overhead_tokens: 0,
        safety_margin_pct: 0,
        image_token_budget: 50,
        tool_surcharge_tokens: 0,
        web_search_surcharge_tokens: 0,
        code_interpreter_surcharge_tokens: 0,
        minimal_generation_floor: 50,
    }
}

fn user(content: &str) -> HistoryMessage {
    HistoryMessage {
        role: HistoryRole::User,
        content: content.to_owned(),
    }
}

fn assistant(content: &str) -> HistoryMessage {
    HistoryMessage {
        role: HistoryRole::Assistant,
        content: content.to_owned(),
    }
}

fn ten(c: char) -> String {
    c.to_string().repeat(10)
}

/// `U1 A1 U2 A2 U3 A3`, ten bytes each.
fn history() -> Vec<HistoryMessage> {
    vec![
        user(&ten('a')),
        assistant(&ten('b')),
        user(&ten('c')),
        assistant(&ten('d')),
        user(&ten('e')),
        assistant(&ten('f')),
    ]
}

fn plan(
    token_budget: u64,
    history: &[HistoryMessage],
    images: u32,
) -> Result<mini_chat::domain::context::ContextPlan, ContextError> {
    let budgets = unit_budgets();
    plan_context(&ContextInput {
        token_budget,
        budgets: &budgets,
        system_prompt: &"s".repeat(20),
        history,
        user_message: &"u".repeat(10),
        image_count: images,
    })
}

fn contents(plan: &mini_chat::domain::context::ContextPlan) -> Vec<String> {
    plan.history.iter().map(|m| m.content.clone()).collect()
}

// ── token estimation (5.5.4) ─────────────────────────────────────────────────

#[test]
fn estimate_follows_the_documented_formula() {
    let b = EstimationBudgets::default(); // 4 bytes/token, +100, +10 %
    assert_eq!(estimate_tokens("", &b), 110, "empty = overhead with margin");
    assert_eq!(estimate_tokens("abcd", &b), 112, "ceil(101 * 1.10)");
    assert_eq!(
        estimate_tokens(&"x".repeat(40), &b),
        121,
        "(10 + 100) * 1.10"
    );
    assert_eq!(
        estimate_tokens(&"x".repeat(41), &b),
        123,
        "ceil(11 + 100) then ceil(122.1)"
    );
}

#[test]
fn estimate_counts_utf8_bytes_not_characters() {
    let b = EstimationBudgets::default();
    // four 2-byte characters = 8 bytes = 2 tokens -> (2 + 100) * 1.10 = 112.2 -> 113
    assert_eq!(estimate_tokens("\u{e9}\u{e9}\u{e9}\u{e9}", &b), 113);
}

#[test]
fn estimate_uses_the_budgets_of_the_model() {
    let b = EstimationBudgets {
        bytes_per_token_conservative: 3,
        fixed_overhead_tokens: 0,
        safety_margin_pct: 20,
        ..EstimationBudgets::default()
    };
    // ceil(10 / 3) = 4; 4 * 120 / 100 = 4.8 -> 5
    assert_eq!(estimate_tokens(&"x".repeat(10), &b), 5);
    assert_eq!(estimate_tokens("", &b), 0);
}

// ── token budget (2.2 Context Window Budget) ─────────────────────────────────

#[test]
fn budget_is_the_smaller_of_max_input_and_window_minus_output_minus_overhead() {
    // max_input 120_000, window 128_000 - 4096 output = 123_904 -> 120_000 - 100
    let e = entry_with(json!({}));
    assert_eq!(token_budget(&e, 4096, ToolSet::default()), Some(119_900));

    // the window side is smaller
    let e = entry_with(json!({"context_window": 10_000, "max_input_tokens": 120_000}));
    assert_eq!(
        token_budget(&e, 4096, ToolSet::default()),
        Some(10_000 - 4096 - 100)
    );

    // max_input_tokens = 0: no separate input limit
    let e = entry_with(json!({"max_input_tokens": 0}));
    assert_eq!(
        token_budget(&e, 4096, ToolSet::default()),
        Some(128_000 - 4096 - 100)
    );
}

#[test]
fn budget_deducts_the_surcharge_of_every_tool_in_the_request() {
    let e = entry_with(json!({"max_input_tokens": 0}));
    let base = 128_000 - 4096 - 100;
    let with = |file_search, web_search, code_interpreter| {
        token_budget(
            &e,
            4096,
            ToolSet {
                file_search,
                web_search,
                code_interpreter,
            },
        )
    };
    assert_eq!(with(true, false, false), Some(base - 500), "tool surcharge");
    assert_eq!(
        with(false, true, false),
        Some(base - 500),
        "web search surcharge"
    );
    assert_eq!(
        with(false, false, true),
        Some(base - 1000),
        "code interpreter surcharge"
    );
    assert_eq!(with(true, true, true), Some(base - 2000));
}

#[test]
fn budget_is_none_when_nothing_is_left_for_the_input() {
    // applied output >= context window
    let e = entry_with(json!({"context_window": 1000, "max_input_tokens": 0}));
    assert_eq!(token_budget(&e, 1000, ToolSet::default()), None);
    assert_eq!(token_budget(&e, 4096, ToolSet::default()), None);
    // deductions reach the input limit: 600 - 500 = 100 = fixed overhead
    let e = entry_with(json!({"context_window": 600, "max_input_tokens": 0}));
    assert_eq!(token_budget(&e, 500, ToolSet::default()), None);
    // one token left
    let e = entry_with(json!({"context_window": 601, "max_input_tokens": 0}));
    assert_eq!(token_budget(&e, 500, ToolSet::default()), Some(1));
}

// ── INPUT_TOO_LONG ───────────────────────────────────────────────────────────

#[test]
fn input_longer_than_max_input_tokens_is_rejected() {
    let e = entry_with(json!({"max_input_tokens": 200}));
    // 300 bytes -> ceil(75 + 100) * 1.10 = 193 <= 200
    assert_eq!(check_input_length(&e, &"x".repeat(300)), Ok(()));
    // 360 bytes -> (90 + 100) * 1.10 = 209 > 200
    assert_eq!(
        check_input_length(&e, &"x".repeat(360)),
        Err(ContextError::InputTooLong)
    );
}

#[test]
fn max_input_tokens_zero_means_no_separate_limit() {
    let e = entry_with(json!({"max_input_tokens": 0}));
    assert_eq!(check_input_length(&e, &"x".repeat(1_000_000)), Ok(()));
}

// ── truncation algorithm (4) ─────────────────────────────────────────────────

#[test]
fn everything_is_kept_when_it_fits() {
    let history = history();
    // mandatory 20 + 10 + all six messages 60
    let out = plan(90, &history, 0).unwrap();
    assert_eq!(out.history, history);
    assert!(!out.messages_truncated);
    assert_eq!(out.estimated_tokens, 90);
}

#[test]
fn an_empty_history_is_fine() {
    let out = plan(30, &[], 0).unwrap();
    assert!(out.history.is_empty());
    assert!(!out.messages_truncated);
    assert_eq!(out.estimated_tokens, 30);
}

#[test]
fn oldest_messages_are_dropped_first_and_whole_turns_remain() {
    let history = history();
    // mandatory 30, room for two messages -> A3, U3 fit... walk: A3(10) U3(10) fit, A2(10) no
    let out = plan(30 + 20, &history, 0).unwrap();
    assert_eq!(contents(&out), vec![ten('e'), ten('f')]);
    assert!(out.messages_truncated);
    assert_eq!(out.estimated_tokens, 50);

    // room for four messages
    let out = plan(30 + 40, &history, 0).unwrap();
    assert_eq!(contents(&out), vec![ten('c'), ten('d'), ten('e'), ten('f')]);
}

#[test]
fn a_kept_range_never_starts_with_an_assistant_answer() {
    let history = history();
    // room for three: the walk keeps A2 U3 A3, A2 would lead -> dropped as well
    let out = plan(30 + 30, &history, 0).unwrap();
    assert_eq!(contents(&out), vec![ten('e'), ten('f')]);
    assert_eq!(out.history[0].role, HistoryRole::User);
    assert!(out.messages_truncated);
    assert_eq!(
        out.estimated_tokens, 50,
        "the dropped answer is not counted"
    );
}

#[test]
fn the_walk_stops_at_the_first_message_that_does_not_fit() {
    // newest -> oldest: A2(5) fits, U2(100) does not; the small older turn is not rescued
    let history = vec![
        user("12345"),
        assistant("12345"),
        user(&"x".repeat(100)),
        assistant("12345"),
    ];
    let out = plan(30 + 20, &history, 0).unwrap();
    assert!(
        out.history.is_empty(),
        "A2 alone would lead the range, so it is dropped too: {:?}",
        out.history
    );
    assert!(out.messages_truncated);
}

#[test]
fn only_the_mandatory_items_remain_when_the_budget_has_no_room_for_history() {
    let out = plan(30, &history(), 0).unwrap();
    assert!(out.history.is_empty());
    assert!(out.messages_truncated);
    assert_eq!(out.estimated_tokens, 30);
}

#[test]
fn mandatory_items_that_do_not_fit_are_rejected() {
    assert_eq!(plan(29, &history(), 0), Err(ContextError::BudgetExceeded));
    assert_eq!(plan(0, &[], 0), Err(ContextError::BudgetExceeded));
}

#[test]
fn images_of_the_current_turn_are_mandatory_and_cost_the_image_budget_each() {
    // 30 + 2 * 50 = 130
    assert_eq!(plan(129, &[], 2), Err(ContextError::BudgetExceeded));
    let out = plan(130, &[], 2).unwrap();
    assert_eq!(out.estimated_tokens, 130);
    // 140: ten tokens of room, one message fits but would lead -> dropped
    let out = plan(140, &history(), 2).unwrap();
    assert!(out.history.is_empty());
}

#[test]
fn a_leading_assistant_message_is_dropped_even_when_everything_fits() {
    let history = vec![
        assistant("orphan answer"),
        user("question"),
        assistant("answer"),
    ];
    let out = plan(10_000, &history, 0).unwrap();
    assert_eq!(
        contents(&out),
        vec!["question".to_owned(), "answer".to_owned()]
    );
    assert!(
        out.messages_truncated,
        "a non-system recent message was dropped"
    );
}

#[test]
fn the_plan_is_deterministic() {
    let history = history();
    for budget in [30, 50, 60, 70, 90] {
        assert_eq!(
            plan(budget, &history, 0),
            plan(budget, &history, 0),
            "budget {budget}"
        );
    }
}

#[test]
fn history_order_is_preserved() {
    let history = history();
    let out = plan(60, &history, 0).unwrap();
    let mut expected = out.history.clone();
    expected.sort_by_key(|m| history.iter().position(|h| h == m));
    assert_eq!(out.history, expected);
}
