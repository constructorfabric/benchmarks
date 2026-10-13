//! Application clock.

use time::OffsetDateTime;

/// The current UTC time truncated to microseconds, the precision shared by every supported
/// database, so a value survives a round trip through storage unchanged.
#[must_use]
pub fn now() -> OffsetDateTime {
    let now = OffsetDateTime::now_utc();
    let nanos = now.nanosecond();
    now.replace_nanosecond(nanos - nanos % 1_000).unwrap_or(now)
}
