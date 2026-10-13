#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 1: `cpt-cf-mini-chat-algo-quota-billing-period-keys`.

use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime, UtcOffset};

use super::{PeriodKeys, period_keys};
use crate::domain::service::quota::test_support::{date, utc};
use crate::domain::service::quota::types::QuotaPeriod;

fn rfc3339(at: OffsetDateTime) -> String {
    at.format(&Rfc3339).unwrap()
}

#[test]
fn the_last_second_of_a_day_still_belongs_to_that_day() {
    let keys = period_keys(utc(2026, 2, 28, 23, 59, 59));

    assert_eq!(keys.daily_start, date(2026, 2, 28));
    assert_eq!(keys.monthly_start, date(2026, 2, 1));
    assert_eq!(rfc3339(keys.daily_reset()), "2026-03-01T00:00:00Z");
    assert_eq!(rfc3339(keys.monthly_reset()), "2026-03-01T00:00:00Z");
}

#[test]
fn midnight_starts_the_next_day_and_for_the_first_of_a_month_the_next_month() {
    let keys = period_keys(utc(2026, 3, 1, 0, 0, 0));

    assert_eq!(keys.daily_start, date(2026, 3, 1));
    assert_eq!(keys.monthly_start, date(2026, 3, 1));
    assert_eq!(rfc3339(keys.daily_reset()), "2026-03-02T00:00:00Z");
    assert_eq!(rfc3339(keys.monthly_reset()), "2026-04-01T00:00:00Z");
}

#[test]
fn the_monthly_start_is_the_first_of_the_utc_month() {
    let keys = period_keys(utc(2026, 2, 28, 15, 30, 0));

    assert_eq!(keys.monthly_start, date(2026, 2, 1));
    assert_eq!(keys.daily_start, date(2026, 2, 28));
}

#[test]
fn the_last_instant_of_the_year_resets_into_the_next_year() {
    let at = utc(2026, 12, 31, 23, 59, 59) + Duration::microseconds(999_999);

    let keys = period_keys(at);

    assert_eq!(keys.daily_start, date(2026, 12, 31));
    assert_eq!(keys.monthly_start, date(2026, 12, 1));
    assert_eq!(rfc3339(keys.daily_reset()), "2027-01-01T00:00:00Z");
    assert_eq!(rfc3339(keys.monthly_reset()), "2027-01-01T00:00:00Z");
}

#[test]
fn month_ends_of_every_length_reset_on_the_first_of_the_next_month() {
    let cases = [
        (
            utc(2026, 1, 31, 23, 59, 59),
            "2026-02-01T00:00:00Z",
            "2026-02-01T00:00:00Z",
        ),
        (
            utc(2026, 4, 30, 23, 59, 59),
            "2026-05-01T00:00:00Z",
            "2026-05-01T00:00:00Z",
        ),
        (
            utc(2026, 2, 28, 0, 0, 0),
            "2026-03-01T00:00:00Z",
            "2026-03-01T00:00:00Z",
        ),
        // 2028 is a leap year: the day after February 28th is the 29th.
        (
            utc(2028, 2, 28, 23, 0, 0),
            "2028-02-29T00:00:00Z",
            "2028-03-01T00:00:00Z",
        ),
    ];
    for (at, daily_reset, monthly_reset) in cases {
        let keys = period_keys(at);

        assert_eq!(rfc3339(keys.daily_reset()), daily_reset, "{at}");
        assert_eq!(rfc3339(keys.monthly_reset()), monthly_reset, "{at}");
    }
}

#[test]
fn a_leap_day_exists_and_resets_to_the_first_of_march() {
    let keys = period_keys(utc(2028, 2, 29, 12, 0, 0));

    assert_eq!(keys.daily_start, date(2028, 2, 29));
    assert_eq!(rfc3339(keys.daily_reset()), "2028-03-01T00:00:00Z");
    assert_eq!(rfc3339(keys.monthly_reset()), "2028-03-01T00:00:00Z");
}

#[test]
fn a_non_utc_instant_is_converted_to_utc_before_the_keys_are_taken() {
    // 2026-03-01T01:30:00+03:00 is 2026-02-28T22:30:00Z.
    let at = utc(2026, 2, 28, 22, 30, 0).to_offset(UtcOffset::from_hms(3, 0, 0).unwrap());
    assert_eq!(at.day(), 1, "the local date is already March 1st");

    let keys = period_keys(at);

    assert_eq!(keys.daily_start, date(2026, 2, 28));
    assert_eq!(keys.monthly_start, date(2026, 2, 1));
    assert_eq!(rfc3339(keys.daily_reset()), "2026-03-01T00:00:00Z");
}

#[test]
fn the_reset_instants_are_utc_midnights_after_the_instant() {
    for at in [
        utc(2026, 3, 10, 12, 0, 0),
        utc(2026, 3, 10, 0, 0, 0),
        utc(2026, 12, 31, 23, 59, 59),
    ] {
        let keys = period_keys(at);

        for reset in [keys.daily_reset(), keys.monthly_reset()] {
            assert_eq!(reset.offset(), UtcOffset::UTC);
            assert_eq!((reset.hour(), reset.minute(), reset.second()), (0, 0, 0));
            assert_eq!(reset.nanosecond(), 0);
            assert!(reset > at, "{reset} after {at}");
        }
        assert!(keys.daily_reset() <= keys.monthly_reset());
    }
}

#[test]
fn the_accessors_pick_the_period() {
    let keys: PeriodKeys = period_keys(utc(2026, 3, 10, 12, 0, 0));

    assert_eq!(keys.start(QuotaPeriod::Daily), date(2026, 3, 10));
    assert_eq!(keys.start(QuotaPeriod::Monthly), date(2026, 3, 1));
    assert_eq!(keys.next_reset(QuotaPeriod::Daily), keys.daily_reset());
    assert_eq!(keys.next_reset(QuotaPeriod::Monthly), keys.monthly_reset());
}

#[test]
fn sub_second_precision_does_not_move_the_keys() {
    let at = utc(2026, 3, 10, 23, 59, 59) + Duration::nanoseconds(999_999_999);

    assert_eq!(period_keys(at).daily_start, date(2026, 3, 10));
}
