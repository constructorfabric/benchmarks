#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::{CreditsError, MAX_MULTIPLIER, MAX_TOKENS, credits_micro};

#[test]
fn each_component_is_rounded_up_separately() {
    // ceil(7 * 1.5) + ceil(3 * 2.5) = 11 + 8, not ceil(18)
    assert_eq!(credits_micro(7, 3, 1_500_000, 2_500_000).unwrap(), 19);
}

#[test]
fn the_reserve_of_a_five_byte_message_matches_the_design_derivation() {
    assert_eq!(
        credits_micro(113, 4096, 1_000_000, 3_000_000).unwrap(),
        12_401
    );
}

#[test]
fn zero_tokens_cost_nothing() {
    assert_eq!(credits_micro(0, 0, 1, 1).unwrap(), 0);
}

#[test]
fn a_zero_multiplier_is_refused() {
    assert_eq!(
        credits_micro(1, 1, 0, 1),
        Err(CreditsError::InvalidMultiplier(0))
    );
    assert_eq!(
        credits_micro(1, 1, 1, 0),
        Err(CreditsError::InvalidMultiplier(0))
    );
}

#[test]
fn out_of_range_inputs_are_refused() {
    assert_eq!(
        credits_micro(-1, 0, 1, 1),
        Err(CreditsError::InvalidTokenCount(-1))
    );
    assert_eq!(
        credits_micro(MAX_TOKENS + 1, 0, 1, 1),
        Err(CreditsError::InvalidTokenCount(MAX_TOKENS + 1))
    );
    assert_eq!(
        credits_micro(1, 1, MAX_MULTIPLIER + 1, 1),
        Err(CreditsError::InvalidMultiplier(MAX_MULTIPLIER + 1))
    );
}

#[test]
fn the_largest_valid_inputs_do_not_overflow() {
    assert!(credits_micro(MAX_TOKENS, MAX_TOKENS, MAX_MULTIPLIER, MAX_MULTIPLIER).is_ok());
}
