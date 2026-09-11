//! Per-host circuit breaker.
//!
//! Consecutive upstream failures trip the breaker for a cool-down; while it is
//! open the gateway refuses the request without dialling and answers with a
//! `503` whose `Retry-After` is the remaining cool-down.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// The states a breaker walks through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Requests flow through.
    Closed,
    /// Requests are refused until the cool-down elapses.
    Open,
}

/// The breaker for one upstream host.
#[derive(Debug)]
struct Breaker {
    state: State,
    failures: u32,
    opened_at: Option<Instant>,
}

impl Breaker {
    fn new() -> Self {
        Self {
            state: State::Closed,
            failures: 0,
            opened_at: None,
        }
    }

    fn open(&mut self) {
        self.state = State::Open;
        self.opened_at = Some(Instant::now());
    }

    fn close(&mut self) {
        self.state = State::Closed;
        self.failures = 0;
        self.opened_at = None;
    }
}

/// The breaker registry, keyed by host.
#[derive(Debug)]
pub struct CircuitBreakers {
    inner: Mutex<HashMap<String, Breaker>>,
    threshold: u32,
    cool_down: Duration,
}

impl CircuitBreakers {
    /// Builds the breaker registry.
    pub fn new(threshold: u32, cool_down: Duration) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            threshold: threshold.max(1),
            cool_down,
        }
    }

    /// Whether a dial to `host` is allowed right now.
    ///
    /// An open breaker whose cool-down has elapsed half-closes and is tried
    /// again; a failure afterwards reopens it for a further cool-down.
    pub fn allows(&self, host: &str) -> Option<Duration> {
        let mut guard = self.inner.lock();
        let breaker = guard.entry(host.to_string()).or_insert_with(Breaker::new);
        match breaker.state {
            State::Closed => None,
            State::Open => {
                let elapsed = breaker
                    .opened_at
                    .map(|at| at.elapsed())
                    .unwrap_or(self.cool_down);
                if elapsed >= self.cool_down {
                    breaker.close();
                    None
                } else {
                    Some(self.cool_down - elapsed)
                }
            }
        }
    }

    /// Records a successful exchange.
    pub fn record_success(&self, host: &str) {
        let mut guard = self.inner.lock();
        if let Some(breaker) = guard.get_mut(host) {
            breaker.close();
        }
    }

    /// Records a failed exchange.
    pub fn record_failure(&self, host: &str) {
        let mut guard = self.inner.lock();
        let breaker = guard.entry(host.to_string()).or_insert_with(Breaker::new);
        breaker.failures += 1;
        if breaker.failures >= self.threshold {
            breaker.open();
        }
    }

    /// The number of tracked hosts, used by tests.
    pub fn tracked(&self) -> usize {
        self.inner.lock().len()
    }

    /// The current state of a host's breaker.
    pub fn state_of(&self, host: &str) -> State {
        self.inner
            .lock()
            .get(host)
            .map(|breaker| breaker.state)
            .unwrap_or(State::Closed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_breaker_stays_closed_below_the_threshold() {
        let breakers = CircuitBreakers::new(3, Duration::from_millis(50));
        breakers.record_failure("a");
        breakers.record_failure("a");
        assert_eq!(breakers.state_of("a"), State::Closed);
        assert!(breakers.allows("a").is_none());
    }

    #[test]
    fn the_breaker_opens_at_the_threshold_and_recovers() {
        let breakers = CircuitBreakers::new(2, Duration::from_millis(20));
        breakers.record_failure("a");
        breakers.record_failure("a");
        assert_eq!(breakers.state_of("a"), State::Open);
        assert!(breakers.allows("a").is_some(), "still cooling down");
        std::thread::sleep(Duration::from_millis(40));
        assert!(breakers.allows("a").is_none(), "cool-down elapsed");
        breakers.record_success("a");
        assert_eq!(breakers.state_of("a"), State::Closed);
    }

    #[test]
    fn hosts_are_tracked_independently() {
        let breakers = CircuitBreakers::new(1, Duration::from_secs(1));
        breakers.record_failure("a");
        assert_eq!(breakers.state_of("a"), State::Open);
        assert_eq!(breakers.state_of("b"), State::Closed);
        assert_eq!(breakers.tracked(), 1);
    }
}
