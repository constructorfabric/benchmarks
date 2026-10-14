//! Period boundary tests: the four normative examples of DESIGN 5.4.2, month, leap-day and year
//! ends, offsets converted to UTC, determinism from one instant and the `next_reset` instants.
//!
//! Slice S1-all: algorithm `cpt-cf-mini-chat-algo-quota-and-billing-period-boundaries`,
//! definition of done `cpt-cf-mini-chat-dod-quota-and-billing-period-buckets`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use time::{Date, Month, OffsetDateTime, Time, UtcOffset};

use super::{PeriodStarts, PeriodType, next_reset};
use crate::test_support::quota::{date, utc, utc_ms};

fn starts(instant: OffsetDateTime) -> (Date, Date) {
    let starts = PeriodStarts::of(instant);
    (starts.daily, starts.monthly)
}

#[test]
fn the_period_values_are_the_row_keys() {
    assert_eq!(PeriodType::Daily.as_str(), "daily");
    assert_eq!(PeriodType::Monthly.as_str(), "monthly");
    assert_eq!(PeriodType::ALL, [PeriodType::Daily, PeriodType::Monthly]);
}

// ── the four normative examples ──────────────────────────────────────────────

#[test]
fn mid_afternoon_of_the_last_february_day() {
    assert_eq!(
        starts(utc(2026, 2, 28, 15, 30, 0)),
        (date(2026, 2, 28), date(2026, 2, 1))
    );
}

#[test]
fn the_last_second_of_a_day_is_still_that_day() {
    let (daily, _) = starts(utc(2026, 2, 28, 23, 59, 59));
    assert_eq!(daily, date(2026, 2, 28));
}

#[test]
fn midnight_starts_the_next_day_and_the_next_month() {
    assert_eq!(
        starts(utc(2026, 3, 1, 0, 0, 0)),
        (date(2026, 3, 1), date(2026, 3, 1))
    );
}

#[test]
fn start_of_period_selects_the_matching_date() {
    let periods = PeriodStarts::of(utc(2026, 2, 28, 15, 30, 0));
    assert_eq!(periods.start_of(PeriodType::Daily), date(2026, 2, 28));
    assert_eq!(periods.start_of(PeriodType::Monthly), date(2026, 2, 1));
}

// ── ends of month, leap day and year ─────────────────────────────────────────

#[test]
fn the_end_of_a_thirty_one_day_month() {
    assert_eq!(
        starts(utc(2026, 1, 31, 23, 59, 59)),
        (date(2026, 1, 31), date(2026, 1, 1))
    );
}

#[test]
fn the_leap_day() {
    assert_eq!(
        starts(utc(2028, 2, 29, 12, 0, 0)),
        (date(2028, 2, 29), date(2028, 2, 1))
    );
    assert_eq!(
        starts(utc(2028, 3, 1, 0, 0, 0)),
        (date(2028, 3, 1), date(2028, 3, 1))
    );
}

#[test]
fn the_end_of_the_year_and_the_first_instant_of_the_next() {
    assert_eq!(
        starts(utc(2026, 12, 31, 23, 59, 59)),
        (date(2026, 12, 31), date(2026, 12, 1))
    );
    assert_eq!(
        starts(utc(2027, 1, 1, 0, 0, 0)),
        (date(2027, 1, 1), date(2027, 1, 1))
    );
}

#[test]
fn sub_second_precision_stays_in_the_day_of_the_instant() {
    assert_eq!(
        starts(utc_ms(2026, 2, 28, 23, 59, 59, 999)),
        (date(2026, 2, 28), date(2026, 2, 1))
    );
}

// ── UTC and determinism ──────────────────────────────────────────────────────

#[test]
fn an_instant_with_an_offset_is_converted_to_utc_first() {
    // 2026-03-01T01:30:00+02:00 is 2026-02-28T23:30:00Z
    let local = Date::from_calendar_date(2026, Month::March, 1)
        .unwrap()
        .with_time(Time::from_hms(1, 30, 0).unwrap())
        .assume_offset(UtcOffset::from_hms(2, 0, 0).unwrap());
    assert_eq!(starts(local), (date(2026, 2, 28), date(2026, 2, 1)));

    // 2026-02-28T23:30:00-05:00 is 2026-03-01T04:30:00Z
    let west = Date::from_calendar_date(2026, Month::February, 28)
        .unwrap()
        .with_time(Time::from_hms(23, 30, 0).unwrap())
        .assume_offset(UtcOffset::from_hms(-5, 0, 0).unwrap());
    assert_eq!(starts(west), (date(2026, 3, 1), date(2026, 3, 1)));
}

#[test]
fn two_requests_in_the_same_utc_second_compute_identical_values() {
    let a = PeriodStarts::of(utc_ms(2026, 2, 28, 12, 0, 0, 10));
    let b = PeriodStarts::of(utc_ms(2026, 2, 28, 12, 0, 0, 990));
    assert_eq!(a, b);
}

#[test]
fn the_starts_of_the_started_at_of_a_turn_are_the_starts_of_its_preflight_instant() {
    // the orphan derivation uses the same function on `chat_turns.started_at`
    let preflight = utc_ms(2026, 2, 28, 23, 59, 59, 999);
    assert_eq!(PeriodStarts::of(preflight), PeriodStarts::of(preflight));
    // and it does not move with a later settlement clock
    assert_ne!(
        PeriodStarts::of(preflight),
        PeriodStarts::of(utc(2026, 3, 1, 0, 0, 1))
    );
}

// ── next reset ───────────────────────────────────────────────────────────────

#[test]
fn the_daily_reset_is_midnight_utc_of_the_next_day() {
    assert_eq!(
        next_reset(PeriodType::Daily, utc(2026, 2, 28, 15, 30, 0)),
        utc(2026, 3, 1, 0, 0, 0)
    );
    assert_eq!(
        next_reset(PeriodType::Daily, utc(2026, 5, 10, 0, 0, 1)),
        utc(2026, 5, 11, 0, 0, 0)
    );
}

#[test]
fn the_monthly_reset_is_midnight_utc_of_the_first_of_the_next_month() {
    assert_eq!(
        next_reset(PeriodType::Monthly, utc(2026, 2, 28, 15, 30, 0)),
        utc(2026, 3, 1, 0, 0, 0)
    );
    assert_eq!(
        next_reset(PeriodType::Monthly, utc(2026, 1, 1, 0, 0, 0)),
        utc(2026, 2, 1, 0, 0, 0)
    );
}

#[test]
fn the_reset_rolls_over_the_year() {
    assert_eq!(
        next_reset(PeriodType::Daily, utc(2026, 12, 31, 23, 59, 59)),
        utc(2027, 1, 1, 0, 0, 0)
    );
    assert_eq!(
        next_reset(PeriodType::Monthly, utc(2026, 12, 15, 8, 0, 0)),
        utc(2027, 1, 1, 0, 0, 0)
    );
}

#[test]
fn an_instant_exactly_at_a_boundary_belongs_to_the_new_period() {
    assert_eq!(
        next_reset(PeriodType::Daily, utc(2026, 3, 1, 0, 0, 0)),
        utc(2026, 3, 2, 0, 0, 0)
    );
    assert_eq!(
        next_reset(PeriodType::Monthly, utc(2026, 3, 1, 0, 0, 0)),
        utc(2026, 4, 1, 0, 0, 0)
    );
}

#[test]
fn the_reset_is_computed_in_utc_for_an_instant_with_an_offset() {
    let local = Date::from_calendar_date(2026, Month::March, 1)
        .unwrap()
        .with_time(Time::from_hms(1, 30, 0).unwrap())
        .assume_offset(UtcOffset::from_hms(2, 0, 0).unwrap());
    // 2026-02-28T23:30:00Z: the daily reset is 2026-03-01T00:00:00Z
    assert_eq!(
        next_reset(PeriodType::Daily, local),
        utc(2026, 3, 1, 0, 0, 0)
    );
}

#[test]
fn the_reset_is_an_rfc_3339_utc_instant() {
    let reset = next_reset(PeriodType::Daily, utc(2026, 2, 28, 15, 30, 0));
    assert_eq!(reset.offset(), UtcOffset::UTC);
    let text = reset
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    assert_eq!(text, "2026-03-01T00:00:00Z");
}
