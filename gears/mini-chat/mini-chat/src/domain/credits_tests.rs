//! Tests for the canonical credit formula.

use super::*;

#[test]
fn per_component_ceil_div() {
    // 1 token at 2.5 credits/1K tokens = 2_500_000_000 micro per 1M.
    assert_eq!(credits_micro_checked(1, 0, 2_500_000_000, 1).unwrap(), 2_500);
    // Per-component rounding: 1 in + 1 out rounds each component up.
    assert_eq!(credits_micro_checked(1, 1, 1, 1).unwrap(), 2);
    assert_eq!(credits_micro_checked(0, 0, 1, 1).unwrap(), 0);
}

#[test]
fn rejects_zero_multipliers() {
    assert_eq!(
        credits_micro_checked(1, 1, 0, 1),
        Err(CreditError::ZeroMultiplier)
    );
    assert_eq!(
        credits_micro_checked(1, 1, 1, 0),
        Err(CreditError::ZeroMultiplier)
    );
}

#[test]
fn rejects_out_of_range_tokens() {
    assert_eq!(
        credits_micro_checked(MAX_TOKENS + 1, 0, 1, 1),
        Err(CreditError::InvalidTokenCount)
    );
    assert_eq!(
        credits_micro_checked(-1, 0, 1, 1),
        Err(CreditError::InvalidTokenCount)
    );
}

#[test]
fn rejects_out_of_range_multipliers() {
    assert_eq!(
        credits_micro_checked(1, 0, MAX_MULT + 1, 1),
        Err(CreditError::InvalidMultiplier)
    );
}

#[test]
fn or_max_substitutes_on_error() {
    assert_eq!(credits_micro_or_max(1, 1, 0, 0), i64::MAX);
    assert_eq!(credits_micro_or_max(1000, 500, 1_000_000, 1_000_000), 1500);
}

#[test]
fn example_from_design() {
    // DESIGN 5.10.2: premium reserve 3_750_000 for 1000 in + 500 out.
    assert_eq!(
        credits_micro_checked(1_000, 500, 2_500_000_000, 2_500_000_000).unwrap(),
        3_750_000
    );
}
