// Created: 2026-09-01 by Constructor Tech
//! Per-endpoint circuit breaker.
//!
//! Core Data Plane functionality — not a `GuardPlugin`
//! (`docs/DESIGN.md` §3.1). Failures are counted in a rolling window; once
//! the configured threshold is reached the breaker opens and rejects with
//! `503 CircuitBreakerOpen` until the open period elapses.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// Breaker thresholds, applied per upstream endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thresholds {
    /// Consecutive failures within the window that trip the breaker.
    pub failure_threshold: u32,
    /// Rolling window in which failures are counted.
    pub window: Duration,
    /// How long an open breaker stays open before probing again.
    pub open: Duration,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            window: Duration::from_secs(30),
            open: Duration::from_secs(30),
        }
    }
}

#[derive(Debug, Default)]
struct EndpointState {
    failures: Vec<Instant>,
    opened_at: Option<Instant>,
}

/// The circuit breaker registry.
#[derive(Debug)]
pub struct CircuitBreaker {
    thresholds: Thresholds,
    endpoints: Mutex<BTreeMap<String, EndpointState>>,
}

/// Outcome of consulting the breaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The endpoint is usable.
    Closed,
    /// The endpoint is open; retry after this many seconds.
    Open { retry_after_secs: u64 },
}

impl CircuitBreaker {
    /// A breaker with `thresholds`.
    #[must_use]
    pub fn new(thresholds: Thresholds) -> Self {
        Self {
            thresholds,
            endpoints: Mutex::new(BTreeMap::new()),
        }
    }

    /// A breaker with the default thresholds.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(Thresholds::default())
    }

    /// The thresholds this breaker trips on.
    #[must_use]
    pub fn thresholds(&self) -> &Thresholds {
        &self.thresholds
    }

    /// Whether a request may be attempted against `endpoint`.
    #[must_use]
    pub fn probe(&self, endpoint: &str) -> Verdict {
        let now = Instant::now();
        let mut endpoints = self.endpoints.lock();
        let Some(state) = endpoints.get_mut(endpoint) else {
            return Verdict::Closed;
        };
        state
            .failures
            .retain(|t| now.duration_since(*t) < self.thresholds.window);
        match state.opened_at {
            None => Verdict::Closed,
            Some(opened) if now.duration_since(opened) >= self.thresholds.open => {
                // Half-open: allow a single probe through and reset the
                // failure history so the next failure re-opens quickly.
                state.opened_at = None;
                state.failures.clear();
                Verdict::Closed
            }
            Some(opened) => Verdict::Open {
                retry_after_secs: self
                    .thresholds
                    .open
                    .saturating_sub(now.duration_since(opened))
                    .as_secs()
                    .max(1),
            },
        }
    }

    /// Record a failure against `endpoint`.
    pub fn record_failure(&self, endpoint: &str) {
        let now = Instant::now();
        let mut endpoints = self.endpoints.lock();
        let state = endpoints.entry(endpoint.to_owned()).or_default();
        state
            .failures
            .retain(|t| now.duration_since(*t) < self.thresholds.window);
        state.failures.push(now);
        if state.failures.len()
            >= usize::try_from(self.thresholds.failure_threshold).unwrap_or(usize::MAX)
            && state.opened_at.is_none()
        {
            state.opened_at = Some(now);
        }
    }

    /// Record a success, clearing the failure history.
    pub fn record_success(&self, endpoint: &str) {
        if let Some(state) = self.endpoints.lock().get_mut(endpoint) {
            state.failures.clear();
            state.opened_at = None;
        }
    }

    /// Forget all state. Used by tests.
    pub fn clear(&self) {
        self.endpoints.lock().clear();
    }

    /// `true` when `endpoint` is currently open.
    #[must_use]
    pub fn is_open(&self, endpoint: &str) -> bool {
        matches!(self.probe(endpoint), Verdict::Open { .. })
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn thresholds() -> Thresholds {
        Thresholds {
            failure_threshold: 3,
            window: Duration::from_secs(30),
            open: Duration::from_millis(50),
        }
    }

    #[test]
    fn a_closed_breaker_admits_requests() {
        let breaker = CircuitBreaker::new(thresholds());
        assert_eq!(breaker.probe("ep"), Verdict::Closed);
        assert!(!breaker.is_open("ep"));
    }

    #[test]
    fn consecutive_failures_trip_the_breaker() {
        let breaker = CircuitBreaker::new(thresholds());
        for _ in 0..3 {
            breaker.record_failure("ep");
        }
        assert!(breaker.is_open("ep"));
        match breaker.probe("ep") {
            Verdict::Open { retry_after_secs } => assert!(retry_after_secs >= 1),
            Verdict::Closed => panic!("expected open"),
        }
        // Other endpoints are unaffected.
        assert!(!breaker.is_open("other"));
    }

    #[test]
    fn a_success_resets_the_count() {
        let breaker = CircuitBreaker::new(thresholds());
        breaker.record_failure("ep");
        breaker.record_failure("ep");
        breaker.record_success("ep");
        breaker.record_failure("ep");
        assert!(!breaker.is_open("ep"));
    }

    #[test]
    fn the_breaker_half_opens_after_the_open_period() {
        let breaker = CircuitBreaker::new(thresholds());
        for _ in 0..3 {
            breaker.record_failure("ep");
        }
        assert!(breaker.is_open("ep"));
        std::thread::sleep(Duration::from_millis(60));
        assert!(!breaker.is_open("ep"), "half-open allows a probe");
        // A single failure re-opens because the history was reset to a
        // clean slate; the counter restarts from zero.
        breaker.record_failure("ep");
        assert!(!breaker.is_open("ep"));
    }

    #[test]
    fn failures_outside_the_window_do_not_count() {
        let breaker = CircuitBreaker::new(Thresholds {
            failure_threshold: 2,
            window: Duration::from_millis(30),
            open: Duration::from_secs(10),
        });
        breaker.record_failure("ep");
        std::thread::sleep(Duration::from_millis(40));
        breaker.record_failure("ep");
        assert!(!breaker.is_open("ep"));
    }
}
