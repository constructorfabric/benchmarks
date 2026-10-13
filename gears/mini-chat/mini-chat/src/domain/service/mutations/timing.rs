//! The mutation time rule.

use time::OffsetDateTime;

/// The mutation time: `clock`, raised to 1 microsecond after the later of the
/// target's `started_at` and its user message `created_at` when the clock is
/// not past them, so the replacement sorts after the replaced turn also under
/// clock skew between pods.
#[must_use]
pub fn mutation_time(
    clock: OffsetDateTime,
    target_started_at: OffsetDateTime,
    user_message_at: OffsetDateTime,
) -> OffsetDateTime {
    let later = target_started_at.max(user_message_at);
    if clock > later {
        clock
    } else {
        later + time::Duration::microseconds(1)
    }
}

#[cfg(test)]
#[path = "timing_tests.rs"]
mod tests;
