#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

#[test]
fn per_component_ceil() {
    assert_eq!(credits_micro(1, 1, 1, 1), Ok(2));
}

#[test]
fn design_example() {
    assert_eq!(
        credits_micro(1000, 500, 2_500_000_000, 2_500_000_000),
        Ok(3_750_000)
    );
}

#[test]
fn zero_tokens_cost_nothing() {
    assert_eq!(credits_micro(0, 0, 1, 1), Ok(0));
}

#[test]
fn zero_multiplier_rejected() {
    assert_eq!(credits_micro(1, 1, 0, 1), Err(CreditsError::ZeroMultiplier));
    assert_eq!(credits_micro(1, 1, 1, 0), Err(CreditsError::ZeroMultiplier));
}

#[test]
fn multiplier_above_1e10_rejected() {
    assert_eq!(
        credits_micro(1, 1, 10_000_000_001, 1),
        Err(CreditsError::MultiplierTooLarge)
    );
    assert_eq!(
        credits_micro(1, 1, 1, 10_000_000_001),
        Err(CreditsError::MultiplierTooLarge)
    );
    assert!(credits_micro(1, 1, 10_000_000_000, 10_000_000_000).is_ok());
}

#[test]
fn tokens_above_10m_rejected() {
    assert_eq!(
        credits_micro(10_000_001, 0, 1, 1),
        Err(CreditsError::InvalidTokenCount)
    );
    assert_eq!(
        credits_micro(0, 10_000_001, 1, 1),
        Err(CreditsError::InvalidTokenCount)
    );
    assert_eq!(credits_micro(-1, 0, 1, 1), Err(CreditsError::InvalidTokenCount));
    assert!(credits_micro(10_000_000, 10_000_000, 10_000_000_000, 10_000_000_000).is_ok());
}
