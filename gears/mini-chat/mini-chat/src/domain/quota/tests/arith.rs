//! Credits, estimation, periods and billing derivation.

use mini_chat_sdk::{EstimationBudgets, UsageTokens};
use time::{Date, Month, OffsetDateTime, Time, UtcOffset};

use crate::domain::quota::{
    BillingOutcome, SettlementMethod, credits_micro, derive_billing, estimate_text_tokens, next_daily_reset,
    next_monthly_reset, period_starts,
};

fn date(y: i32, m: u8, d: u8) -> Date {
    Date::from_calendar_date(y, Month::try_from(m).unwrap(), d).unwrap()
}

fn dt(year: i32, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> OffsetDateTime {
    date(year, month, day).with_time(Time::from_hms(hour, minute, second).unwrap()).assume_utc()
}

#[test]
fn credits_spec_example_standard_turn() {
    // DESIGN spec scenario: in_mult 1_000_000, out_mult 3_000_000, 100 in / 50 out -> 250.
    assert_eq!(credits_micro(100, 50, 1_000_000, 3_000_000).unwrap(), 250);
}

#[test]
fn credits_rounding_is_per_component() {
    // 1 token * 1 micro / 1e6 -> ceil = 1 per component, so 2 in total (not ceil(2/1e6) = 1).
    assert_eq!(credits_micro(1, 1, 1, 1).unwrap(), 2);
    assert_eq!(credits_micro(0, 0, 1, 1).unwrap(), 0);
    assert_eq!(credits_micro(1_000_000, 0, 1, 1).unwrap(), 1);
    assert_eq!(credits_micro(1_000_001, 0, 1, 1).unwrap(), 2);
    // 1 credit per 1K tokens multiplier.
    assert_eq!(credits_micro(1000, 1000, 1_000_000_000, 2_000_000_000).unwrap(), 3_000_000);
}

#[test]
fn credits_design_5_10_style_numbers() {
    // Premium reserve of the test catalog: 220 in, 4096 out, 3x / 15x.
    assert_eq!(credits_micro(220, 4096, 3_000_000, 15_000_000).unwrap(), 660 + 61_440);
    // Standard reserve: 1x / 3x.
    assert_eq!(credits_micro(220, 4096, 1_000_000, 3_000_000).unwrap(), 220 + 12_288);
}

#[test]
fn credits_bounds_and_errors() {
    assert!(credits_micro(10_000_000, 10_000_000, 10_000_000_000, 10_000_000_000).is_ok());
    let e = credits_micro(10_000_001, 0, 1, 1).unwrap_err();
    assert!(e.contains("input token"), "{e}");
    assert!(credits_micro(0, 10_000_001, 1, 1).unwrap_err().contains("output token"));
    assert!(credits_micro(-1, 0, 1, 1).is_err());
    let zero = credits_micro(1, 1, 0, 1).unwrap_err();
    assert!(zero.contains("zero"), "{zero}");
    assert!(credits_micro(1, 1, 1, 0).unwrap_err().contains("zero"));
    let big = credits_micro(1, 1, 10_000_000_001, 1).unwrap_err();
    assert!(big.contains("invalid input credit multiplier"), "{big}");
    assert!(credits_micro(1, 1, 1, -5).is_err());
}

#[test]
fn estimation_formula() {
    let b = EstimationBudgets::default(); // bpt 4, overhead 100, margin 10
    assert_eq!(estimate_text_tokens(400, &b), 220);
    // ceil(401/4) = 101; (101 + 100) * 110 / 100 = 221.1 -> 222
    assert_eq!(estimate_text_tokens(401, &b), 222);
    // empty message: overhead with margin
    assert_eq!(estimate_text_tokens(0, &b), 110);
    let custom = EstimationBudgets { bytes_per_token_conservative: 3, fixed_overhead_tokens: 0, safety_margin_pct: 20, ..b };
    // ceil(10/3) = 4; 4 * 120 / 100 = 4.8 -> 5
    assert_eq!(estimate_text_tokens(10, &custom), 5);
    // bytes_per_token 0 is treated as 1
    let zero = EstimationBudgets { bytes_per_token_conservative: 0, fixed_overhead_tokens: 0, safety_margin_pct: 0, ..b };
    assert_eq!(estimate_text_tokens(7, &zero), 7);
}

#[test]
fn period_starts_daily_and_monthly() {
    let p = period_starts(dt(2026, 2, 28, 15, 30, 0));
    assert_eq!(p.daily, date(2026, 2, 28));
    assert_eq!(p.monthly, date(2026, 2, 1));
    let p = period_starts(dt(2026, 2, 28, 23, 59, 59));
    assert_eq!(p.daily, date(2026, 2, 28));
    let p = period_starts(dt(2026, 3, 1, 0, 0, 0));
    assert_eq!(p.daily, date(2026, 3, 1));
    assert_eq!(p.monthly, date(2026, 3, 1));
    // non-UTC offsets are converted to UTC first
    let p = period_starts(dt(2026, 3, 1, 1, 0, 0).replace_offset(UtcOffset::from_hms(2, 0, 0).unwrap()));
    assert_eq!(p.daily, date(2026, 2, 28));
    assert_eq!(p.monthly, date(2026, 2, 1));
}

#[test]
fn next_resets_cross_month_and_year() {
    assert_eq!(next_daily_reset(dt(2026, 10, 3, 12, 0, 0)), dt(2026, 10, 4, 0, 0, 0));
    assert_eq!(next_daily_reset(dt(2026, 2, 28, 23, 59, 59)), dt(2026, 3, 1, 0, 0, 0));
    assert_eq!(next_daily_reset(dt(2026, 12, 31, 8, 0, 0)), dt(2027, 1, 1, 0, 0, 0));
    assert_eq!(next_daily_reset(dt(2026, 10, 3, 0, 0, 0)), dt(2026, 10, 4, 0, 0, 0));
    assert_eq!(next_monthly_reset(dt(2026, 10, 3, 12, 0, 0)), dt(2026, 11, 1, 0, 0, 0));
    assert_eq!(next_monthly_reset(dt(2026, 12, 31, 23, 59, 59)), dt(2027, 1, 1, 0, 0, 0));
    assert_eq!(next_monthly_reset(dt(2026, 1, 1, 0, 0, 0)), dt(2026, 2, 1, 0, 0, 0));
}

fn usage(i: i64, o: i64) -> UsageTokens {
    UsageTokens { input_tokens: i, output_tokens: o, ..UsageTokens::default() }
}

#[test]
fn billing_derivation_table() {
    use BillingOutcome::{Aborted, Completed, Failed};
    use SettlementMethod::{Actual, Estimated, Released};

    assert_eq!(derive_billing("completed", None, Some(&usage(10, 5))), (Completed, Actual));
    assert_eq!(derive_billing("completed", None, None), (Completed, Actual));
    assert_eq!(derive_billing("cancelled", None, Some(&usage(10, 5))), (Aborted, Estimated));
    assert_eq!(derive_billing("failed", Some("orphan_timeout"), None), (Aborted, Estimated));
    for code in [
        "provider_error",
        "provider_timeout",
        "rate_limited",
        "web_search_calls_exceeded",
        "code_interpreter_calls_exceeded",
        "agentic_iterations_exceeded",
        "unexpected_tool_use",
        "message_persistence_failed",
    ] {
        assert_eq!(derive_billing("failed", Some(code), Some(&usage(10, 0))), (Failed, Actual), "{code}");
        assert_eq!(derive_billing("failed", Some(code), Some(&usage(0, 3))), (Failed, Actual), "{code}");
        assert_eq!(derive_billing("failed", Some(code), Some(&usage(0, 0))), (Failed, Estimated), "{code}");
        assert_eq!(derive_billing("failed", Some(code), None), (Failed, Estimated), "{code}");
    }
    for code in ["context_length_exceeded", "validation_error", "input_too_long", "turn_setup_failed"] {
        assert_eq!(derive_billing("failed", Some(code), Some(&usage(10, 10))), (Failed, Released), "{code}");
        assert_eq!(derive_billing("failed", Some(code), None), (Failed, Released), "{code}");
    }
    assert_eq!(derive_billing("failed", Some("something_new"), Some(&usage(10, 10))), (Failed, Estimated));
    assert_eq!(derive_billing("failed", None, None), (Failed, Estimated));
}
