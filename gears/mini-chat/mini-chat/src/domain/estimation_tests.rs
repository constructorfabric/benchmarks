#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use chrono::{DateTime, NaiveDate, Utc};

fn datetime(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

fn date(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

fn budgets(bpt: u32, overhead: u32, margin: u32) -> EstimationBudgets {
    EstimationBudgets {
        bytes_per_token_conservative: bpt,
        fixed_overhead_tokens: overhead,
        safety_margin_pct: margin,
        ..EstimationBudgets::default()
    }
}

#[test]
fn text_tokens_formula() {
    let text = "a".repeat(100);
    // ceil((25 + 100) * 110 / 100) = ceil(137.5) = 138
    assert_eq!(estimated_text_tokens(&text, &budgets(4, 100, 10)), 138);
    assert_eq!(item_tokens(&text, &budgets(4, 100, 10)), 138);
}

#[test]
fn empty_text_is_overhead_with_margin() {
    assert_eq!(estimated_text_tokens("", &budgets(4, 100, 10)), 110);
}

#[test]
fn text_tokens_count_utf8_bytes() {
    let text = "é".repeat(10); // 20 bytes
    assert_eq!(estimated_text_tokens(&text, &budgets(4, 0, 0)), 5);
    assert_eq!(estimated_text_tokens(&text, &budgets(1, 0, 0)), 20);
}

#[test]
fn zero_bpt_clamped_to_one() {
    assert_eq!(estimated_text_tokens("abcd", &budgets(0, 0, 0)), 4);
}

#[test]
fn period_starts_utc() {
    let (daily, monthly) = period_starts(datetime("2026-02-28T23:59:59Z"));
    assert_eq!(daily, date(2026, 2, 28));
    assert_eq!(monthly, date(2026, 2, 1));
}

#[test]
fn period_starts_converts_offset_to_utc() {
    let (daily, monthly) = period_starts(datetime("2026-03-01T00:30:00+02:00"));
    assert_eq!(daily, date(2026, 2, 28));
    assert_eq!(monthly, date(2026, 2, 1));
}

#[test]
fn next_reset_daily_and_monthly() {
    let now = datetime("2026-02-28T23:59:59Z");
    assert_eq!(
        next_reset(PeriodType::Daily, now),
        datetime("2026-03-01T00:00:00Z")
    );
    assert_eq!(
        next_reset(PeriodType::Monthly, now),
        datetime("2026-03-01T00:00:00Z")
    );
    let dec = datetime("2026-12-15T10:00:00Z");
    assert_eq!(
        next_reset(PeriodType::Monthly, dec),
        datetime("2027-01-01T00:00:00Z")
    );
    assert_eq!(
        next_reset(PeriodType::Daily, dec),
        datetime("2026-12-16T00:00:00Z")
    );
}
