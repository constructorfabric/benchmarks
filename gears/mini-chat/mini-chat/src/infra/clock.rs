//! The production application clock of the quota rules.

use time::OffsetDateTime;

use crate::domain::quota::Clock;

/// The system UTC clock: the one instant the preflight reads, from which the period starts and
/// `chat_turns.started_at` are derived.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> OffsetDateTime {
        // microseconds, the precision every supported database keeps, so the instant written as
        // `started_at` reads back equal and the period starts never depend on lost precision
        let now = OffsetDateTime::now_utc();
        now.replace_microsecond(now.microsecond()).unwrap_or(now)
    }
}
