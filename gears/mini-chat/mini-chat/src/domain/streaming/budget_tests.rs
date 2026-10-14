//! The context budget formula (slice S1-setup-path of
//! `cpt-cf-mini-chat-feature-message-streaming`): `max_input_tokens` of 0, the lowered input
//! limit, the tool surcharges and the two rejections.
//!
//! Algorithm `cpt-cf-mini-chat-algo-message-streaming-context-budget`; constraint
//! `cpt-cf-mini-chat-constraint-context-budget`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mini_chat_sdk::ModelCatalogEntry;
use serde_json::json;

use super::{ContextBudget, ToolDefinition, ToolKind, ToolSet, compute_budget};
use crate::domain::error::DomainError;
use crate::test_support::send::std_model;

/// An entry with the given window and limits; surcharges: file search 500, web search 500, code
/// interpreter 1000.
fn entry(window: u32, max_input: u32, overhead: u32) -> ModelCatalogEntry {
    std_model(&json!({
        "context_window": window,
        "max_input_tokens": max_input,
        "max_output_tokens": 2_000,
        "estimation_budgets": {
            "bytes_per_token_conservative": 4,
            "fixed_overhead_tokens": overhead,
            "safety_margin_pct": 0,
            "image_token_budget": 1000,
            "tool_surcharge_tokens": 500,
            "web_search_surcharge_tokens": 500,
            "code_interpreter_surcharge_tokens": 1000
        }
    }))
}

fn tools(kinds: &[ToolKind]) -> ToolSet {
    ToolSet {
        tools: kinds
            .iter()
            .map(|kind| ToolDefinition {
                kind: *kind,
                search_context_size: None,
            })
            .collect(),
        ..ToolSet::default()
    }
}

#[test]
fn without_an_input_limit_the_window_minus_the_output_cap_is_the_limit() {
    let budget = compute_budget(&entry(10_000, 0, 100), 2_000, &tools(&[])).unwrap();
    assert_eq!(
        budget,
        ContextBudget {
            token_budget: 7_900,
            effective_budget: 8_000
        }
    );
}

#[test]
fn a_smaller_max_input_tokens_lowers_the_limit() {
    let budget = compute_budget(&entry(10_000, 5_000, 100), 2_000, &tools(&[])).unwrap();
    assert_eq!(budget.effective_budget, 5_000);
    assert_eq!(budget.token_budget, 4_900);
}

#[test]
fn a_larger_max_input_tokens_does_not_raise_the_limit() {
    let budget = compute_budget(&entry(10_000, 9_000, 100), 2_000, &tools(&[])).unwrap();
    assert_eq!(budget.effective_budget, 8_000);
    assert_eq!(budget.token_budget, 7_900);
}

#[test]
fn the_applied_output_cap_of_the_decision_is_used_not_the_catalog_value() {
    let budget = compute_budget(&entry(10_000, 0, 0), 500, &tools(&[])).unwrap();
    assert_eq!(budget.effective_budget, 9_500);
    assert_eq!(budget.token_budget, 9_500);
}

#[test]
fn each_tool_of_the_set_deducts_its_own_surcharge() {
    let cases: [(&[ToolKind], i64); 6] = [
        (&[], 100),
        (&[ToolKind::FileSearch], 600),
        (&[ToolKind::WebSearch], 600),
        (&[ToolKind::CodeInterpreter], 1_100),
        (&[ToolKind::SearchKnowledge], 100),
        (
            &[
                ToolKind::FileSearch,
                ToolKind::WebSearch,
                ToolKind::CodeInterpreter,
            ],
            2_100,
        ),
    ];
    for (kinds, deductions) in cases {
        let budget = compute_budget(&entry(10_000, 0, 100), 2_000, &tools(kinds)).unwrap();
        assert_eq!(budget.token_budget, 8_000 - deductions, "{kinds:?}");
        // the effective budget subtracts neither surcharge nor fixed overhead
        assert_eq!(budget.effective_budget, 8_000, "{kinds:?}");
    }
}

#[test]
fn an_output_cap_at_or_above_the_window_is_a_budget_rejection() {
    for cap in [10_000, 10_001, 50_000] {
        let result = compute_budget(&entry(10_000, 0, 0), cap, &tools(&[]));
        assert!(
            matches!(result, Err(DomainError::ContextBudgetExceeded)),
            "cap {cap}: {result:?}"
        );
    }
}

#[test]
fn deductions_that_reach_the_input_limit_are_a_budget_rejection() {
    // input limit 600: overhead 600 reaches it, 599 leaves one token
    let reaches = compute_budget(&entry(1_000, 0, 600), 400, &tools(&[]));
    assert!(
        matches!(reaches, Err(DomainError::ContextBudgetExceeded)),
        "{reaches:?}"
    );
    let leaves = compute_budget(&entry(1_000, 0, 599), 400, &tools(&[])).unwrap();
    assert_eq!(leaves.token_budget, 1);
    assert_eq!(leaves.effective_budget, 600);
}

#[test]
fn a_tool_surcharge_can_push_the_deductions_to_the_limit() {
    // input limit 1100: overhead 100 + code interpreter 1000 reaches it
    let result = compute_budget(
        &entry(2_000, 0, 100),
        900,
        &tools(&[ToolKind::CodeInterpreter]),
    );
    assert!(
        matches!(result, Err(DomainError::ContextBudgetExceeded)),
        "{result:?}"
    );
}

#[test]
fn the_input_limit_lowered_by_max_input_tokens_also_bounds_the_deductions() {
    let result = compute_budget(&entry(100_000, 400, 400), 1_000, &tools(&[]));
    assert!(
        matches!(result, Err(DomainError::ContextBudgetExceeded)),
        "{result:?}"
    );
}
