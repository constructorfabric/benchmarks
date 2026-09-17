//! Circuit breaker for upstream endpoints.
//!
//! Core data-plane functionality (not a plugin): when the failure count for
//! an upstream endpoint passes the threshold the breaker opens and the data
//! plane answers `503 CircuitBreakerOpen` without dialling. After the cool
//! period the breaker half-opens and lets one request through.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;

use crate::error::{ErrorKind, OagwError};

/// Breaker state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Requests flow normally.
    Closed,
    /// Tripped: reject immediately.
    Open,
    /// Cool period elapsed: admit a single probe request.
    HalfOpen,
}

#[derive(Debug)]
struct Breaker {
    state: State,
    failures: u32,
    opened_at: Option<Instant>,
    in_flight_probe: bool,
}

impl Breaker {
    fn new() -> Self {
        Self {
            state: State::Closed,
            failures: 0,
            opened_at: None,
            in_flight_probe: false,
        }
    }
}

/// Configuration for the breaker.
#[derive(Debug, Clone, Copy)]
pub struct BreakerPolicy {
    /// Consecutive failures that trip the breaker.
    pub threshold: u32,
    /// Time the breaker stays open before half-opening.
    pub cool_off: Duration,
}

impl Default for BreakerPolicy {
    fn default() -> Self {
        Self {
            threshold: 5,
            cool_off: Duration::from_secs(30),
        }
    }
}

/// Shared breaker registry.
#[derive(Debug, Default)]
pub struct CircuitBreaker {
    breakers: DashMap<String, Breaker>,
    policy: parking_lot::RwLock<BreakerPolicy>,
}

impl CircuitBreaker {
    /// Create a breaker registry with default policy.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the policy (used by gear configuration).
    pub fn set_policy(&self, policy: BreakerPolicy) {
        *self.policy.write() = policy;
    }

    /// Guard against a call on `key`.
    ///
    /// Returns `Err` when the breaker is open, blocking the request.
    pub fn before(&self, key: &str) -> Result<(), OagwError> {
        let cool_off = self.policy.read().cool_off;
        let mut entry = self.breakers.entry(key.to_owned()).or_insert_with(Breaker::new);
        match entry.state {
            State::Closed => Ok(()),
            State::Open => {
                if entry
                    .opened_at
                    .is_some_and(|t| t.elapsed() >= cool_off)
                {
                    entry.state = State::HalfOpen;
                    entry.in_flight_probe = true;
                    Ok(())
                } else {
                    Err(OagwError::new(
                        ErrorKind::CircuitBreakerOpen,
                        format!("circuit breaker for {key} is open"),
                    ))
                }
            }
            State::HalfOpen => {
                if entry.in_flight_probe {
                    Err(OagwError::new(
                        ErrorKind::CircuitBreakerOpen,
                        format!("circuit breaker for {key} is open"),
                    ))
                } else {
                    entry.in_flight_probe = true;
                    Ok(())
                }
            }
        }
    }

    /// Record a successful call.
    pub fn success(&self, key: &str) {
        let mut entry = self.breakers.entry(key.to_owned()).or_insert_with(Breaker::new);
        entry.failures = 0;
        entry.state = State::Closed;
        entry.opened_at = None;
        entry.in_flight_probe = false;
    }

    /// Record a failed call.
    pub fn failure(&self, key: &str) {
        let threshold = self.policy.read().threshold;
        let mut entry = self.breakers.entry(key.to_owned()).or_insert_with(Breaker::new);
        if entry.state == State::HalfOpen {
            entry.state = State::Open;
            entry.opened_at = Some(Instant::now());
            entry.in_flight_probe = false;
            return;
        }
        entry.failures = entry.failures.saturating_add(1);
        if entry.failures >= threshold.max(1) {
            entry.state = State::Open;
            entry.opened_at = Some(Instant::now());
        }
    }

    /// Current state for `key`.
    #[must_use]
    pub fn state(&self, key: &str) -> State {
        self.breakers
            .get(key)
            .map(|b| b.state)
            .unwrap_or(State::Closed)
    }
}

/// Shared breaker handle.
pub type SharedBreaker = Arc<CircuitBreaker>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closed_until_threshold() {
        let b = CircuitBreaker::new();
        for _ in 0..4 {
            b.failure("k");
        }
        assert_eq!(b.state("k"), State::Closed);
        assert!(b.before("k").is_ok());
        b.failure("k");
        assert_eq!(b.state("k"), State::Open);
        assert!(b.before("k").is_err());
    }

    #[test]
    fn success_resets_counter() {
        let b = CircuitBreaker::new();
        for _ in 0..4 {
            b.failure("k");
            b.success("k");
        }
        assert_eq!(b.state("k"), State::Closed);
    }

    #[test]
    fn open_breaker_produces_503() {
        let b = CircuitBreaker::new();
        for _ in 0..5 {
            b.failure("k");
        }
        let err = b.before("k").unwrap_err();
        assert_eq!(err.kind, ErrorKind::CircuitBreakerOpen);
        assert_eq!(err.kind.status(), http::StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn independent_keys() {
        let b = CircuitBreaker::new();
        for _ in 0..5 {
            b.failure("a");
        }
        assert!(b.before("b").is_ok());
    }
}
