#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 2: `cpt-cf-mini-chat-algo-quota-billing-estimate-reserve` and
//! `cpt-cf-mini-chat-dod-quota-billing-token-estimation`.

use mini_chat_sdk::{KillSwitches, ModelCatalogEntry, ModelTier};
use tracing::Level;

use super::{EstimateInput, ReserveEstimate, estimate_reserve};
use crate::domain::service::quota::test_support::{NO_SWITCHES, model};
use crate::test_doubles::{LogCapture, entry, entry_json};

/// A premium candidate with the budgets of the DESIGN example: ratio 4,
/// overhead 100, margin 10; image 1000, tool 500, web 500, code 1000;
/// multipliers `1_000_000_000` both; `max_output_tokens` 500.
fn candidate() -> ModelCatalogEntry {
    model("m", ModelTier::Premium)
        .text_budgets(4, 100, 10)
        .build()
}

fn input<'a>(
    candidate: &'a ModelCatalogEntry,
    switches: &'a KillSwitches,
    message_bytes: u64,
) -> EstimateInput<'a> {
    EstimateInput {
        candidate,
        kill_switches: switches,
        message_bytes,
        image_count: 0,
        web_search_enabled: false,
        has_ready_file_search_documents: false,
        has_ready_code_interpreter_files: false,
        prior_context_tokens: 0,
        streaming_max_output_tokens: 32768,
        minimal_generation_floor: 50,
    }
}

fn estimate(candidate: &ModelCatalogEntry, message_bytes: u64) -> ReserveEstimate {
    estimate_reserve(&input(candidate, &NO_SWITCHES, message_bytes))
}

// ---- text estimate ---------------------------------------------------------------------------------

#[test]
fn a_thousand_bytes_with_ratio_4_overhead_100_margin_10_estimate_385_text_tokens() {
    let result = estimate(&candidate(), 1000);

    assert_eq!(result.estimated_input_tokens, 385);
    assert_eq!(result.max_output_tokens_applied, 500);
    assert_eq!(result.reserve_tokens, 885);
}

#[test]
fn an_empty_message_estimates_the_fixed_overhead_with_the_margin() {
    let result = estimate(&candidate(), 0);

    assert_eq!(result.estimated_input_tokens, 110);
}

#[test]
fn the_byte_division_and_the_margin_both_round_up() {
    let rounded_bytes = model("m", ModelTier::Standard)
        .text_budgets(4, 0, 0)
        .build();
    // ceil(5 / 4) = 2
    assert_eq!(estimate(&rounded_bytes, 5).estimated_input_tokens, 2);

    let rounded_margin = model("m", ModelTier::Standard)
        .text_budgets(1, 0, 10)
        .build();
    // ceil(1 * 110 / 100) = 2
    assert_eq!(estimate(&rounded_margin, 1).estimated_input_tokens, 2);

    let overhead_only = model("m", ModelTier::Standard)
        .text_budgets(4, 5, 10)
        .build();
    // ceil(5 * 110 / 100) = ceil(5.5) = 6
    assert_eq!(estimate(&overhead_only, 0).estimated_input_tokens, 6);
}

#[test]
fn a_zero_bytes_per_token_ratio_is_clamped_to_one() {
    let zero_ratio = model("m", ModelTier::Standard)
        .text_budgets(0, 0, 0)
        .build();

    assert_eq!(estimate(&zero_ratio, 1000).estimated_input_tokens, 1000);
}

#[test]
fn the_budgets_decoded_by_default_apply_when_the_entry_omits_them() {
    // `entry_json` has no `estimation_budgets`: ratio 4, overhead 100, margin 10.
    let bare = entry(entry_json("bare", true));

    assert_eq!(estimate(&bare, 1000).estimated_input_tokens, 385);
}

#[test]
fn each_candidate_uses_its_own_budgets() {
    let tight = model("a", ModelTier::Premium).text_budgets(1, 0, 0).build();
    let loose = model("b", ModelTier::Standard)
        .text_budgets(8, 0, 0)
        .build();

    assert_eq!(estimate(&tight, 800).estimated_input_tokens, 800);
    assert_eq!(estimate(&loose, 800).estimated_input_tokens, 100);
}

// ---- surcharges ------------------------------------------------------------------------------------

#[test]
fn images_add_the_per_image_budget_each() {
    let candidate = candidate();
    let mut request = input(&candidate, &NO_SWITCHES, 1000);
    request.image_count = 3;

    assert_eq!(
        estimate_reserve(&request).estimated_input_tokens,
        385 + 3 * 1000
    );
}

#[test]
fn the_file_search_surcharge_needs_documents_tool_support_and_the_switch_off() {
    let supported = model("m", ModelTier::Premium)
        .text_budgets(1, 0, 0)
        .tools(false, true, false)
        .build();
    let unsupported = model("m", ModelTier::Premium)
        .text_budgets(1, 0, 0)
        .tools(true, false, true)
        .build();
    let disabled = KillSwitches {
        disable_file_search: true,
        ..NO_SWITCHES
    };
    let cases = [
        (&supported, &NO_SWITCHES, true, 1000 + 500),
        (&supported, &NO_SWITCHES, false, 1000),
        (&unsupported, &NO_SWITCHES, true, 1000),
        (&supported, &disabled, true, 1000),
    ];
    for (candidate, switches, documents, expected) in cases {
        let mut request = input(candidate, switches, 1000);
        request.has_ready_file_search_documents = documents;

        assert_eq!(
            estimate_reserve(&request).estimated_input_tokens,
            expected,
            "documents={documents}, switches={switches:?}"
        );
    }
}

#[test]
fn the_web_search_surcharge_needs_the_request_flag_and_tool_support() {
    let supported = model("m", ModelTier::Premium)
        .text_budgets(1, 0, 0)
        .tools(true, false, false)
        .build();
    let unsupported = model("m", ModelTier::Premium)
        .text_budgets(1, 0, 0)
        .tools(false, true, true)
        .build();
    for (candidate, enabled, expected) in [
        (&supported, true, 1000 + 500),
        (&supported, false, 1000),
        (&unsupported, true, 1000),
    ] {
        let mut request = input(candidate, &NO_SWITCHES, 1000);
        request.web_search_enabled = enabled;

        assert_eq!(
            estimate_reserve(&request).estimated_input_tokens,
            expected,
            "enabled={enabled}"
        );
    }
}

#[test]
fn the_code_interpreter_surcharge_needs_ready_files_support_and_the_switch_off() {
    let supported = model("m", ModelTier::Premium)
        .text_budgets(1, 0, 0)
        .tools(false, false, true)
        .build();
    let unsupported = model("m", ModelTier::Premium)
        .text_budgets(1, 0, 0)
        .tools(true, true, false)
        .build();
    let disabled = KillSwitches {
        disable_code_interpreter: true,
        ..NO_SWITCHES
    };
    let cases = [
        (&supported, &NO_SWITCHES, true, 1000 + 1000),
        (&supported, &NO_SWITCHES, false, 1000),
        (&unsupported, &NO_SWITCHES, true, 1000),
        (&supported, &disabled, true, 1000),
    ];
    for (candidate, switches, files, expected) in cases {
        let mut request = input(candidate, switches, 1000);
        request.has_ready_code_interpreter_files = files;

        assert_eq!(
            estimate_reserve(&request).estimated_input_tokens,
            expected,
            "files={files}, switches={switches:?}"
        );
    }
}

#[test]
fn prior_context_and_every_surcharge_add_up_once_per_turn() {
    let candidate = model("m", ModelTier::Premium)
        .text_budgets(1, 0, 0)
        .surcharges(1000, 500, 300, 700)
        .build();
    let mut request = input(&candidate, &NO_SWITCHES, 1000);
    request.image_count = 2;
    request.web_search_enabled = true;
    request.has_ready_file_search_documents = true;
    request.has_ready_code_interpreter_files = true;
    request.prior_context_tokens = 250;

    let result = estimate_reserve(&request);

    // text 1000 + prior 250 + images 2000 + file 500 + web 300 + code 700
    assert_eq!(result.estimated_input_tokens, 4750);
    assert_eq!(result.reserve_tokens, 4750 + 500);
}

// ---- output cap, floor, credits ----------------------------------------------------------------------

#[test]
fn max_output_tokens_applied_is_the_smaller_of_the_catalog_and_the_gear_cap() {
    let candidate = model("m", ModelTier::Premium).max_output(4096).build();

    let mut request = input(&candidate, &NO_SWITCHES, 0);
    request.streaming_max_output_tokens = 2048;
    assert_eq!(estimate_reserve(&request).max_output_tokens_applied, 2048);

    request.streaming_max_output_tokens = 32768;
    assert_eq!(estimate_reserve(&request).max_output_tokens_applied, 4096);

    request.streaming_max_output_tokens = 4096;
    assert_eq!(estimate_reserve(&request).max_output_tokens_applied, 4096);
}

#[test]
fn the_reserve_is_the_estimated_input_plus_the_applied_output_cap() {
    let candidate = model("m", ModelTier::Premium)
        .text_budgets(1, 0, 0)
        .max_output(500)
        .build();

    let result = estimate(&candidate, 1000);

    assert_eq!(
        result.reserve_tokens,
        result.estimated_input_tokens + result.max_output_tokens_applied
    );
    assert_eq!(result.reserve_tokens, 1500);
}

#[test]
fn the_floor_is_the_smaller_of_the_configured_floor_and_the_applied_cap() {
    let candidate = model("m", ModelTier::Premium).max_output(500).build();

    let mut request = input(&candidate, &NO_SWITCHES, 0);
    request.minimal_generation_floor = 50;
    assert_eq!(
        estimate_reserve(&request).minimal_generation_floor_applied,
        50
    );

    request.minimal_generation_floor = 1000;
    assert_eq!(
        estimate_reserve(&request).minimal_generation_floor_applied,
        500,
        "capped by max_output_tokens_applied"
    );

    let small_output = model("m", ModelTier::Premium).max_output(30).build();
    let mut request = input(&small_output, &NO_SWITCHES, 0);
    request.minimal_generation_floor = 50;
    assert_eq!(
        estimate_reserve(&request).minimal_generation_floor_applied,
        30
    );
}

#[test]
fn the_catalog_floor_budget_is_never_read() {
    let mut json = entry_json("m", true);
    json["estimation_budgets"] = serde_json::json!({ "minimal_generation_floor": 999 });
    let candidate = entry(json);

    let mut request = input(&candidate, &NO_SWITCHES, 0);
    request.minimal_generation_floor = 50;

    assert_eq!(
        estimate_reserve(&request).minimal_generation_floor_applied,
        50
    );
}

#[test]
fn the_reserved_credits_use_the_estimated_input_the_output_cap_and_the_candidates_multipliers() {
    // DESIGN 5.10: 1000 estimated input and 500 output tokens.
    let premium = model("p", ModelTier::Premium)
        .text_budgets(1, 0, 0)
        .mult(2_500_000_000, 2_500_000_000)
        .build();
    let standard = model("s", ModelTier::Standard)
        .text_budgets(1, 0, 0)
        .mult(1_000_000_000, 1_000_000_000)
        .build();

    assert_eq!(estimate(&premium, 1000).reserved_credits_micro, 3_750_000);
    assert_eq!(estimate(&standard, 1000).reserved_credits_micro, 1_500_000);
}

#[test]
fn the_input_and_output_multipliers_are_applied_per_component() {
    let candidate = model("m", ModelTier::Premium)
        .text_budgets(1, 0, 0)
        .mult(2_000_000_000, 4_000_000_000)
        .build();

    // 1000 * 2 credits per 1K + 500 * 4 credits per 1K = 2_000_000 + 2_000_000
    assert_eq!(estimate(&candidate, 1000).reserved_credits_micro, 4_000_000);
}

#[test]
fn estimating_twice_gives_the_same_values() {
    let candidate = candidate();

    assert_eq!(estimate(&candidate, 1234), estimate(&candidate, 1234));
}

// ---- overflow and credit errors ----------------------------------------------------------------------

#[test]
fn a_sum_that_overflows_books_the_largest_credit_value_and_warns_naming_the_model() {
    let capture = LogCapture::start();
    let candidate = model("overflow-model", ModelTier::Premium).build();
    let mut request = input(&candidate, &NO_SWITCHES, 1000);
    request.prior_context_tokens = i64::MAX;

    let result = estimate_reserve(&request);

    assert_eq!(result.reserved_credits_micro, i64::MAX);
    assert!(
        !capture
            .at_containing(Level::WARN, "overflow-model")
            .is_empty(),
        "{}",
        capture.all_text()
    );
}

#[test]
fn a_credit_error_books_the_largest_credit_value_instead_of_failing() {
    let capture = LogCapture::start();
    let broken = [
        ("zero-mult", 0_u64, 1_000_000_000_u64),
        ("huge-mult", 1_000_000_000, 10_000_000_001),
    ];
    for (id, in_mult, out_mult) in broken {
        let candidate = model(id, ModelTier::Premium)
            .text_budgets(1, 0, 0)
            .mult(in_mult, out_mult)
            .build();

        let result = estimate(&candidate, 1000);

        assert_eq!(result.reserved_credits_micro, i64::MAX, "{id}");
        assert_eq!(result.estimated_input_tokens, 1000, "{id}");
        assert_eq!(result.reserve_tokens, 1500, "{id}");
        assert!(
            !capture.at_containing(Level::WARN, id).is_empty(),
            "{id}: {}",
            capture.all_text()
        );
    }
}

#[test]
fn more_than_ten_million_estimated_input_tokens_is_a_credit_error_not_a_panic() {
    let candidate = model("m", ModelTier::Premium).text_budgets(1, 0, 0).build();
    let mut request = input(&candidate, &NO_SWITCHES, 0);
    request.prior_context_tokens = 10_000_001;

    assert_eq!(estimate_reserve(&request).reserved_credits_micro, i64::MAX);
}
