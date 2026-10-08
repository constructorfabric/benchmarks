use super::*;

#[test]
fn per_component_ceil() {
    // ceil(1 * 1_500_000 / 1_000_000) = 2, ceil(1 * 1 / 1_000_000) = 1.
    // Rounding the sum instead would give ceil(1_500_001 / 1_000_000) = 2.
    assert_eq!(credits_micro_checked(1, 1, 1_500_000, 1), Ok(2 + 1));
}

#[test]
fn zero_tokens_cost_nothing() {
    assert_eq!(credits_micro_checked(0, 0, 1, 1), Ok(0));
}

#[test]
fn rejects_zero_multiplier() {
    assert_eq!(
        credits_micro_checked(10, 10, 0, 1),
        Err(CreditError::ZeroMultiplier)
    );
    assert_eq!(
        credits_micro_checked(10, 10, 1, 0),
        Err(CreditError::ZeroMultiplier)
    );
}

#[test]
fn rejects_negative_multiplier_as_zero_or_below() {
    assert_eq!(
        credits_micro_checked(10, 10, -1, 1),
        Err(CreditError::ZeroMultiplier)
    );
}

#[test]
fn rejects_multiplier_over_limit() {
    assert_eq!(
        credits_micro_checked(1, 1, 10_000_000_001, 1),
        Err(CreditError::MultiplierTooLarge)
    );
    assert_eq!(
        credits_micro_checked(1, 1, 1, 10_000_000_001),
        Err(CreditError::MultiplierTooLarge)
    );
}

#[test]
fn accepts_multiplier_at_limit() {
    assert!(credits_micro_checked(1, 1, 10_000_000_000, 10_000_000_000).is_ok());
}

#[test]
fn rejects_tokens_over_10m() {
    assert_eq!(
        credits_micro_checked(10_000_001, 0, 1, 1),
        Err(CreditError::InvalidTokenCount)
    );
    assert_eq!(
        credits_micro_checked(0, 10_000_001, 1, 1),
        Err(CreditError::InvalidTokenCount)
    );
}

#[test]
fn rejects_negative_tokens() {
    assert_eq!(
        credits_micro_checked(-1, 0, 1, 1),
        Err(CreditError::InvalidTokenCount)
    );
    assert_eq!(
        credits_micro_checked(0, -1, 1, 1),
        Err(CreditError::InvalidTokenCount)
    );
}

#[test]
fn maximum_inputs_do_not_overflow() {
    // 10_000_000 * 10_000_000_000 = 1e17 per component (1e11 credits), well inside i64.
    assert_eq!(
        credits_micro_checked(10_000_000, 10_000_000, 10_000_000_000, 10_000_000_000),
        Ok(2 * 100_000_000_000)
    );
}

#[test]
fn section_5_10_example() {
    // DESIGN 5.10.2: premium reserve for 1_000 input + 500 output tokens.
    assert_eq!(
        credits_micro_checked(1_000, 500, 2_500_000_000, 2_500_000_000),
        Ok(3_750_000)
    );
    // DESIGN 5.10.3: standard reserve.
    assert_eq!(
        credits_micro_checked(1_000, 500, 1_000_000_000, 1_000_000_000),
        Ok(1_500_000)
    );
    // DESIGN 5.10.4: settlement by actual usage 900 + 300.
    assert_eq!(
        credits_micro_checked(900, 300, 1_000_000_000, 1_000_000_000),
        Ok(1_200_000)
    );
}
