use mini_chat_sdk::{EstimationBudgets, KillSwitches};

use super::{
    CreditError, ReserveInputs, ToolGates, candidate_reserve, credits_micro, estimate_text_tokens,
};
use crate::domain::test_fixtures::standard;

#[test]
fn credits_ceil_per_component() {
    assert_eq!(credits_micro(1, 1, 1_000_000, 3_000_000), Ok(1 + 3));
    assert_eq!(credits_micro(1500, 0, 1, 1), Ok(1));
    // Rounded per component, not on the sum (ceil(1e-6) + ceil(1e-6) = 2).
    assert_eq!(credits_micro(1, 1, 1, 1), Ok(2));
    assert_eq!(credits_micro(0, 0, 1, 1), Ok(0));
    assert_eq!(credits_micro(1_000_000, 0, 1, 1), Ok(1));
    assert_eq!(credits_micro(1_000_001, 0, 1, 1), Ok(2));
    // Bounds are inclusive.
    assert_eq!(
        credits_micro(10_000_000, 10_000_000, 10_000_000_000, 10_000_000_000),
        Ok(200_000_000_000)
    );

    assert_eq!(credits_micro(1, 1, 0, 1), Err(CreditError::ZeroMultiplier));
    assert_eq!(credits_micro(1, 1, 1, 0), Err(CreditError::ZeroMultiplier));
    assert_eq!(
        credits_micro(1, 1, 10_000_000_001, 1),
        Err(CreditError::MultiplierOutOfRange(10_000_000_001))
    );
    assert_eq!(
        credits_micro(1, 1, -1, 1),
        Err(CreditError::MultiplierOutOfRange(-1))
    );
    assert_eq!(
        credits_micro(10_000_001, 0, 1, 1),
        Err(CreditError::TokenCountOutOfRange(10_000_001))
    );
    assert_eq!(
        credits_micro(0, 10_000_001, 1, 1),
        Err(CreditError::TokenCountOutOfRange(10_000_001))
    );
    assert_eq!(
        credits_micro(-1, 0, 1, 1),
        Err(CreditError::TokenCountOutOfRange(-1))
    );
}

fn budgets(bpt: u32, overhead: u32, margin: u32) -> EstimationBudgets {
    EstimationBudgets {
        bytes_per_token_conservative: bpt,
        fixed_overhead_tokens: overhead,
        safety_margin_pct: margin,
        ..EstimationBudgets::default()
    }
}

#[test]
fn text_estimate() {
    // ceil((ceil(10 / 4) + 100) * 110 / 100) = ceil(113.3) = 114
    assert_eq!(estimate_text_tokens(10, &budgets(4, 100, 10)), 114);
    // Empty message: overhead before the margin.
    assert_eq!(estimate_text_tokens(0, &budgets(4, 100, 10)), 110);
    // Exact division stays exact.
    assert_eq!(estimate_text_tokens(400, &budgets(4, 0, 0)), 100);
    assert_eq!(estimate_text_tokens(401, &budgets(4, 0, 0)), 101);
    // bytes_per_token 0 is clamped to 1.
    assert_eq!(estimate_text_tokens(10, &budgets(0, 0, 0)), 10);
}

fn none() -> ReserveInputs {
    ReserveInputs::default()
}

/// Estimated input tokens of `standard("m")` (text estimate of an empty
/// message is 110) for the given inputs and kill switches.
fn est(m: &mini_chat_sdk::ModelCatalogEntry, i: &ReserveInputs, ks: KillSwitches) -> i64 {
    candidate_reserve(m, i, &ks, 32_768, 50).estimated_input_tokens
}

#[test]
fn surcharges_gated_by_tool_support_and_kill_switch() {
    let m = standard("m");
    let off = KillSwitches::default();
    assert_eq!(est(&m, &none(), off), 110);

    // file_search: ready docs AND tool_support.file_search AND !disable_file_search.
    let docs = ReserveInputs {
        chat_has_ready_docs: true,
        ..none()
    };
    assert_eq!(est(&m, &docs, off), 110 + 500);
    let mut without_file_search = m.clone();
    without_file_search.general_config.tool_support.file_search = false;
    assert_eq!(est(&without_file_search, &docs, off), 110);
    let ks_fs = KillSwitches {
        disable_file_search: true,
        ..off
    };
    assert_eq!(est(&m, &docs, ks_fs), 110);

    // code_interpreter: ready XLSX AND tool_support AND !disable_code_interpreter.
    let xlsx = ReserveInputs {
        chat_has_ready_xlsx: true,
        ..none()
    };
    assert_eq!(est(&m, &xlsx, off), 110 + 1000);
    let mut without_interpreter = m.clone();
    without_interpreter
        .general_config
        .tool_support
        .code_interpreter = false;
    assert_eq!(est(&without_interpreter, &xlsx, off), 110);
    let ks_ci = KillSwitches {
        disable_code_interpreter: true,
        ..off
    };
    assert_eq!(est(&m, &xlsx, ks_ci), 110);

    // web_search: requested AND tool_support.web_search.
    let ws = ReserveInputs {
        web_search_requested: true,
        ..none()
    };
    assert_eq!(est(&m, &ws, off), 110 + 500);
    let mut without_web_search = m.clone();
    without_web_search.general_config.tool_support.web_search = false;
    assert_eq!(est(&without_web_search, &ws, off), 110);

    // Images: count x image_token_budget; prior context is added as is.
    let imgs = ReserveInputs {
        prior_context_tokens: 300,
        image_count: 2,
        ..none()
    };
    assert_eq!(est(&m, &imgs, off), 110 + 2000 + 300);

    // Tool gates mirror the surcharges.
    let all = ReserveInputs {
        chat_has_ready_docs: true,
        chat_has_ready_xlsx: true,
        web_search_requested: true,
        ..none()
    };
    assert_eq!(
        candidate_reserve(&m, &all, &off, 32_768, 50).tools,
        ToolGates {
            file_search: true,
            web_search: true,
            code_interpreter: true,
        }
    );
    assert_eq!(
        candidate_reserve(&m, &none(), &off, 32_768, 50).tools,
        ToolGates::default()
    );
    assert_eq!(
        candidate_reserve(&without_web_search, &all, &ks_fs, 32_768, 50).tools,
        ToolGates {
            file_search: false,
            web_search: false,
            code_interpreter: true,
        }
    );
}

#[test]
fn reserve_plan_values() {
    let m = standard("m"); // max_output_tokens 4096, mults 1e6 / 3e6
    let i = ReserveInputs {
        message_bytes: 10,
        prior_context_tokens: 300,
        ..none()
    };
    let ks = KillSwitches::default();

    let p = candidate_reserve(&m, &i, &ks, 2048, 50);
    assert_eq!(p.estimated_input_tokens, 114 + 300);
    assert_eq!(p.max_output_tokens_applied, 2048); // min(4096, config 2048)
    assert_eq!(p.reserve_tokens, 414 + 2048);
    assert_eq!(p.reserved_credits_micro, 414 + 3 * 2048);
    assert_eq!(p.minimal_generation_floor_applied, 50);

    let p = candidate_reserve(&m, &i, &ks, 32_768, 5000);
    assert_eq!(p.max_output_tokens_applied, 4096); // model cap
    assert_eq!(p.minimal_generation_floor_applied, 4096); // min(floor, max_out)

    // A reserve that cannot be computed is i64::MAX (candidate unavailable).
    let mut zero = m.clone();
    zero.input_tokens_credit_multiplier_micro = 0;
    assert_eq!(
        candidate_reserve(&zero, &i, &ks, 2048, 50).reserved_credits_micro,
        i64::MAX
    );
    let mut huge = m;
    huge.output_tokens_credit_multiplier_micro = u64::MAX;
    assert_eq!(
        candidate_reserve(&huge, &i, &ks, 2048, 50).reserved_credits_micro,
        i64::MAX
    );
}
