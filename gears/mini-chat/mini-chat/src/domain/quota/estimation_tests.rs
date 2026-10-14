//! Token estimation and reserve calculation tests: the text formula and its rounding edges,
//! the per-turn surcharges and their gates, the budgets source, the input limit check, the
//! reserve numbers of DESIGN 5.10 and the substitution of a failed credit computation.
//!
//! Slice S1-all: algorithms `cpt-cf-mini-chat-algo-quota-and-billing-token-estimation` and
//! `cpt-cf-mini-chat-algo-quota-and-billing-reserve-calculation`, definition of done
//! `cpt-cf-mini-chat-dod-quota-and-billing-estimation-reserve`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mini_chat_sdk::{EstimationBudgets, KillSwitches, ModelCatalogEntry, ModelTier};
use serde_json::{Value, json};

use super::{RequestFacts, calculate_reserve, estimate_tokens, input_too_long};
use crate::test_support::entry;

fn model_with(budgets: &Value) -> ModelCatalogEntry {
    entry(
        "m",
        ModelTier::Standard,
        true,
        &json!({ "estimation_budgets": budgets }),
    )
}

fn default_model() -> ModelCatalogEntry {
    entry("m", ModelTier::Standard, true, &json!({}))
}

fn tools(mut model: ModelCatalogEntry, web: bool, file: bool, code: bool) -> ModelCatalogEntry {
    model.general_config.tool_support.web_search = web;
    model.general_config.tool_support.file_search = file;
    model.general_config.tool_support.code_interpreter = code;
    model
}

fn facts(bytes: u64) -> RequestFacts {
    RequestFacts {
        message_bytes: bytes,
        image_count: 0,
        ready_documents: 0,
        ready_xlsx_attachments: 0,
        web_search_enabled: false,
    }
}

fn text_tokens(model: &ModelCatalogEntry, bytes: u64) -> i64 {
    estimate_tokens(model, &KillSwitches::default(), &facts(bytes), 0).estimated_text_tokens
}

// ── the text formula ─────────────────────────────────────────────────────────

#[test]
fn the_omitted_budgets_of_an_entry_are_the_documented_defaults() {
    let model = default_model();
    assert_eq!(model.estimation_budgets, EstimationBudgets::default());
    assert_eq!(model.estimation_budgets.bytes_per_token_conservative, 4);
    assert_eq!(model.estimation_budgets.fixed_overhead_tokens, 100);
    assert_eq!(model.estimation_budgets.safety_margin_pct, 10);
}

#[test]
fn the_text_estimate_follows_the_formula_with_the_default_budgets() {
    // ceil(4000 / 4) + 100 = 1100; 1100 * 110 / 100 = 1210
    assert_eq!(text_tokens(&default_model(), 4000), 1210);
}

#[test]
fn the_margin_is_applied_with_a_ceiling_after_multiplying_first() {
    // ceil(4001 / 4) + 100 = 1101; 1101 * 110 = 121_110; ceil(121_110 / 100) = 1212
    assert_eq!(text_tokens(&default_model(), 4001), 1212);
    // ceil(1 / 4) + 100 = 101; 101 * 110 = 11_110; ceil(11_110 / 100) = 112
    assert_eq!(text_tokens(&default_model(), 1), 112);
}

#[test]
fn an_empty_message_yields_the_fixed_overhead_before_the_margin() {
    assert_eq!(
        text_tokens(
            &model_with(&json!({ "fixed_overhead_tokens": 100, "safety_margin_pct": 0 })),
            0
        ),
        100
    );
    // with the default margin the overhead is then raised by it
    assert_eq!(text_tokens(&default_model(), 0), 110);
    assert_eq!(
        text_tokens(
            &model_with(&json!({ "fixed_overhead_tokens": 0, "safety_margin_pct": 10 })),
            0
        ),
        0
    );
}

#[test]
fn the_byte_division_rounds_up_at_every_edge() {
    let model = model_with(&json!({
        "bytes_per_token_conservative": 4,
        "fixed_overhead_tokens": 0,
        "safety_margin_pct": 0
    }));
    assert_eq!(text_tokens(&model, 1), 1);
    assert_eq!(text_tokens(&model, 3), 1);
    assert_eq!(text_tokens(&model, 4), 1);
    assert_eq!(text_tokens(&model, 5), 2);
    assert_eq!(text_tokens(&model, 8), 2);
    assert_eq!(text_tokens(&model, 9), 3);
}

#[test]
fn a_bytes_per_token_of_zero_from_a_foreign_snapshot_is_clamped_to_one() {
    let model = model_with(&json!({
        "bytes_per_token_conservative": 0,
        "fixed_overhead_tokens": 0,
        "safety_margin_pct": 0
    }));
    assert_eq!(text_tokens(&model, 10), 10);
}

#[test]
fn the_margin_multiplies_before_it_divides() {
    // base 3, margin 50: 3 * 150 = 450; ceil(450 / 100) = 5 (dividing first would give 3 or 4)
    let model = model_with(&json!({
        "bytes_per_token_conservative": 1,
        "fixed_overhead_tokens": 0,
        "safety_margin_pct": 50
    }));
    assert_eq!(text_tokens(&model, 3), 5);
    // a margin of 100 percent doubles
    let doubled = model_with(&json!({
        "bytes_per_token_conservative": 1,
        "fixed_overhead_tokens": 0,
        "safety_margin_pct": 100
    }));
    assert_eq!(text_tokens(&doubled, 7), 14);
}

#[test]
fn a_huge_message_neither_wraps_nor_panics() {
    let tokens = text_tokens(&default_model(), u64::MAX);
    assert!(tokens > 0, "{tokens}");
    assert!(text_tokens(&default_model(), 4_000_000_000) > 1_000_000_000);
}

// ── prior context and images ─────────────────────────────────────────────────

#[test]
fn the_prior_context_tokens_are_added_as_given() {
    let estimate = estimate_tokens(
        &default_model(),
        &KillSwitches::default(),
        &facts(4000),
        777,
    );
    assert_eq!(estimate.prior_context_tokens, 777);
    assert_eq!(estimate.estimated_input_tokens, 1210 + 777);
}

#[test]
fn every_image_costs_the_image_token_budget_of_the_entry() {
    let model = model_with(&json!({ "image_token_budget": 750 }));
    let with_images = RequestFacts {
        image_count: 3,
        ..facts(0)
    };
    let estimate = estimate_tokens(&model, &KillSwitches::default(), &with_images, 0);
    assert_eq!(estimate.image_surcharge_tokens, 2250);
    let none = estimate_tokens(&model, &KillSwitches::default(), &facts(0), 0);
    assert_eq!(none.image_surcharge_tokens, 0);
}

// ── surcharges and their gates ───────────────────────────────────────────────

#[test]
fn the_file_search_surcharge_needs_a_ready_document_the_tool_support_and_no_kill_switch() {
    let docs = RequestFacts {
        ready_documents: 2,
        ..facts(0)
    };
    let off = KillSwitches::default();
    let supported = tools(default_model(), false, true, false);
    assert_eq!(
        estimate_tokens(&supported, &off, &docs, 0).tool_surcharge_tokens,
        500
    );
    // no ready document
    assert_eq!(
        estimate_tokens(&supported, &off, &facts(0), 0).tool_surcharge_tokens,
        0
    );
    // the candidate does not support file search
    let unsupported = tools(default_model(), true, false, true);
    assert_eq!(
        estimate_tokens(&unsupported, &off, &docs, 0).tool_surcharge_tokens,
        0
    );
    // the kill switch leaves the tool out of the provider request
    let killed = KillSwitches {
        disable_file_search: true,
        ..KillSwitches::default()
    };
    assert_eq!(
        estimate_tokens(&supported, &killed, &docs, 0).tool_surcharge_tokens,
        0
    );
}

#[test]
fn the_web_search_surcharge_needs_the_request_flag_and_the_tool_support() {
    let flagged = RequestFacts {
        web_search_enabled: true,
        ..facts(0)
    };
    let off = KillSwitches::default();
    let supported = tools(default_model(), true, false, false);
    assert_eq!(
        estimate_tokens(&supported, &off, &flagged, 0).web_search_surcharge_tokens,
        500
    );
    // the flag is off
    assert_eq!(
        estimate_tokens(&supported, &off, &facts(0), 0).web_search_surcharge_tokens,
        0
    );
    // the candidate has no web search: the tool is not sent, so no surcharge is reserved
    let unsupported = tools(default_model(), false, true, true);
    assert_eq!(
        estimate_tokens(&unsupported, &off, &flagged, 0).web_search_surcharge_tokens,
        0
    );
}

#[test]
fn the_code_interpreter_surcharge_needs_a_ready_xlsx_the_tool_support_and_no_kill_switch() {
    let xlsx = RequestFacts {
        ready_xlsx_attachments: 1,
        ..facts(0)
    };
    let off = KillSwitches::default();
    let supported = tools(default_model(), false, false, true);
    assert_eq!(
        estimate_tokens(&supported, &off, &xlsx, 0).code_interpreter_surcharge_tokens,
        1000
    );
    assert_eq!(
        estimate_tokens(&supported, &off, &facts(0), 0).code_interpreter_surcharge_tokens,
        0
    );
    let unsupported = tools(default_model(), true, true, false);
    assert_eq!(
        estimate_tokens(&unsupported, &off, &xlsx, 0).code_interpreter_surcharge_tokens,
        0
    );
    let killed = KillSwitches {
        disable_code_interpreter: true,
        ..KillSwitches::default()
    };
    assert_eq!(
        estimate_tokens(&supported, &killed, &xlsx, 0).code_interpreter_surcharge_tokens,
        0
    );
}

#[test]
fn a_surcharge_is_a_fixed_per_turn_addition_that_never_scales_with_the_counts() {
    let model = tools(default_model(), true, true, true);
    let off = KillSwitches::default();
    let one = RequestFacts {
        ready_documents: 1,
        ready_xlsx_attachments: 1,
        web_search_enabled: true,
        ..facts(100)
    };
    let many = RequestFacts {
        ready_documents: 50,
        ready_xlsx_attachments: 9,
        ..one
    };
    assert_eq!(
        estimate_tokens(&model, &off, &one, 0),
        estimate_tokens(&model, &off, &many, 0)
    );
}

#[test]
fn the_estimated_input_is_the_sum_of_the_six_components() {
    let model = tools(default_model(), true, true, true);
    let all = RequestFacts {
        message_bytes: 4000,
        image_count: 2,
        ready_documents: 1,
        ready_xlsx_attachments: 1,
        web_search_enabled: true,
    };
    let estimate = estimate_tokens(&model, &KillSwitches::default(), &all, 300);
    assert_eq!(estimate.estimated_text_tokens, 1210);
    assert_eq!(estimate.prior_context_tokens, 300);
    assert_eq!(estimate.image_surcharge_tokens, 2000);
    assert_eq!(estimate.tool_surcharge_tokens, 500);
    assert_eq!(estimate.web_search_surcharge_tokens, 500);
    assert_eq!(estimate.code_interpreter_surcharge_tokens, 1000);
    assert_eq!(
        estimate.estimated_input_tokens,
        1210 + 300 + 2000 + 500 + 500 + 1000
    );
}

// ── budgets source and determinism ───────────────────────────────────────────

#[test]
fn every_budget_comes_from_the_entry_it_is_estimated_for() {
    let cheap = model_with(&json!({
        "bytes_per_token_conservative": 8,
        "fixed_overhead_tokens": 10,
        "safety_margin_pct": 0
    }));
    let dear = model_with(&json!({
        "bytes_per_token_conservative": 2,
        "fixed_overhead_tokens": 200,
        "safety_margin_pct": 50
    }));
    // ceil(800 / 8) + 10 = 110 against (400 + 200) * 150 / 100 = 900
    assert_eq!(text_tokens(&cheap, 800), 110);
    assert_eq!(text_tokens(&dear, 800), 900);
}

#[test]
fn the_same_inputs_always_give_the_same_estimate() {
    let model = tools(default_model(), true, true, true);
    let all = RequestFacts {
        message_bytes: 12_345,
        image_count: 1,
        ready_documents: 3,
        ready_xlsx_attachments: 2,
        web_search_enabled: true,
    };
    let first = estimate_tokens(&model, &KillSwitches::default(), &all, 42);
    assert_eq!(
        estimate_tokens(&model, &KillSwitches::default(), &all, 42),
        first
    );
}

// ── input limit ──────────────────────────────────────────────────────────────

#[test]
fn a_max_input_tokens_of_zero_means_no_separate_limit() {
    let mut model = default_model();
    model.max_input_tokens = 0;
    assert!(!input_too_long(&model, 1_000_000_000));
}

#[test]
fn the_input_limit_is_exceeded_only_above_max_input_tokens() {
    let mut model = default_model();
    model.max_input_tokens = 1000;
    assert!(!input_too_long(&model, 999));
    assert!(!input_too_long(&model, 1000));
    assert!(input_too_long(&model, 1001));
}

// ── reserve calculation ──────────────────────────────────────────────────────

fn priced(multiplier: u64, max_output_tokens: u32) -> ModelCatalogEntry {
    entry(
        "m",
        ModelTier::Standard,
        true,
        &json!({
            "input_tokens_credit_multiplier_micro": multiplier,
            "output_tokens_credit_multiplier_micro": multiplier,
            "max_output_tokens": max_output_tokens,
        }),
    )
}

#[test]
fn the_premium_reserve_of_the_design_example() {
    let reserve = calculate_reserve(&priced(2_500_000_000, 500), 1000, 32_768, 50);
    assert_eq!(reserve.max_output_tokens_applied, 500);
    assert_eq!(reserve.estimated_input_tokens, 1000);
    assert_eq!(reserve.reserve_tokens, 1500);
    assert_eq!(reserve.reserved_credits_micro, 3_750_000);
    assert!(!reserve.credit_computation_failed);
}

#[test]
fn the_standard_reserve_of_the_design_example() {
    let reserve = calculate_reserve(&priced(1_000_000_000, 500), 1000, 32_768, 50);
    assert_eq!(reserve.reserve_tokens, 1500);
    assert_eq!(reserve.reserved_credits_micro, 1_500_000);
    assert!(!reserve.credit_computation_failed);
}

#[test]
fn the_output_cap_is_the_smaller_of_the_catalog_and_the_streaming_limit() {
    let catalog_smaller = calculate_reserve(&priced(1_000_000, 4096), 10, 32_768, 50);
    assert_eq!(catalog_smaller.max_output_tokens_applied, 4096);
    let streaming_smaller = calculate_reserve(&priced(1_000_000, 100_000), 10, 32_768, 50);
    assert_eq!(streaming_smaller.max_output_tokens_applied, 32_768);
    let equal = calculate_reserve(&priced(1_000_000, 2048), 10, 2048, 50);
    assert_eq!(equal.max_output_tokens_applied, 2048);
    assert_eq!(equal.reserve_tokens, 10 + 2048);
}

#[test]
fn the_floor_is_the_smaller_of_the_configured_floor_and_the_output_cap() {
    assert_eq!(
        calculate_reserve(&priced(1_000_000, 500), 10, 32_768, 50).minimal_generation_floor_applied,
        50
    );
    assert_eq!(
        calculate_reserve(&priced(1_000_000, 30), 10, 32_768, 50).minimal_generation_floor_applied,
        30
    );
    assert_eq!(
        calculate_reserve(&priced(1_000_000, 4096), 10, 32_768, 100)
            .minimal_generation_floor_applied,
        100
    );
}

#[test]
fn the_floor_comes_from_the_configuration_never_from_the_catalog_entry() {
    let mut model = priced(1_000_000, 4096);
    model.estimation_budgets.minimal_generation_floor = 999;
    let reserve = calculate_reserve(&model, 10, 32_768, 50);
    assert_eq!(reserve.minimal_generation_floor_applied, 50);
}

#[test]
fn a_failed_credit_computation_is_marked_and_priced_at_i64_max() {
    // a zero multiplier
    let zero = calculate_reserve(&priced(0, 500), 1000, 32_768, 50);
    assert!(zero.credit_computation_failed);
    assert_eq!(zero.reserved_credits_micro, i64::MAX);
    assert_eq!(
        zero.reserve_tokens, 1500,
        "the token reserve is still computed"
    );

    // a multiplier above the range
    let high = calculate_reserve(&priced(10_000_000_001, 500), 1000, 32_768, 50);
    assert!(high.credit_computation_failed);
    assert_eq!(high.reserved_credits_micro, i64::MAX);

    // an estimate above the token bound
    let huge = calculate_reserve(&priced(1_000_000, 500), 10_000_001, 32_768, 50);
    assert!(huge.credit_computation_failed);
    assert_eq!(huge.reserved_credits_micro, i64::MAX);
}

#[test]
fn a_computable_reserve_is_never_marked_failed_even_when_large() {
    let big = calculate_reserve(&priced(10_000_000_000, 32_768), 9_000_000, 32_768, 50);
    assert!(!big.credit_computation_failed);
    assert!(big.reserved_credits_micro < i64::MAX);
}

#[test]
fn the_availability_check_and_the_booked_reserve_use_the_same_formula() {
    let model = priced(2_500_000_000, 500);
    assert_eq!(
        calculate_reserve(&model, 1000, 32_768, 50),
        calculate_reserve(&model, 1000, 32_768, 50)
    );
}
