//! Injectable clock abstraction (`cpt-cf-oagw-algo-ratelimit-consume`'s
//! `now` input), so token-bucket replenishment (`inst-ratelimit-consume-03`)
//! can be driven deterministically in tests without a real sleep.

use std::time::Instant;

/// A source of monotonic time for the token-bucket engine. Production code
/// always uses [`SystemClock`]; tests inject a [`ManualClock`] instead so
/// replenishment across a `sustained.window` can be exercised without
/// waiting in real time.
pub(crate) trait Clock: Send + Sync {
    fn now(&self) -> Instant;
}

/// The real, monotonic wall clock used on the live request path.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::Clock;
    use parking_lot::Mutex;
    use std::time::{Duration, Instant};

    /// A hand-advanced clock: `now()` returns whatever instant the test last
    /// set, letting `cpt-cf-oagw-state-ratelimit-bucket`'s Exhausted ->
    /// HasTokens replenishment transition (`inst-state-ratelimit-bucket-03`)
    /// be exercised deterministically and instantly.
    pub(crate) struct ManualClock {
        current: Mutex<Instant>,
    }

    impl ManualClock {
        pub(crate) fn new() -> Self {
            Self {
                current: Mutex::new(Instant::now()),
            }
        }

        pub(crate) fn advance(&self, duration: Duration) {
            let mut current = self.current.lock();
            *current += duration;
        }
    }

    impl Clock for ManualClock {
        fn now(&self) -> Instant {
            *self.current.lock()
        }
    }
}
