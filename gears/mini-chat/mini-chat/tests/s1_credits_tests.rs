#![allow(clippy::unwrap_used, clippy::expect_used)]
// `CreditsError` variants are matched with `{ .. }` so the assertions hold whether a
// variant is a unit variant or carries the offending value (DESIGN 5.3 "Error handling").
#![allow(clippy::unneeded_struct_pattern)]

//! S1 credit arithmetic tests (DESIGN section 5.3 "Credit Arithmetic" and
//! "Overflow Protection", section 5.10 "Calculation Example").
//!
//! Canonical formula: `ceil_div(input_tokens * in_mult, 1_000_000) +
//! ceil_div(output_tokens * out_mult, 1_000_000)`, rounded per component.

use mini_chat::domain::quota::{CreditsError, credits_micro_checked};

const MAX_TOKENS: i64 = 10_000_000;
const MAX_MULTIPLIER: i64 = 10_000_000_000;
/// One credit per 1K tokens.
const ONE_CREDIT_PER_1K: i64 = 1_000_000_000;

fn credits(input_tokens: i64, output_tokens: i64, in_mult: i64, out_mult: i64) -> i64 {
    credits_micro_checked(input_tokens, output_tokens, in_mult, out_mult)
        .expect("arguments are within bounds")
}

fn credits_err(input_tokens: i64, output_tokens: i64, in_mult: i64, out_mult: i64) -> CreditsError {
    credits_micro_checked(input_tokens, output_tokens, in_mult, out_mult)
        .expect_err("arguments are out of bounds")
}

// ── Canonical values ─────────────────────────────────────────────────────

#[test]
fn credits_single_token_each_rounds_up_per_component() {
    assert_eq!(credits(1, 1, 1, 1), 2);
}

#[test]
fn credits_rounding_is_per_component_not_on_the_sum() {
    // Each component is 0.5 micro-credit and rounds up to 1; rounding the sum
    // (1.0) would give 1.
    assert_eq!(credits(1, 1, 500_000, 500_000), 2);
}

#[test]
fn credits_one_credit_per_1k_tokens() {
    assert_eq!(
        credits(1000, 500, ONE_CREDIT_PER_1K, ONE_CREDIT_PER_1K),
        1_500_000
    );
}

#[test]
fn credits_zero_tokens_cost_nothing() {
    assert_eq!(credits(0, 0, ONE_CREDIT_PER_1K, 2_500_000_000), 0);
}

#[test]
fn credits_design_example_premium_reserve() {
    // DESIGN 5.10.2: 1_000 input + 500 output at 2.5 credits per 1K tokens.
    assert_eq!(credits(1000, 500, 2_500_000_000, 2_500_000_000), 3_750_000);
}

#[test]
fn credits_design_example_standard_reserve() {
    // DESIGN 5.10.3: 1_000 input + 500 output at 1 credit per 1K tokens.
    assert_eq!(
        credits(1000, 500, ONE_CREDIT_PER_1K, ONE_CREDIT_PER_1K),
        1_500_000
    );
}

#[test]
fn credits_uses_separate_input_and_output_multipliers() {
    // gpt-4.1 catalog multipliers from config/mini-chat.yaml:
    // ceil(1_000 * 3_000_000 / 1e6) + ceil(500 * 15_000_000 / 1e6) = 3_000 + 7_500.
    assert_eq!(credits(1000, 500, 3_000_000, 15_000_000), 10_500);
}

#[test]
fn credits_exact_multiple_is_not_rounded_up() {
    assert_eq!(credits(1_000_000, 0, 1_000_000, 1), 1_000_000);
}

#[test]
fn credits_remainder_rounds_up_by_one() {
    assert_eq!(credits(1_000_001, 0, 1_000_000, 1), 1_000_001);
    assert_eq!(credits(1_000_001, 0, 1, 1), 2);
}

#[test]
fn credits_output_only_is_charged() {
    assert_eq!(credits(0, 2000, 1, ONE_CREDIT_PER_1K), 2_000_000);
}

// ── Token count bounds ───────────────────────────────────────────────────

#[test]
fn credits_accepts_max_token_count() {
    assert_eq!(credits(MAX_TOKENS, 0, 1_000_000, 1_000_000), MAX_TOKENS);
    assert_eq!(credits(0, MAX_TOKENS, 1_000_000, 1_000_000), MAX_TOKENS);
}

#[test]
fn credits_rejects_input_tokens_above_max() {
    let err = credits_err(MAX_TOKENS + 1, 0, 1_000_000, 1_000_000);

    assert!(
        matches!(err, CreditsError::InvalidTokenCount { .. }),
        "got {err:?}"
    );
}

#[test]
fn credits_rejects_output_tokens_above_max() {
    let err = credits_err(0, MAX_TOKENS + 1, 1_000_000, 1_000_000);

    assert!(
        matches!(err, CreditsError::InvalidTokenCount { .. }),
        "got {err:?}"
    );
}

#[test]
fn credits_rejects_negative_token_counts() {
    for (input_tokens, output_tokens) in [(-1, 0), (0, -1), (i64::MIN, 0), (0, i64::MIN)] {
        let err = credits_err(input_tokens, output_tokens, 1_000_000, 1_000_000);

        assert!(
            matches!(err, CreditsError::InvalidTokenCount { .. }),
            "tokens ({input_tokens}, {output_tokens}): got {err:?}"
        );
    }
}

// ── Multiplier bounds ────────────────────────────────────────────────────

#[test]
fn credits_rejects_zero_multiplier() {
    for (in_mult, out_mult) in [(0, 1_000_000), (1_000_000, 0), (0, 0)] {
        let err = credits_err(100, 100, in_mult, out_mult);

        assert!(
            matches!(err, CreditsError::ZeroMultiplier { .. }),
            "multipliers ({in_mult}, {out_mult}): got {err:?}"
        );
    }
}

#[test]
fn credits_rejects_zero_multiplier_even_with_zero_tokens() {
    let err = credits_err(0, 0, 0, 1_000_000);

    assert!(
        matches!(err, CreditsError::ZeroMultiplier { .. }),
        "got {err:?}"
    );
}

#[test]
fn credits_rejects_negative_multiplier_as_zero_multiplier() {
    for (in_mult, out_mult) in [(-5, 1_000_000), (1_000_000, -5)] {
        let err = credits_err(100, 100, in_mult, out_mult);

        assert!(
            matches!(err, CreditsError::ZeroMultiplier { .. }),
            "multipliers ({in_mult}, {out_mult}): got {err:?}"
        );
    }
}

#[test]
fn credits_accepts_max_and_min_multiplier() {
    assert_eq!(credits(1, 1, MAX_MULTIPLIER, 1), 10_001);
    assert_eq!(credits(1, 1, 1, MAX_MULTIPLIER), 10_001);
}

#[test]
fn credits_rejects_multiplier_above_max() {
    for (in_mult, out_mult) in [(MAX_MULTIPLIER + 1, 1), (1, MAX_MULTIPLIER + 1)] {
        let err = credits_err(100, 100, in_mult, out_mult);

        assert!(
            matches!(err, CreditsError::MultiplierTooLarge { .. }),
            "multipliers ({in_mult}, {out_mult}): got {err:?}"
        );
    }
}

#[test]
fn credits_rejects_max_i64_multiplier_as_too_large() {
    let err = credits_err(1, 1, i64::MAX, 1);

    assert!(
        matches!(err, CreditsError::MultiplierTooLarge { .. }),
        "got {err:?}"
    );
}

// ── Overflow protection ──────────────────────────────────────────────────

#[test]
fn credits_max_bounds_do_not_overflow() {
    // 10_000_000 * 10_000_000_000 = 1e17 per component (< i64::MAX ~ 9.2e18);
    // 1e17 / 1e6 = 1e11 per component.
    let result = credits_micro_checked(MAX_TOKENS, MAX_TOKENS, MAX_MULTIPLIER, MAX_MULTIPLIER);

    assert_eq!(result, Ok(200_000_000_000));
}
