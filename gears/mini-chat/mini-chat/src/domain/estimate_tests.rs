use mini_chat_sdk::EstimationBudgets;
use chrono::{TimeZone, Utc};

use super::*;

#[test]
fn credits_round_per_component() {
    assert_eq!(credits_micro(1, 1, 1_000_000, 1_000_000), Ok(2));
    assert_eq!(credits_micro(1, 1, 1, 1), Ok(2));
    assert_eq!(credits_micro(100, 50, 1_000_000, 3_000_000), Ok(250));
    assert_eq!(credits_micro(0, 0, 5, 5), Ok(0));
    assert_eq!(credits_micro(1000, 0, 2_500_000_000, 1), Ok(2_500_000));
}

#[test]
fn credits_reject_invalid_inputs() {
    assert_eq!(credits_micro(10_000_001, 0, 1, 1), Err(CreditError::InvalidTokenCount(10_000_001)));
    assert_eq!(credits_micro(-1, 0, 1, 1), Err(CreditError::InvalidTokenCount(-1)));
    assert_eq!(credits_micro(1, 1, 0, 1), Err(CreditError::ZeroMultiplier));
    assert_eq!(
        credits_micro(1, 1, 10_000_000_001, 1),
        Err(CreditError::InvalidMultiplier(10_000_000_001))
    );
}

#[test]
fn text_estimate_formula() {
    let b = EstimationBudgets::default(); // 4 bytes/token, 100 overhead, 10%
    assert_eq!(estimate_text_tokens(0, &b), 110);
    assert_eq!(estimate_text_tokens(400, &b), 220);
    assert_eq!(estimate_text_tokens(1, &b), 112); // ceil((1+100)*1.1)
}

#[test]
fn periods_are_utc_calendar_based() {
    let t = Utc.with_ymd_and_hms(2026, 2, 28, 23, 59, 59).unwrap();
    assert_eq!(PeriodType::Daily.start_of(t).to_string(), "2026-02-28");
    assert_eq!(PeriodType::Monthly.start_of(t).to_string(), "2026-02-01");
    assert_eq!(PeriodType::Daily.next_reset(t), Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap());
    assert_eq!(
        PeriodType::Monthly.next_reset(Utc.with_ymd_and_hms(2026, 12, 15, 10, 0, 0).unwrap()),
        Utc.with_ymd_and_hms(2027, 1, 1, 0, 0, 0).unwrap()
    );
}

#[test]
fn warning_and_exhausted_flags() {
    assert_eq!(remaining_percentage(100, 0), 100);
    assert_eq!(remaining_percentage(100, 81), 19);
    assert_eq!(remaining_percentage(1000, 999), 0);
    assert_eq!(remaining_percentage(100, 150), 0);
    assert_eq!(warning_flags(20, 80), (true, false));
    assert_eq!(warning_flags(21, 80), (false, false));
    assert_eq!(warning_flags(0, 80), (true, true));
}
