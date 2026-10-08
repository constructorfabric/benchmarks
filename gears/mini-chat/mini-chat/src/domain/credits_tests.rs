use super::*;

#[test]
fn design_example_premium_reserve() {
    // DESIGN §5.10.2: 1000 input + 500 output at 2.5 credits / 1K tokens.
    let r = credits_micro(1_000, 500, 2_500_000_000, 2_500_000_000).expect("ok");
    assert_eq!(r, 3_750_000);
    let s = credits_micro(1_000, 500, 1_000_000_000, 1_000_000_000).expect("ok");
    assert_eq!(s, 1_500_000);
}

#[test]
fn ceiling_is_per_component() {
    // 1 token at 1 micro-credit per 1M tokens rounds up to 1 per component.
    assert_eq!(credits_micro(1, 1, 1, 1).expect("ok"), 2);
    assert_eq!(credits_micro(0, 0, 1, 1).expect("ok"), 0);
    // 999_999 * 1 / 1e6 -> ceil = 1
    assert_eq!(credits_micro(999_999, 0, 1, 1).expect("ok"), 1);
    assert_eq!(credits_micro(1_000_000, 0, 1, 1).expect("ok"), 1);
    assert_eq!(credits_micro(1_000_001, 0, 1, 1).expect("ok"), 2);
}

#[test]
fn bounds_are_checked() {
    assert_eq!(
        credits_micro(-1, 0, 1, 1),
        Err(CreditsError::InvalidTokenCount(-1))
    );
    assert_eq!(
        credits_micro(MAX_TOKENS + 1, 0, 1, 1),
        Err(CreditsError::InvalidTokenCount(MAX_TOKENS + 1))
    );
    assert_eq!(credits_micro(1, 1, 0, 1), Err(CreditsError::ZeroMultiplier));
    assert_eq!(
        credits_micro(1, 1, 1, MAX_MULTIPLIER + 1),
        Err(CreditsError::MultiplierOutOfRange(MAX_MULTIPLIER + 1))
    );
    assert!(credits_micro(MAX_TOKENS, MAX_TOKENS, MAX_MULTIPLIER, MAX_MULTIPLIER).is_ok());
}
