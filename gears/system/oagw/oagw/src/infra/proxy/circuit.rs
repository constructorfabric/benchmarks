//! Per-host circuit breaker (DESIGN.md §3.3, error kind
//! [`ErrorKind::CircuitBreakerOpen`]).
//!
//! The breaker is keyed by endpoint authority (`host:port`), trips after
//! [`FAILURE_THRESHOLD`] consecutive failures inside [`FAILURE_WINDOW`] and
//! stays open for [`OPEN_DURATION`]; the first call after that is a half-open
//! probe that either closes the breaker on success or re-opens it.

use std::time::{Duration, Instant};

use dashmap::DashMap;

use crate::domain::error::{DomainError, ErrorKind};

/// Consecutive failures that trip the breaker.
pub const FAILURE_THRESHOLD: u32 = 5;
/// How long a failure still counts towards the threshold.
pub const FAILURE_WINDOW: Duration = Duration::from_secs(60);
/// How long the breaker stays open before the half-open probe.
pub const OPEN_DURATION: Duration = Duration::from_secs(30);

/// State of one host's breaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CircuitState {
    /// Requests are allowed and failures are counted.
    Closed,
    /// Requests are refused.
    Open,
    /// Exactly one probe request is allowed through.
    HalfOpen,
}

#[derive(Debug)]
struct Circuit {
    state: CircuitState,
    failures: u32,
    last_failure: Option<Instant>,
    opened_at: Option<Instant>,
}

impl Circuit {
    fn closed() -> Self {
        Self {
            state: CircuitState::Closed,
            failures: 0,
            last_failure: None,
            opened_at: None,
        }
    }
}

/// Breakers of every endpoint host the data plane has dialed.
#[derive(Debug)]
pub struct CircuitBreakers {
    circuits: DashMap<String, parking_lot::Mutex<Circuit>>,
    threshold: u32,
    window: Duration,
    open_duration: Duration,
}

impl Default for CircuitBreakers {
    fn default() -> Self {
        Self::new()
    }
}

impl CircuitBreakers {
    /// Creates a breaker registry with the default thresholds.
    #[must_use]
    pub fn new() -> Self {
        Self {
            circuits: DashMap::new(),
            threshold: FAILURE_THRESHOLD,
            window: FAILURE_WINDOW,
            open_duration: OPEN_DURATION,
        }
    }

    /// Whether a request to `host` may proceed.
    ///
    /// An open breaker whose cooldown has elapsed admits exactly one probe.
    #[must_use]
    pub fn allows(&self, host: &str, now: Instant) -> bool {
        let circuit = self
            .circuits
            .entry(host.to_owned())
            .or_insert_with(|| parking_lot::Mutex::new(Circuit::closed()));
        let mut circuit = circuit.lock();
        match circuit.state {
            CircuitState::Closed => true,
            CircuitState::Open => {
                let elapsed = circuit.opened_at.is_some_and(|opened| {
                    now.checked_duration_since(opened)
                        .is_some_and(|d| d >= self.open_duration)
                });
                if elapsed {
                    circuit.state = CircuitState::HalfOpen;
                    true
                } else {
                    false
                }
            }
            CircuitState::HalfOpen => false,
        }
    }

    /// Records a successful dial: the breaker closes and the failure count
    /// resets.
    pub fn record_success(&self, host: &str) {
        let Some(circuit) = self.circuits.get(host) else {
            return;
        };
        let mut circuit = circuit.lock();
        circuit.state = CircuitState::Closed;
        circuit.failures = 0;
        circuit.last_failure = None;
        circuit.opened_at = None;
    }

    /// Records a failed dial and trips the breaker once the threshold is
    /// reached.
    pub fn record_failure(&self, host: &str, now: Instant) {
        let circuit = self
            .circuits
            .entry(host.to_owned())
            .or_insert_with(|| parking_lot::Mutex::new(Circuit::closed()));
        let mut circuit = circuit.lock();
        if circuit.state == CircuitState::HalfOpen {
            circuit.state = CircuitState::Open;
            circuit.opened_at = Some(now);
            return;
        }
        let in_window = circuit.last_failure.is_some_and(|last| {
            now.checked_duration_since(last)
                .is_some_and(|d| d <= self.window)
        });
        circuit.failures = if in_window {
            circuit.failures.saturating_add(1)
        } else {
            1
        };
        circuit.last_failure = Some(now);
        if circuit.failures >= self.threshold {
            circuit.state = CircuitState::Open;
            circuit.opened_at = Some(now);
        }
    }

    /// The rendered state of `host`, for tests and diagnostics.
    #[must_use]
    pub fn state_of(&self, host: &str) -> &'static str {
        let Some(circuit) = self.circuits.get(host) else {
            return "closed";
        };
        match circuit.lock().state {
            CircuitState::Closed => "closed",
            CircuitState::Open => "open",
            CircuitState::HalfOpen => "half-open",
        }
    }
}

/// The error an open breaker produces.
#[must_use]
pub fn open_error(host: &str) -> DomainError {
    DomainError::new(
        ErrorKind::CircuitBreakerOpen,
        format!("circuit breaker for {host} is open"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_closed_breaker_admits_requests_and_resets_on_success() {
        let breakers = CircuitBreakers::new();
        let start = Instant::now();
        for step in 0..4 {
            breakers.record_failure("a.example.com:443", start + Duration::from_secs(step));
        }
        assert_eq!(breakers.state_of("a.example.com:443"), "closed");
        assert!(breakers.allows("a.example.com:443", start + Duration::from_secs(5)));
        breakers.record_success("a.example.com:443");
        assert_eq!(breakers.state_of("a.example.com:443"), "closed");
    }

    #[test]
    fn the_threshold_opens_the_breaker() {
        let breakers = CircuitBreakers::new();
        let start = Instant::now();
        for step in 0u64..u64::from(FAILURE_THRESHOLD) {
            breakers.record_failure("b.example.com:443", start + Duration::from_secs(step));
        }
        assert_eq!(breakers.state_of("b.example.com:443"), "open");
        assert!(!breakers.allows("b.example.com:443", start + Duration::from_secs(1)));
        assert!(open_error("b.example.com:443").kind.status() == 503);
    }

    #[test]
    fn failures_outside_the_window_do_not_accumulate() {
        let breakers = CircuitBreakers::new();
        let start = Instant::now();
        for step in 0u64..u64::from(FAILURE_THRESHOLD) {
            breakers.record_failure(
                "c.example.com:443",
                start + Duration::from_secs(step * (FAILURE_WINDOW.as_secs() + 1)),
            );
        }
        assert_eq!(breakers.state_of("c.example.com:443"), "closed");
    }

    #[test]
    fn the_cooldown_admits_exactly_one_probe() {
        let breakers = CircuitBreakers::new();
        let start = Instant::now();
        for step in 0u64..u64::from(FAILURE_THRESHOLD) {
            breakers.record_failure("d.example.com:443", start + Duration::from_secs(step));
        }
        // The breaker opens at the threshold-crossing failure (`start + 4`).
        let after = start + Duration::from_secs(4) + OPEN_DURATION + Duration::from_secs(1);
        assert!(
            breakers.allows("d.example.com:443", after),
            "half-open probe admitted"
        );
        assert_eq!(breakers.state_of("d.example.com:443"), "half-open");
        assert!(!breakers.allows("d.example.com:443", after + Duration::from_millis(1)));
        breakers.record_failure("d.example.com:443", after);
        assert_eq!(breakers.state_of("d.example.com:443"), "open");
        assert!(!breakers.allows("d.example.com:443", after + Duration::from_secs(2)));
    }

    #[test]
    fn a_successful_probe_closes_the_breaker() {
        let breakers = CircuitBreakers::new();
        let start = Instant::now();
        for step in 0u64..u64::from(FAILURE_THRESHOLD) {
            breakers.record_failure("e.example.com:443", start + Duration::from_secs(step));
        }
        let after = start + Duration::from_secs(4) + OPEN_DURATION + Duration::from_secs(1);
        assert!(breakers.allows("e.example.com:443", after));
        breakers.record_success("e.example.com:443");
        assert_eq!(breakers.state_of("e.example.com:443"), "closed");
        assert!(breakers.allows("e.example.com:443", after + Duration::from_secs(1)));
    }
}
