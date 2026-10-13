#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 1: `cpt-cf-mini-chat-algo-quota-billing-credits-micro` and
//! `cpt-cf-mini-chat-dod-quota-billing-credit-arithmetic`.

use super::{CreditError, MAX_MULTIPLIER_MICRO, MAX_TOKEN_COUNT, credits_micro};

/// Exact oracle in 128-bit integers.
fn oracle(input: i64, output: i64, in_mult: u64, out_mult: u64) -> i128 {
    fn ceil(tokens: i64, mult: u64) -> i128 {
        let product = i128::from(tokens) * i128::from(mult);
        (product + 999_999).div_euclid(1_000_000)
    }
    ceil(input, in_mult) + ceil(output, out_mult)
}

#[test]
fn the_limits_are_the_documented_ones() {
    assert_eq!(MAX_TOKEN_COUNT, 10_000_000);
    assert_eq!(MAX_MULTIPLIER_MICRO, 10_000_000_000);
}

// ---- per-component ceiling ---------------------------------------------------------------------

#[test]
fn one_token_each_at_multiplier_one_costs_two_micro_credits() {
    assert_eq!(credits_micro(1, 1, 1, 1), Ok(2));
}

#[test]
fn each_component_rounds_up_on_its_own() {
    assert_eq!(credits_micro(1, 0, 1, 1), Ok(1));
    assert_eq!(credits_micro(0, 1, 1, 1), Ok(1));
    // 999_999 / 1_000_000 -> 1 and 3_500_007 / 1_000_000 -> 4.
    assert_eq!(credits_micro(3, 7, 333_333, 500_001), Ok(5));
}

#[test]
fn the_ceiling_is_applied_per_component_never_to_the_sum() {
    // Two halves of a micro-credit: ceil(0.5) + ceil(0.5) = 2, ceil(1.0) = 1.
    assert_eq!(credits_micro(500_000, 500_000, 1, 1), Ok(2));
}

#[test]
fn an_exact_multiple_adds_nothing() {
    assert_eq!(credits_micro(1_000_000, 0, 1, 1), Ok(1));
    assert_eq!(credits_micro(1_000_000, 1_000_000, 1, 2), Ok(3));
}

#[test]
fn zero_tokens_cost_nothing() {
    assert_eq!(credits_micro(0, 0, 1_000_000_000, 1_000_000_000), Ok(0));
}

#[test]
fn matches_the_exact_oracle_over_a_grid_of_values() {
    let tokens = [0_i64, 1, 2, 999, 1_000, 123_456, 9_999_999, 10_000_000];
    let mults = [
        1_u64,
        2,
        333_333,
        999_999,
        1_000_000,
        1_000_001,
        2_500_000_000,
        9_999_999_999,
        10_000_000_000,
    ];
    for input in tokens {
        for output in tokens {
            for in_mult in mults {
                for out_mult in mults {
                    let expected = oracle(input, output, in_mult, out_mult);
                    assert_eq!(
                        credits_micro(input, output, in_mult, out_mult),
                        Ok(i64::try_from(expected).unwrap()),
                        "({input}, {output}, {in_mult}, {out_mult})"
                    );
                }
            }
        }
    }
}

// ---- the worked numbers of DESIGN 5.10 -----------------------------------------------------------

#[test]
fn design_5_10_premium_reserve_is_3_750_000() {
    // 1000 estimated input + 500 max output at 2.5 credits per 1K tokens.
    assert_eq!(
        credits_micro(1_000, 500, 2_500_000_000, 2_500_000_000),
        Ok(3_750_000)
    );
}

#[test]
fn design_5_10_standard_reserve_is_1_500_000() {
    assert_eq!(
        credits_micro(1_000, 500, 1_000_000_000, 1_000_000_000),
        Ok(1_500_000)
    );
}

#[test]
fn design_5_10_actual_900_input_300_output_is_1_200_000() {
    assert_eq!(
        credits_micro(900, 300, 1_000_000_000, 1_000_000_000),
        Ok(1_200_000)
    );
}

#[test]
fn the_estimated_formula_example_is_1_050_000() {
    // reserve_tokens 1500, max_output_tokens_applied 500, floor 50: the credits
    // of 1000 input and 50 output tokens at 1_000_000_000 / 1_000_000_000.
    assert_eq!(
        credits_micro(1_000, 50, 1_000_000_000, 1_000_000_000),
        Ok(1_050_000)
    );
}

// ---- token count errors ---------------------------------------------------------------------------

#[test]
fn the_largest_token_count_is_accepted() {
    assert_eq!(
        credits_micro(10_000_000, 0, 1_000_000, 1_000_000),
        Ok(10_000_000)
    );
    assert_eq!(
        credits_micro(0, 10_000_000, 1_000_000, 1_000_000),
        Ok(10_000_000)
    );
}

#[test]
fn an_input_token_count_above_the_limit_is_an_error_naming_it() {
    let error = credits_micro(10_000_001, 0, 1, 1).unwrap_err();

    assert_eq!(
        error,
        CreditError::InvalidTokenCount {
            field: "input_tokens",
            value: 10_000_001
        }
    );
    assert!(error.to_string().contains("10000001"), "{error}");
}

#[test]
fn an_output_token_count_above_the_limit_is_an_error_naming_it() {
    assert_eq!(
        credits_micro(0, 10_000_001, 1, 1),
        Err(CreditError::InvalidTokenCount {
            field: "output_tokens",
            value: 10_000_001
        })
    );
}

#[test]
fn a_negative_token_count_is_an_error() {
    assert_eq!(
        credits_micro(-1, 0, 1, 1),
        Err(CreditError::InvalidTokenCount {
            field: "input_tokens",
            value: -1
        })
    );
    assert_eq!(
        credits_micro(0, i64::MIN, 1, 1),
        Err(CreditError::InvalidTokenCount {
            field: "output_tokens",
            value: i64::MIN
        })
    );
}

#[test]
fn the_input_count_is_checked_before_the_output_count() {
    assert_eq!(
        credits_micro(-5, -6, 1, 1),
        Err(CreditError::InvalidTokenCount {
            field: "input_tokens",
            value: -5
        })
    );
}

// ---- multiplier errors ----------------------------------------------------------------------------

#[test]
fn a_zero_multiplier_is_an_error_because_it_would_make_the_usage_free() {
    assert_eq!(
        credits_micro(1, 1, 0, 1),
        Err(CreditError::ZeroMultiplier { field: "in_mult" })
    );
    assert_eq!(
        credits_micro(1, 1, 1, 0),
        Err(CreditError::ZeroMultiplier { field: "out_mult" })
    );
    assert_eq!(
        credits_micro(0, 0, 0, 0),
        Err(CreditError::ZeroMultiplier { field: "in_mult" }),
        "zero tokens do not excuse a zero multiplier"
    );
}

#[test]
fn a_zero_multiplier_is_reported_before_a_range_error_of_the_other_one() {
    assert_eq!(
        credits_micro(1, 1, 0, u64::MAX),
        Err(CreditError::ZeroMultiplier { field: "in_mult" })
    );
    assert_eq!(
        credits_micro(1, 1, u64::MAX, 0),
        Err(CreditError::ZeroMultiplier { field: "out_mult" })
    );
}

#[test]
fn the_largest_multiplier_is_accepted_and_one_more_is_not() {
    assert!(credits_micro(1, 1, 10_000_000_000, 10_000_000_000).is_ok());

    assert_eq!(
        credits_micro(1, 1, 10_000_000_001, 1),
        Err(CreditError::InvalidMultiplier {
            field: "in_mult",
            value: 10_000_000_001
        })
    );
    assert_eq!(
        credits_micro(1, 1, 1, 10_000_000_001),
        Err(CreditError::InvalidMultiplier {
            field: "out_mult",
            value: 10_000_000_001
        })
    );
}

#[test]
fn a_multiplier_that_does_not_fit_a_signed_64_bit_integer_is_an_error() {
    let too_big = u64::try_from(i64::MAX).unwrap() + 1;

    assert_eq!(
        credits_micro(1, 1, too_big, 1),
        Err(CreditError::InvalidMultiplier {
            field: "in_mult",
            value: too_big
        })
    );
    assert_eq!(
        credits_micro(1, 1, 1, u64::MAX),
        Err(CreditError::InvalidMultiplier {
            field: "out_mult",
            value: u64::MAX
        })
    );
}

#[test]
fn a_token_count_error_wins_over_a_multiplier_error() {
    assert_eq!(
        credits_micro(10_000_001, 1, 0, 1),
        Err(CreditError::InvalidTokenCount {
            field: "input_tokens",
            value: 10_000_001
        })
    );
}

#[test]
fn the_largest_valid_inputs_do_not_overflow() {
    // 10_000_000 tokens at 10_000_000_000 per million: 10^11 per component.
    assert_eq!(
        credits_micro(10_000_000, 10_000_000, 10_000_000_000, 10_000_000_000),
        Ok(200_000_000_000)
    );
}
