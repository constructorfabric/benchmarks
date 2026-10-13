#![allow(clippy::unwrap_used, clippy::expect_used)]

use mini_chat_sdk::{EstimationBudgets, KillSwitches, ModelCatalogEntry};

use super::{
    ToolContext, ToolGates, effective_input_budget, estimate_text_tokens, max_output_applied,
    token_budget, tool_gates, tool_surcharges,
};
use crate::domain::error::DomainError;
use crate::test_support::catalog_entry;

fn budgets(bpt: u32, fixed: u32, margin: u32) -> EstimationBudgets {
    EstimationBudgets {
        bytes_per_token_conservative: bpt,
        fixed_overhead_tokens: fixed,
        safety_margin_pct: margin,
        ..EstimationBudgets::default()
    }
}

fn no_switches() -> KillSwitches {
    KillSwitches {
        disable_premium_tier: false,
        force_standard_tier: false,
        disable_web_search: false,
        disable_file_search: false,
        disable_images: false,
        disable_code_interpreter: false,
    }
}

fn sized_model(context_window: u32, max_output: u32, max_input: u32) -> ModelCatalogEntry {
    let mut m = catalog_entry("m", true);
    m.context_window = context_window;
    m.max_output_tokens = max_output;
    m.max_input_tokens = max_input;
    m.estimation_budgets.fixed_overhead_tokens = 100;
    m
}

#[test]
fn text_estimate_formula() {
    // ceil(400 / 4) + 100 = 200; ceil(200 * 110 / 100) = 220
    assert_eq!(estimate_text_tokens(400, &budgets(4, 100, 10)), 220);
}

#[test]
fn empty_message_gives_fixed_overhead_with_margin() {
    assert_eq!(estimate_text_tokens(0, &budgets(4, 100, 10)), 110);
}

#[test]
fn text_estimate_rounds_both_divisions_up() {
    // ceil(401 / 4) = 101; 101 + 100 = 201; ceil(201 * 110 / 100) = ceil(221.1) = 222
    assert_eq!(estimate_text_tokens(401, &budgets(4, 100, 10)), 222);
    // ceil(1 / 3) = 1; (1 + 0) * 100 / 100 = 1
    assert_eq!(estimate_text_tokens(1, &budgets(3, 0, 0)), 1);
}

#[test]
fn zero_bytes_per_token_is_clamped_to_one() {
    assert_eq!(estimate_text_tokens(10, &budgets(0, 0, 0)), 10);
}

#[test]
fn max_output_applied_is_min_of_model_and_cap() {
    let m = sized_model(128_000, 4096, 0);
    assert_eq!(max_output_applied(&m, 32_768), 4096);
    assert_eq!(max_output_applied(&m, 1000), 1000);
}

#[test]
fn effective_input_budget_caps_by_max_input_when_set() {
    // max_input 0 = no separate limit
    assert_eq!(
        effective_input_budget(&sized_model(4096, 1024, 0), 1024),
        3072
    );
    assert_eq!(
        effective_input_budget(&sized_model(4096, 1024, 2000), 1024),
        2000
    );
    assert_eq!(
        effective_input_budget(&sized_model(4096, 1024, 9000), 1024),
        3072
    );
}

#[test]
fn token_budget_subtracts_surcharges_and_fixed_overhead() {
    // min(3072, 4096 - 1024) - 500 - 100
    assert_eq!(
        token_budget(&sized_model(4096, 1024, 3072), 1024, 500).unwrap(),
        2472
    );
}

#[test]
fn token_budget_rejects_output_filling_the_window() {
    let m = sized_model(4096, 4096, 0);
    assert_eq!(
        token_budget(&m, 4096, 0),
        Err(DomainError::ContextBudgetExceeded)
    );
}

#[test]
fn token_budget_rejects_deductions_reaching_the_limit() {
    let m = sized_model(4096, 1024, 0);
    // 3072 - 2972 - 100 = 0
    assert_eq!(
        token_budget(&m, 1024, 2972),
        Err(DomainError::ContextBudgetExceeded)
    );
    assert_eq!(token_budget(&m, 1024, 2971).unwrap(), 1);
}

#[test]
fn tool_gates_follow_support_switches_and_chat_state() {
    let mut m = catalog_entry("m", true);
    m.general_config.tool_support.web_search = true;
    m.general_config.tool_support.file_search = true;
    m.general_config.tool_support.code_interpreter = true;
    let all_on = ToolContext {
        chat_has_ready_documents: true,
        chat_has_ready_ci_files: true,
        web_search_requested: true,
    };
    let ks = no_switches();
    assert_eq!(
        tool_gates(&m, &ks, &all_on),
        ToolGates {
            file_search: true,
            web_search: true,
            code_interpreter: true
        }
    );

    // nothing requested / no ready attachments
    let none = ToolContext {
        chat_has_ready_documents: false,
        chat_has_ready_ci_files: false,
        web_search_requested: false,
    };
    assert_eq!(
        tool_gates(&m, &ks, &none),
        ToolGates {
            file_search: false,
            web_search: false,
            code_interpreter: false
        }
    );

    // kill switches
    let off = KillSwitches {
        disable_web_search: true,
        disable_file_search: true,
        disable_code_interpreter: true,
        ..no_switches()
    };
    assert_eq!(
        tool_gates(&m, &off, &all_on),
        ToolGates {
            file_search: false,
            web_search: false,
            code_interpreter: false
        }
    );

    // catalog tool support
    let mut bare = m;
    bare.general_config.tool_support.web_search = false;
    bare.general_config.tool_support.file_search = false;
    bare.general_config.tool_support.code_interpreter = false;
    assert_eq!(
        tool_gates(&bare, &ks, &all_on),
        ToolGates {
            file_search: false,
            web_search: false,
            code_interpreter: false
        }
    );
}

#[test]
fn tool_surcharges_count_only_gated_tools() {
    let b = EstimationBudgets {
        tool_surcharge_tokens: 500,
        web_search_surcharge_tokens: 300,
        code_interpreter_surcharge_tokens: 1000,
        ..EstimationBudgets::default()
    };
    let gates = |f, w, c| ToolGates {
        file_search: f,
        web_search: w,
        code_interpreter: c,
    };
    assert_eq!(tool_surcharges(gates(false, false, false), &b), 0);
    assert_eq!(tool_surcharges(gates(true, false, false), &b), 500);
    assert_eq!(tool_surcharges(gates(false, true, false), &b), 300);
    assert_eq!(tool_surcharges(gates(false, false, true), &b), 1000);
    assert_eq!(tool_surcharges(gates(true, true, true), &b), 1800);
}
