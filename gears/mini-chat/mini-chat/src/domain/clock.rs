//! Time source abstraction: production uses [`SystemClock`]; tests inject
//! [`FixedClock`] for deterministic timestamps.

use std::sync::Mutex;

use time::{Duration, OffsetDateTime};

/// Source of "now" for the domain (UTC).
pub trait Clock: Send + Sync {
    /// Current instant.
    fn now(&self) -> OffsetDateTime;
}

/// Wall-clock time (UTC).
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }
}

/// A settable clock for tests: returns the stored instant until moved.
#[derive(Debug)]
pub struct FixedClock {
    now: Mutex<OffsetDateTime>,
}

impl FixedClock {
    /// Clock frozen at `now`.
    #[must_use]
    pub fn new(now: OffsetDateTime) -> Self {
        Self {
            now: Mutex::new(now),
        }
    }

    /// Replace the current instant.
    pub fn set(&self, now: OffsetDateTime) {
        *self
            .now
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = now;
    }

    /// Move the clock forward (or backward for a negative duration).
    pub fn advance(&self, by: Duration) {
        let mut guard = self
            .now
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard += by;
    }
}

impl Clock for FixedClock {
    fn now(&self) -> OffsetDateTime {
        *self
            .now
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
#[path = "clock_tests.rs"]
mod clock_tests;
