//! Application clock.
//!
//! On SQLite `time::OffsetDateTime` is stored as RFC 3339 text with trailing zeros of the
//! fraction removed, so values of different lengths do not sort lexically. `now()` always
//! returns a strictly increasing UTC instant whose nanosecond field ends with a non-zero digit,
//! so every stored timestamp has exactly nine fractional digits and text order equals time order.

use std::sync::Mutex;

use time::OffsetDateTime;

static LAST: Mutex<i128> = Mutex::new(0);

/// Current UTC time, strictly increasing within the process, with a 9-digit fraction.
#[must_use]
pub fn now() -> OffsetDateTime {
    let wall = OffsetDateTime::now_utc().unix_timestamp_nanos();
    let mut last = LAST.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut candidate = wall.max(*last + 1);
    if candidate % 10 == 0 {
        candidate += 1;
    }
    *last = candidate;
    drop(last);
    OffsetDateTime::from_unix_timestamp_nanos(candidate).unwrap_or_else(|_| OffsetDateTime::now_utc())
}

/// Normalizes an instant to the stored precision rules (non-zero last nanosecond digit).
#[must_use]
pub fn normalize(t: OffsetDateTime) -> OffsetDateTime {
    let ns = t.unix_timestamp_nanos();
    let fixed = if ns % 10 == 0 { ns + 1 } else { ns };
    OffsetDateTime::from_unix_timestamp_nanos(fixed).unwrap_or(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strictly_increasing_with_nine_digit_fraction() {
        let mut prev = now();
        for _ in 0..1000 {
            let t = now();
            assert!(t > prev);
            let s = t.format(&time::format_description::well_known::Rfc3339).unwrap();
            let frac = s.split('.').nth(1).unwrap();
            assert_eq!(frac.len(), 10, "{s}");
            prev = t;
        }
    }
}
