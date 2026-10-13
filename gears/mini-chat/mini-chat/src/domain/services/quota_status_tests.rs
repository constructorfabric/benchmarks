#![allow(clippy::unwrap_used, clippy::expect_used)]

use chrono::{NaiveDate, TimeZone, Utc};
use mini_chat_sdk::{TierLimits, UserLimits};
use uuid::Uuid;

use super::{QuotaTier, compute_tier_status};
use crate::domain::model::PeriodType;
use crate::infra::db::entities::quota_usage;

fn limits(premium: (i64, i64), standard: (i64, i64)) -> UserLimits {
    let tier = |(d, m)| TierLimits {
        limit_daily_credits_micro: d,
        limit_monthly_credits_micro: m,
    };
    UserLimits {
        user_id: Uuid::nil(),
        policy_version: 1,
        standard: tier(standard),
        premium: tier(premium),
    }
}

fn row(
    bucket: &str,
    period: PeriodType,
    start: NaiveDate,
    spent: i64,
    reserved: i64,
) -> quota_usage::Model {
    quota_usage::Model {
        id: Uuid::new_v4(),
        tenant_id: Uuid::nil(),
        user_id: Uuid::nil(),
        period_type: period.as_str().to_owned(),
        period_start: start,
        bucket: bucket.to_owned(),
        spent_credits_micro: spent,
        reserved_credits_micro: reserved,
        calls: 0,
        input_tokens: 0,
        output_tokens: 0,
        file_search_calls: 0,
        web_search_calls: 0,
        code_interpreter_calls: 0,
        rag_retrieval_calls: 0,
        image_inputs: 0,
        image_upload_bytes: 0,
        updated_at: None,
    }
}

fn now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 12, 31, 23, 59, 59).unwrap()
}

fn day() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 12, 31).unwrap()
}

fn month() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 12, 1).unwrap()
}

#[test]
fn tiers_and_periods_are_ordered_and_skip_non_positive_limits() {
    let out = compute_tier_status(&limits((10, 0), (5, -5)), &[], now(), 80);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].tier, QuotaTier::Premium);
    assert_eq!(out[1].tier, QuotaTier::Total);
    assert_eq!(out[0].periods.len(), 1);
    assert_eq!(out[0].periods[0].period, PeriodType::Daily);
    assert_eq!(out[1].periods.len(), 1);

    let out = compute_tier_status(&limits((10, 100), (10, 100)), &[], now(), 80);
    let periods: Vec<_> = out[0].periods.iter().map(|p| p.period).collect();
    assert_eq!(periods, [PeriodType::Daily, PeriodType::Monthly]);
}

#[test]
fn tier_whose_limits_are_all_non_positive_is_omitted() {
    let out = compute_tier_status(&limits((10, 0), (0, -5)), &[], now(), 80);
    let tiers: Vec<_> = out.iter().map(|t| t.tier).collect();
    assert_eq!(tiers, [QuotaTier::Premium]);

    let out = compute_tier_status(&limits((0, 0), (0, 0)), &[], now(), 80);
    assert!(out.is_empty(), "{out:?}");
}

#[test]
fn used_is_spent_plus_reserved_of_the_matching_bucket_and_period() {
    let rows = [
        row("total", PeriodType::Daily, day(), 30, 10),
        row("tier:premium", PeriodType::Daily, day(), 1, 1),
        // Other period starts and period types never count.
        row("total", PeriodType::Daily, day().pred_opt().unwrap(), 99, 0),
        row("total", PeriodType::Monthly, month(), 5, 0),
    ];
    let out = compute_tier_status(&limits((100, 100), (100, 100)), &rows, now(), 80);
    let total = &out[1].periods;
    assert_eq!(total[0].used, 40);
    assert_eq!(total[0].remaining, 60);
    assert_eq!(total[0].remaining_percentage, 60);
    assert_eq!(total[1].used, 5);
    let premium = &out[0].periods;
    assert_eq!(premium[0].used, 2);
    assert_eq!(premium[1].used, 0);
    assert_eq!(premium[1].remaining_percentage, 100);
}

#[test]
fn percentage_is_floored_and_flags_follow_the_threshold() {
    // (spent, expected remaining %, warning at 80, exhausted)
    let cases = [
        (0, 100, false, false),
        (20, 80, false, false),
        (21, 79, false, false),
        (79, 21, false, false),
        (80, 20, true, false),
        (81, 19, true, false),
        (99, 1, true, false),
        (100, 0, true, true),
        (150, 0, true, true),
    ];
    for (spent, pct, warning, exhausted) in cases {
        let rows = [row("total", PeriodType::Daily, day(), spent, 0)];
        let out = compute_tier_status(&limits((0, 0), (100, 0)), &rows, now(), 80);
        let p = &out[0].periods[0];
        assert_eq!(p.remaining_percentage, pct, "spent {spent}");
        assert_eq!(p.warning, warning, "spent {spent}");
        assert_eq!(p.exhausted, exhausted, "spent {spent}");
        assert!(p.remaining >= 0);
    }

    // Floor: 0.5% remaining is 0 -> exhausted.
    let rows = [row("total", PeriodType::Daily, day(), 995, 0)];
    let p = &compute_tier_status(&limits((0, 0), (1000, 0)), &rows, now(), 80)[0].periods[0];
    assert_eq!(p.remaining, 5);
    assert_eq!(p.remaining_percentage, 0);
    assert!(p.exhausted);
}

#[test]
fn threshold_boundaries() {
    let rows = [row("total", PeriodType::Daily, day(), 1, 0)];
    // 99% remaining: warning only when the threshold is 1 (99 <= 100 - 1).
    let at = |threshold| {
        compute_tier_status(&limits((0, 0), (100, 0)), &rows, now(), threshold)[0].periods[0]
            .warning
    };
    assert!(at(1));
    assert!(!at(2));
}

#[test]
fn huge_values_do_not_overflow() {
    let rows = [row("total", PeriodType::Daily, day(), i64::MAX, i64::MAX)];
    let p = &compute_tier_status(&limits((0, 0), (i64::MAX, 0)), &rows, now(), 80)[0].periods[0];
    assert_eq!(p.used, i64::MAX);
    assert_eq!(p.remaining, 0);
    assert!(p.exhausted);
    let p = &compute_tier_status(&limits((0, 0), (i64::MAX, 0)), &[], now(), 80)[0].periods[0];
    assert_eq!(p.remaining_percentage, 100);
}

#[test]
fn next_reset_is_next_utc_midnight_and_first_of_next_month() {
    let out = compute_tier_status(&limits((0, 0), (10, 10)), &[], now(), 80);
    let p = &out[0].periods;
    assert_eq!(
        p[0].next_reset,
        Utc.with_ymd_and_hms(2027, 1, 1, 0, 0, 0).unwrap()
    );
    assert_eq!(
        p[1].next_reset,
        Utc.with_ymd_and_hms(2027, 1, 1, 0, 0, 0).unwrap()
    );
    let mid = Utc.with_ymd_and_hms(2026, 2, 10, 13, 0, 0).unwrap();
    let p = &compute_tier_status(&limits((0, 0), (10, 10)), &[], mid, 80)[0].periods;
    assert_eq!(
        p[0].next_reset,
        Utc.with_ymd_and_hms(2026, 2, 11, 0, 0, 0).unwrap()
    );
    assert_eq!(
        p[1].next_reset,
        Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap()
    );
}
