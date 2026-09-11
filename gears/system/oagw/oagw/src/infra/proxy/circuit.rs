//! Circuit breaker — core gateway resilience, not a plugin (ADR-0002).
//!
//! Threshold from `cpt-cf-oagw-nfr-high-availability`: the breaker trips
//! within 5 failed requests in a 30 s window, stays open for the cool-down,
//! then admits a single probe (half-open) before closing again.

use std::time::{Duration, Instant};

use dashmap::DashMap;

/// Failures inside [`WINDOW`] that trip the breaker.
pub const FAILURE_THRESHOLD: usize = 5;
/// Sliding window over which failures are counted.
pub const WINDOW: Duration = Duration::from_secs(30);
/// How long the breaker stays open before admitting a probe.
pub const COOL_DOWN: Duration = Duration::from_secs(30);

/// Breaker state, as reported to metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    /// Requests flow.
    Closed,
    /// A single probe is admitted.
    HalfOpen,
    /// Requests are refused.
    Open,
}

impl BreakerState {
    /// Metric label.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            BreakerState::Closed => "closed",
            BreakerState::HalfOpen => "half_open",
            BreakerState::Open => "open",
        }
    }

    /// Gauge value: `0` closed, `1` half-open, `2` open.
    #[must_use]
    pub fn as_gauge(self) -> i64 {
        match self {
            BreakerState::Closed => 0,
            BreakerState::HalfOpen => 1,
            BreakerState::Open => 2,
        }
    }
}

#[derive(Debug)]
struct BreakerEntry {
    failures: Vec<Instant>,
    opened_at: Option<Instant>,
    probe_in_flight: bool,
}

impl BreakerEntry {
    fn new() -> Self {
        Self {
            failures: Vec::new(),
            opened_at: None,
            probe_in_flight: false,
        }
    }

    fn state(&mut self, now: Instant) -> BreakerState {
        match self.opened_at {
            None => BreakerState::Closed,
            Some(at) if now.saturating_duration_since(at) >= COOL_DOWN => BreakerState::HalfOpen,
            Some(_) => BreakerState::Open,
        }
    }
}

/// Per-endpoint circuit breakers.
#[derive(Default)]
pub struct CircuitBreakerRegistry {
    entries: DashMap<String, BreakerEntry>,
}

/// What a caller should do with the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Proceed.
    Allow,
    /// Proceed; this is the half-open probe.
    Probe,
    /// Refuse with `503 CircuitBreakerOpen`.
    Refuse {
        /// Seconds until the breaker admits a probe.
        retry_after: u64,
    },
}

impl CircuitBreakerRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Decide whether a request to `key` may proceed.
    #[must_use]
    pub fn admit(&self, key: &str) -> Admission {
        let now = Instant::now();
        let mut entry = self
            .entries
            .entry(key.to_owned())
            .or_insert_with(BreakerEntry::new);
        match entry.state(now) {
            BreakerState::Closed => Admission::Allow,
            BreakerState::HalfOpen => {
                if entry.probe_in_flight {
                    Admission::Refuse { retry_after: 1 }
                } else {
                    entry.probe_in_flight = true;
                    Admission::Probe
                }
            }
            BreakerState::Open => {
                let elapsed = entry
                    .opened_at
                    .map_or(Duration::ZERO, |at| now.saturating_duration_since(at));
                let remaining = COOL_DOWN.saturating_sub(elapsed).as_secs().max(1);
                Admission::Refuse {
                    retry_after: remaining,
                }
            }
        }
    }

    /// Record a successful exchange: the breaker closes and history is
    /// cleared.
    pub fn record_success(&self, key: &str) -> BreakerState {
        let mut entry = self
            .entries
            .entry(key.to_owned())
            .or_insert_with(BreakerEntry::new);
        let previous = entry.state(Instant::now());
        entry.failures.clear();
        entry.opened_at = None;
        entry.probe_in_flight = false;
        previous
    }

    /// Record a failed exchange, tripping the breaker at the threshold.
    /// Returns the state after the update.
    pub fn record_failure(&self, key: &str) -> BreakerState {
        let now = Instant::now();
        let mut entry = self
            .entries
            .entry(key.to_owned())
            .or_insert_with(BreakerEntry::new);
        entry.probe_in_flight = false;
        entry
            .failures
            .retain(|at| now.saturating_duration_since(*at) < WINDOW);
        entry.failures.push(now);
        if entry.failures.len() >= FAILURE_THRESHOLD {
            entry.opened_at = Some(now);
            entry.failures.clear();
            return BreakerState::Open;
        }
        BreakerState::Closed
    }

    /// Current state of `key`, without side effects on the probe slot.
    #[must_use]
    pub fn state(&self, key: &str) -> BreakerState {
        let now = Instant::now();
        self.entries
            .get_mut(key)
            .map_or(BreakerState::Closed, |mut entry| entry.state(now))
    }

    /// Drop the breaker for `key` — used when an upstream is deleted.
    pub fn forget(&self, key: &str) {
        self.entries.remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::{Admission, BreakerState, CircuitBreakerRegistry, FAILURE_THRESHOLD};

    #[test]
    fn a_fresh_breaker_is_closed() {
        let registry = CircuitBreakerRegistry::new();
        assert_eq!(registry.admit("api.openai.com"), Admission::Allow);
        assert_eq!(registry.state("api.openai.com"), BreakerState::Closed);
    }

    #[test]
    fn the_breaker_trips_at_the_threshold() {
        let registry = CircuitBreakerRegistry::new();
        let key = "api.openai.com";
        for i in 1..FAILURE_THRESHOLD {
            assert_eq!(
                registry.record_failure(key),
                BreakerState::Closed,
                "still closed after {i} failures"
            );
            assert_eq!(registry.admit(key), Admission::Allow);
        }
        assert_eq!(registry.record_failure(key), BreakerState::Open);
        match registry.admit(key) {
            Admission::Refuse { retry_after } => assert!(retry_after >= 1),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_success_resets_the_failure_history() {
        let registry = CircuitBreakerRegistry::new();
        let key = "api.openai.com";
        for _ in 0..(FAILURE_THRESHOLD - 1) {
            registry.record_failure(key);
        }
        registry.record_success(key);
        // The counter restarted, so the threshold is a full run away again.
        for _ in 0..(FAILURE_THRESHOLD - 1) {
            assert_eq!(registry.record_failure(key), BreakerState::Closed);
        }
    }

    #[test]
    fn breakers_are_per_key() {
        let registry = CircuitBreakerRegistry::new();
        for _ in 0..FAILURE_THRESHOLD {
            registry.record_failure("a");
        }
        assert!(matches!(registry.admit("a"), Admission::Refuse { .. }));
        assert_eq!(registry.admit("b"), Admission::Allow);
    }

    #[test]
    fn forget_clears_the_breaker() {
        let registry = CircuitBreakerRegistry::new();
        for _ in 0..FAILURE_THRESHOLD {
            registry.record_failure("a");
        }
        registry.forget("a");
        assert_eq!(registry.admit("a"), Admission::Allow);
    }

    #[test]
    fn state_labels_and_gauges_are_stable() {
        assert_eq!(BreakerState::Closed.as_str(), "closed");
        assert_eq!(BreakerState::HalfOpen.as_str(), "half_open");
        assert_eq!(BreakerState::Open.as_str(), "open");
        assert_eq!(BreakerState::Closed.as_gauge(), 0);
        assert_eq!(BreakerState::HalfOpen.as_gauge(), 1);
        assert_eq!(BreakerState::Open.as_gauge(), 2);
    }
}
