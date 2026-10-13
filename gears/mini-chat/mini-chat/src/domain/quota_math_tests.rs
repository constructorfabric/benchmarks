#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use time::macros::datetime;

#[test]
fn credits_per_component_ceil() {
    // 1 token at 1 micro-credit per 1M tokens -> ceil(1/1e6) = 1 per component
    assert_eq!(credits_micro_checked(1, 1, 1, 1).unwrap(), 2);
    assert_eq!(
        credits_micro_checked(1000, 500, 2_500_000_000, 2_500_000_000).unwrap(),
        2_500_000 + 1_250_000
    );
    assert_eq!(credits_micro_checked(0, 0, 5, 5).unwrap(), 0);
    // DESIGN 5.10 style: 11000 in / 500 out at 1e9
    assert_eq!(
        credits_micro_checked(11000, 500, 1_000_000_000, 1_000_000_000).unwrap(),
        11_500_000
    );
}

#[test]
fn credits_bounds() {
    assert_eq!(
        credits_micro_checked(1, 1, 0, 1),
        Err(CreditError::ZeroMultiplier)
    );
    assert_eq!(
        credits_micro_checked(1, 1, MAX_MULT + 1, 1),
        Err(CreditError::MultiplierTooLarge)
    );
    assert_eq!(
        credits_micro_checked(MAX_TOKENS + 1, 1, 1, 1),
        Err(CreditError::InvalidTokenCount)
    );
    assert_eq!(
        credits_micro_checked(-1, 1, 1, 1),
        Err(CreditError::InvalidTokenCount)
    );
    assert!(credits_micro_checked(MAX_TOKENS, MAX_TOKENS, MAX_MULT, MAX_MULT).is_ok());
}

#[test]
fn text_estimate_formula() {
    let b = EstimationBudgets::default(); // bpt 4, fixed 100, margin 10
    assert_eq!(estimate_text_tokens(0, &b), 110);
    assert_eq!(estimate_text_tokens(400, &b), 220);
    assert_eq!(estimate_text_tokens(401, &b), 222); // ceil(401/4)=101 -> 201*1.1=221.1 -> 222
    let mut z = b;
    z.bytes_per_token_conservative = 0; // clamped to 1
    assert_eq!(estimate_text_tokens(10, &z), 121);
}

#[test]
fn periods_and_resets() {
    let now = datetime!(2026-02-28 15:30:00 UTC);
    assert_eq!(Period::Daily.start(now).to_string(), "2026-02-28");
    assert_eq!(Period::Monthly.start(now).to_string(), "2026-02-01");
    assert_eq!(
        Period::Daily.next_reset(now),
        datetime!(2026-03-01 0:00 UTC)
    );
    assert_eq!(
        Period::Monthly.next_reset(now),
        datetime!(2026-03-01 0:00 UTC)
    );
    let dec = datetime!(2026-12-31 23:59:59 UTC);
    assert_eq!(
        Period::Monthly.next_reset(dec),
        datetime!(2027-01-01 0:00 UTC)
    );
}

#[test]
fn remaining_and_flags() {
    assert_eq!(remaining_percentage(100, 0), 100);
    assert_eq!(remaining_percentage(100, 81), 19);
    assert_eq!(remaining_percentage(1000, 995), 0); // floored: <1% -> 0
    assert_eq!(remaining_percentage(100, 150), 0);
    assert_eq!(warning_flags(19, 80), (true, false));
    assert_eq!(warning_flags(20, 80), (true, false));
    assert_eq!(warning_flags(21, 80), (false, false));
    assert_eq!(warning_flags(0, 80), (true, true));
}
