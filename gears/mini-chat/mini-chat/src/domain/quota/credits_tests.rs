//! Credit arithmetic tests: the DESIGN 5.3 formula with per-component round-up division, the
//! worked values of DESIGN 5.10, every bound, the error order and the failure substitution
//! contract of the reserve.
//!
//! Slice S1-all: algorithm `cpt-cf-mini-chat-algo-quota-and-billing-credit-arithmetic`,
//! definition of done `cpt-cf-mini-chat-dod-quota-and-billing-credit-arithmetic`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::{CreditError, MAX_MULTIPLIER_MICRO, MAX_TOKEN_COUNT, credits_micro};

// ── worked values ────────────────────────────────────────────────────────────

#[test]
fn the_worked_value_of_the_premium_model_is_3_750_000() {
    // 1,000 input and 500 output tokens at 2.5 credits per 1K tokens
    assert_eq!(
        credits_micro(1_000, 500, 2_500_000_000, 2_500_000_000),
        Ok(3_750_000)
    );
}

#[test]
fn the_worked_value_of_the_standard_model_is_1_500_000() {
    assert_eq!(
        credits_micro(1_000, 500, 1_000_000_000, 1_000_000_000),
        Ok(1_500_000)
    );
}

#[test]
fn the_worked_value_of_the_settled_turn_is_1_200_000() {
    // 900 input and 300 output tokens at 1.0 credit per 1K tokens
    assert_eq!(
        credits_micro(900, 300, 1_000_000_000, 1_000_000_000),
        Ok(1_200_000)
    );
}

#[test]
fn a_multiplier_is_micro_credits_per_million_tokens() {
    // 1_000_000_000 charges one credit per 1,000 tokens
    assert_eq!(credits_micro(1_000, 0, 1_000_000_000, 1), Ok(1_000_000));
    // 1_000_000 charges one credit per 1,000,000 tokens
    assert_eq!(credits_micro(1_000_000, 0, 1_000_000, 1), Ok(1_000_000));
}

#[test]
fn input_and_output_use_their_own_multipliers() {
    // 1,000 * 2_000_000_000 / 1e6 = 2_000_000 and 1,000 * 500_000_000 / 1e6 = 500_000
    assert_eq!(
        credits_micro(1_000, 1_000, 2_000_000_000, 500_000_000),
        Ok(2_500_000)
    );
}

// ── rounding ─────────────────────────────────────────────────────────────────

#[test]
fn each_component_is_rounded_up_separately_and_never_on_the_sum() {
    // each product is 1 micro-credit-token: ceil(1 / 1e6) = 1 per component, 2 in total, while
    // rounding the sum would give ceil(2 / 1e6) = 1
    assert_eq!(credits_micro(1, 1, 1, 1), Ok(2));
}

#[test]
fn a_product_below_one_million_rounds_up_to_one() {
    assert_eq!(credits_micro(1, 0, 1, 1), Ok(1));
    assert_eq!(credits_micro(0, 1, 1, 1), Ok(1));
    assert_eq!(credits_micro(3, 0, 333_333, 1), Ok(1));
}

#[test]
fn a_product_just_above_a_multiple_rounds_to_the_next_unit() {
    // 3 * 333_334 = 1_000_002
    assert_eq!(credits_micro(3, 0, 333_334, 1), Ok(2));
    // exactly 1_000_000 does not round up
    assert_eq!(credits_micro(2, 0, 500_000, 1), Ok(1));
}

#[test]
fn zero_tokens_cost_nothing() {
    assert_eq!(credits_micro(0, 0, 1, 1), Ok(0));
    assert_eq!(
        credits_micro(0, 0, MAX_MULTIPLIER_MICRO, MAX_MULTIPLIER_MICRO),
        Ok(0)
    );
}

#[test]
fn the_rounding_of_the_components_differs_from_the_sum_where_the_design_says_it_may() {
    // 500_000 + 500_000 micro-credit-tokens: 1 + 1 = 2 per component, while the rounded sum
    // 1_000_000 / 1_000_000 would be 1
    assert_eq!(credits_micro(500_000, 500_000, 1, 1), Ok(2));
    // components that sum to exactly one unit: 400_000 + 600_000
    assert_eq!(credits_micro(400_000, 600_000, 1, 1), Ok(2));
}

// ── bounds ───────────────────────────────────────────────────────────────────

#[test]
fn the_upper_token_bound_is_inclusive() {
    assert_eq!(MAX_TOKEN_COUNT, 10_000_000);
    assert_eq!(credits_micro(MAX_TOKEN_COUNT, 0, 1, 1), Ok(10));
    assert_eq!(credits_micro(0, MAX_TOKEN_COUNT, 1, 1), Ok(10));
}

#[test]
fn a_token_count_above_ten_million_is_rejected_before_any_multiplication() {
    assert_eq!(
        credits_micro(10_000_001, 0, 1, 1),
        Err(CreditError::InvalidTokenCount { tokens: 10_000_001 })
    );
    assert_eq!(
        credits_micro(0, 10_000_001, 1, 1),
        Err(CreditError::InvalidTokenCount { tokens: 10_000_001 })
    );
}

#[test]
fn a_negative_token_count_is_rejected() {
    assert_eq!(
        credits_micro(-1, 0, 1, 1),
        Err(CreditError::InvalidTokenCount { tokens: -1 })
    );
    assert_eq!(
        credits_micro(0, -5, 1, 1),
        Err(CreditError::InvalidTokenCount { tokens: -5 })
    );
    assert_eq!(
        credits_micro(i64::MIN, 0, 1, 1),
        Err(CreditError::InvalidTokenCount { tokens: i64::MIN })
    );
    assert_eq!(
        credits_micro(i64::MAX, 0, 1, 1),
        Err(CreditError::InvalidTokenCount { tokens: i64::MAX })
    );
}

#[test]
fn the_error_names_the_offending_token_count() {
    let error = credits_micro(1, 20_000_000, 1, 1).unwrap_err();
    assert!(error.to_string().contains("20000000"), "{error}");
}

#[test]
fn a_zero_multiplier_is_a_zero_multiplier_error_not_a_free_turn() {
    assert_eq!(
        credits_micro(1_000, 1_000, 0, 1_000_000),
        Err(CreditError::ZeroMultiplier)
    );
    assert_eq!(
        credits_micro(1_000, 1_000, 1_000_000, 0),
        Err(CreditError::ZeroMultiplier)
    );
    assert_eq!(credits_micro(0, 0, 0, 0), Err(CreditError::ZeroMultiplier));
}

#[test]
fn a_multiplier_above_ten_billion_is_a_separate_out_of_range_error() {
    assert_eq!(MAX_MULTIPLIER_MICRO, 10_000_000_000);
    assert_eq!(
        credits_micro(1, 1, 10_000_000_001, 1),
        Err(CreditError::MultiplierOutOfRange {
            multiplier: 10_000_000_001
        })
    );
    assert_eq!(
        credits_micro(1, 1, 1, 10_000_000_001),
        Err(CreditError::MultiplierOutOfRange {
            multiplier: 10_000_000_001
        })
    );
    assert_eq!(
        credits_micro(1, 1, u64::MAX, 1),
        Err(CreditError::MultiplierOutOfRange {
            multiplier: u64::MAX
        })
    );
}

#[test]
fn the_upper_multiplier_bound_is_inclusive() {
    assert_eq!(
        credits_micro(1_000_000, 0, MAX_MULTIPLIER_MICRO, 1),
        Ok(10_000_000_000)
    );
}

#[test]
fn the_largest_valid_inputs_do_not_overflow() {
    // 2 * (10^7 * 10^10 / 10^6) = 2 * 10^11
    assert_eq!(
        credits_micro(
            MAX_TOKEN_COUNT,
            MAX_TOKEN_COUNT,
            MAX_MULTIPLIER_MICRO,
            MAX_MULTIPLIER_MICRO
        ),
        Ok(200_000_000_000)
    );
}

#[test]
fn the_token_bounds_are_checked_before_the_multiplier_bounds() {
    // an invalid token count and a zero multiplier: the token count is reported
    assert_eq!(
        credits_micro(-1, 0, 0, 0),
        Err(CreditError::InvalidTokenCount { tokens: -1 })
    );
    // the input count is checked before the output count
    assert_eq!(
        credits_micro(10_000_001, -1, 1, 1),
        Err(CreditError::InvalidTokenCount { tokens: 10_000_001 })
    );
}

#[test]
fn the_multiplier_failure_is_reported_even_for_zero_tokens() {
    // both checks run before any multiplication, so zero tokens do not hide a bad multiplier
    assert_eq!(credits_micro(0, 0, 0, 1), Err(CreditError::ZeroMultiplier));
    assert_eq!(
        credits_micro(0, 0, 1, 10_000_000_001),
        Err(CreditError::MultiplierOutOfRange {
            multiplier: 10_000_000_001
        })
    );
}

// ── determinism ──────────────────────────────────────────────────────────────

#[test]
fn the_computation_is_pure_and_repeatable() {
    let first = credits_micro(123_456, 7_890, 3_300_000_000, 9_900_000_000);
    for _ in 0..3 {
        assert_eq!(
            credits_micro(123_456, 7_890, 3_300_000_000, 9_900_000_000),
            first
        );
    }
    assert!(first.is_ok());
}
