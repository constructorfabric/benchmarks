//! Identifier generation and RFC 3339 timestamps.
//!
//! The crate's dependency set has no random-number generator and no clock
//! formatting crate, so both are built here: UUIDs are `v5` over mixed
//! process-local entropy, and timestamps are formatted from `SystemTime`
//! with the civil-from-days conversion.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A fresh, collision-resistant UUID for a new resource.
#[must_use]
pub fn new_uuid() -> Uuid {
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let seed_a = RandomState::new().build_hasher().finish();
    let seed_b = RandomState::new().build_hasher().finish();
    let mut material = Vec::with_capacity(32);
    material.extend_from_slice(&seed_a.to_le_bytes());
    material.extend_from_slice(&seed_b.to_le_bytes());
    material.extend_from_slice(&counter.to_le_bytes());
    material.extend_from_slice(&nanos.to_le_bytes());
    Uuid::new_v5(&Uuid::nil(), &material)
}

/// A fresh typed identifier: `{prefix}{uuid}`.
#[must_use]
pub fn typed_id(prefix: &str) -> String {
    format!("{prefix}{}", new_uuid())
}

/// The current time formatted as RFC 3339 with millisecond precision.
#[must_use]
pub fn now_rfc3339() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format_epoch(now)
}

/// Formats seconds since the Unix epoch as RFC 3339 UTC.
///
/// The divisions are calendar arithmetic, never data-driven, so truncation is
/// the point.
#[allow(clippy::integer_division)]
#[must_use]
pub fn format_epoch(elapsed: Duration) -> String {
    let total_seconds = elapsed.as_secs();
    let millis = elapsed.subsec_millis();
    let days = i64::try_from(total_seconds / 86_400).unwrap_or_default();
    let seconds_of_day = total_seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Howard Hinnant's `civil_from_days`, counting days from 1970-01-01.
///
/// Calendar arithmetic: every division is by a fixed constant.
#[allow(clippy::integer_division)]
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = u32::try_from(day_of_year - (153 * mp + 2) / 5 + 1).unwrap_or_default();
    let month = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or_default();
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
#[path = "identifiers_tests.rs"]
mod tests;
