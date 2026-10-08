#![allow(clippy::unwrap_used)]

use super::{CreditsError, ceil_div, credits_micro_checked};

#[test]
fn per_component_ceiling() {
    // 1 token * 1 micro per 1M tokens -> ceil(1/1e6) = 1 per component
    assert_eq!(credits_micro_checked(1, 1, 1, 1).unwrap(), 2);
    assert_eq!(credits_micro_checked(0, 0, 5, 5).unwrap(), 0);
    // DESIGN 5.10 example: 1000 in + 500 out at 2.5e9 -> 3_750_000
    assert_eq!(
        credits_micro_checked(1_000, 500, 2_500_000_000, 2_500_000_000).unwrap(),
        3_750_000
    );
    // standard: 900 + 300 at 1e9 -> 1_200_000
    assert_eq!(
        credits_micro_checked(900, 300, 1_000_000_000, 1_000_000_000).unwrap(),
        1_200_000
    );
    // ceil per component differs from ceil of sum
    assert_eq!(credits_micro_checked(1, 1, 500_000, 500_000).unwrap(), 2);
}

#[test]
fn ceil_div_rounds_up() {
    assert_eq!(ceil_div(1, 1_000_000), 1);
    assert_eq!(ceil_div(1_000_000, 1_000_000), 1);
    assert_eq!(ceil_div(1_000_001, 1_000_000), 2);
    assert_eq!(ceil_div(0, 1_000_000), 0);
}

#[test]
fn bounds_are_validated() {
    assert_eq!(credits_micro_checked(10_000_001, 0, 1, 1), Err(CreditsError::InvalidTokenCount));
    assert_eq!(credits_micro_checked(-1, 0, 1, 1), Err(CreditsError::InvalidTokenCount));
    assert_eq!(credits_micro_checked(1, 1, 0, 1), Err(CreditsError::ZeroMultiplier));
    assert_eq!(
        credits_micro_checked(1, 1, 10_000_000_001, 1),
        Err(CreditsError::MultiplierTooLarge)
    );
    assert!(credits_micro_checked(10_000_000, 10_000_000, 10_000_000_000, 10_000_000_000).is_ok());
}
