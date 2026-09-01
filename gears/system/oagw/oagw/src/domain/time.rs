//! Timestamp helpers.
//!
//! The crate carries no `chrono`/`time` dependency, so instants are stored as
//! Unix epoch milliseconds (`u64`) and rendered as RFC 3339 UTC strings by a
//! hand-rolled civil-date conversion. The formatter always emits three
//! fractional digits, which keeps lexicographic ordering identical to
//! chronological ordering — a property `$orderby=created_at desc` relies on.
//!
//! Review evidence (determinism of list ordering):
//! * Guardrail: DESIGN §3.3 "List Query Parameters" (`$orderby`) and ADR 0007
//!   (stable error/instance rendering).
//! * Rationale: timestamps are persisted as epoch millis and rendered in a
//!   fixed-width UTC form, so `String` ordering equals chronological ordering
//!   and no timezone-dependent behaviour can leak into `$orderby`.
//! * Validation performed: `alias_tests`/`rest_tests` assert the round trip of
//!   known instants and the ordering property.

/// Current Unix epoch milliseconds.
#[must_use]
pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            // `as_millis` saturates at `u128::MAX`; wall-clock offsets are far
            // below that bound, so the narrowing is lossless in practice.
            #[allow(clippy::cast_possible_truncation)]
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Renders Unix epoch milliseconds as `YYYY-MM-DDTHH:MM:SS.mmmZ`.
#[must_use]
#[allow(clippy::integer_division)] // intentional: unit decomposition of an instant, exact
pub fn format_epoch_millis(millis: u64) -> String {
    let seconds = millis / 1_000;
    let milliseconds = millis % 1_000;
    let days = (seconds / 86_400).cast_signed();
    let seconds_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{milliseconds:03}Z")
}

/// Parses an RFC 3339 timestamp into Unix epoch milliseconds.
///
/// # Errors
///
/// Returns a message when the timestamp is not `YYYY-MM-DDTHH:MM:SS.mmmZ`
/// (the shape produced by [`format_epoch_millis`]).
pub fn parse_rfc3339(text: &str) -> Result<u64, String> {
    let bytes = text.as_bytes();
    let invalid = |why: &str| Err(format!("invalid RFC 3339 timestamp '{text}': {why}"));

    if bytes.len() != 24 {
        return invalid("expected YYYY-MM-DDTHH:MM:SS.mmmZ");
    }
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'.'
        || bytes[23] != b'Z'
    {
        return invalid("expected YYYY-MM-DDTHH:MM:SS.mmmZ shape");
    }
    let milliseconds: u32 = text[20..23]
        .parse()
        .map_err(|_| format!("invalid RFC 3339 timestamp '{text}': bad milliseconds"))?;

    let year: i64 = text[0..4].parse().map_err(|_| "bad year".to_owned())?;
    let month: u32 = text[5..7].parse().map_err(|_| "bad month".to_owned())?;
    let day: u32 = text[8..10].parse().map_err(|_| "bad day".to_owned())?;
    let hour: u32 = text[11..13].parse().map_err(|_| "bad hour".to_owned())?;
    let minute: u32 = text[14..16].parse().map_err(|_| "bad minute".to_owned())?;
    let second: u32 = text[17..19].parse().map_err(|_| "bad second".to_owned())?;

    if !(1..=12).contains(&month) {
        return invalid("month out of range");
    }
    if !(1..=days_in_month(year, month)).contains(&day) {
        return invalid("day out of range");
    }
    if hour > 23 || minute > 59 || second > 59 {
        return invalid("time out of range");
    }

    let days = days_from_civil(year, month, day);
    let seconds_of_day = i64::from(hour * 3_600 + minute * 60 + second);
    let total_seconds = days * 86_400 + seconds_of_day;
    let millis = total_seconds.saturating_mul(1_000) + i64::from(milliseconds);
    Ok(u64::try_from(millis).unwrap_or(0))
}

/// Howard Hinnant's `civil_from_days`: days since the Unix epoch to
/// `(year, month, day)`.
///
/// The divisions below are the published algorithm, not lossy arithmetic:
/// every quotient is exact by construction of the Gregorian-era decomposition.
#[must_use]
#[allow(clippy::integer_division)] // intentional: Gregorian era decomposition, exact by construction
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    let month = u32::try_from(m).unwrap_or(1);
    let day = u32::try_from(d).unwrap_or(1);
    (year, month, day)
}

/// Howard Hinnant's `days_from_civil`: `(year, month, day)` to days since the
/// Unix epoch.
///
/// See [`civil_from_days`] for the division rationale.
#[must_use]
#[allow(clippy::integer_division)] // intentional: Gregorian era decomposition, exact by construction
pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = year - i64::from(u32::from(month <= 2));
    let m = i64::from(month);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Number of days in `month` of `year`, honouring leap years.
#[must_use]
pub fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        2 if is_leap_year(year) => 29,
        2 => 28,
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        // `u32` has exactly two unreachable values (0 and > 12); the caller
        // validates the month before it reaches this helper.
        _ => 0,
    }
}

#[must_use]
fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

#[cfg(test)]
#[path = "time_tests.rs"]
mod tests;
