// Created: 2026-08-31 by Constructor Tech
//! Wall-clock access.
//!
//! Kept behind a function so tests can stay deterministic without a `clock`
//! abstraction threaded through every store call.

use std::time::{SystemTime, UNIX_EPOCH};

/// Current instant as epoch milliseconds.
#[must_use]
pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use super::now_millis;

    #[test]
    fn now_is_after_the_crate_epoch() {
        // 2026-01-01T00:00:00Z in epoch milliseconds.
        assert!(now_millis() >= 1_767_225_600_000);
    }
}
