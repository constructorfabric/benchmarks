//! Wall clock used for persisted timestamps.

use time::{OffsetDateTime, UtcOffset};

/// Current UTC time truncated to microseconds, the precision both supported
/// databases store, so a value written and read back compares equal.
#[must_use]
pub fn now_utc() -> OffsetDateTime {
    let now = OffsetDateTime::now_utc().to_offset(UtcOffset::UTC);
    now.replace_microsecond(now.microsecond()).unwrap_or(now)
}
