// Created: 2026-08-29 by Constructor Tech
//! Per-`(upstream_id, endpoint)` sliding-failure circuit breaker (DESIGN §14).
//!
//! Core data-plane logic, not a plugin: after `failure_threshold` consecutive
//! failures the breaker opens for `cool_down`; requests while open fail fast
//! with `503 CircuitBreakerOpen`; the first request after the cool-down is let
//! through (half-open) and a success closes the breaker.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use uuid::Uuid;

use crate::domain::error::OagwError;

/// Breaker state for one `(upstream_id, endpoint)` key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct BreakerState {
    consecutive_failures: u32,
    /// Set when the breaker tripped.
    opened_at: Option<Instant>,
    /// Set when a half-open probe is in flight.
    probing: bool,
}

impl BreakerState {
    fn open(&mut self) {
        self.opened_at = Some(Instant::now());
    }

    fn close(&mut self) {
        self.consecutive_failures = 0;
        self.opened_at = None;
        self.probing = false;
    }

    /// `true` when the breaker blocks traffic.
    fn is_open(&self, cooldown: Duration) -> bool {
        self.opened_at.is_some_and(|at| at.elapsed() < cooldown)
    }

    /// `true` when the cool-down expired and a probe is allowed.
    fn half_open_due(&self, cooldown: Duration) -> bool {
        self.opened_at.is_some_and(|at| at.elapsed() >= cooldown)
    }
}

/// Registry of breakers, keyed by `(upstream_id, endpoint)` and by upstream id.
pub struct CircuitBreakerRegistry {
    by_endpoint: Mutex<HashMap<(Uuid, String), BreakerState>>,
    failure_threshold: u32,
    cooldown: Duration,
}

impl std::fmt::Debug for CircuitBreakerRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CircuitBreakerRegistry")
            .field("failure_threshold", &self.failure_threshold)
            .field("cooldown", &self.cooldown)
            .finish_non_exhaustive()
    }
}

/// Circuit-breaker outcome of the pre-flight check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerCheck {
    /// Traffic is allowed.
    Allowed,
    /// Traffic is rejected: the breaker is open.
    Open,
}

impl Default for CircuitBreakerRegistry {
    fn default() -> Self {
        Self::new(5, Duration::from_secs(30))
    }
}

impl CircuitBreakerRegistry {
    /// Registry with the given failure threshold and cool-down.
    #[must_use]
    pub fn new(failure_threshold: u32, cooldown: Duration) -> Self {
        Self {
            by_endpoint: Mutex::new(HashMap::new()),
            failure_threshold: failure_threshold.max(1),
            cooldown,
        }
    }

    /// Pre-flight check.
    #[must_use]
    pub fn check(&self, upstream_id: Uuid, endpoint: &str) -> BreakerCheck {
        let mut by_endpoint = self.by_endpoint.lock();
        let state = by_endpoint
            .entry((upstream_id, endpoint.to_owned()))
            .or_default();
        if state.is_open(self.cooldown) {
            return BreakerCheck::Open;
        }
        if state.half_open_due(self.cooldown) {
            if state.probing {
                return BreakerCheck::Open;
            }
            state.probing = true;
        }
        BreakerCheck::Allowed
    }

    /// Record a successful response: closes the breaker and resets the counter.
    ///
    /// Returns the state the breaker moved to, or `None` when it was already
    /// closed — the data plane reports transitions (DESIGN §4.2).
    pub fn record_success(
        &self,
        upstream_id: Uuid,
        endpoint: &str,
    ) -> Option<crate::domain::ports::metrics::BreakerState> {
        let mut by_endpoint = self.by_endpoint.lock();
        let state = by_endpoint
            .entry((upstream_id, endpoint.to_owned()))
            .or_default();
        state.opened_at?;
        state.close();
        crate::infra::audit::breaker_transition(upstream_id, endpoint, "closed");
        Some(crate::domain::ports::metrics::BreakerState::Closed)
    }

    /// Record a failure: increments the counter and opens past the threshold.
    ///
    /// Returns the state the breaker moved to, or `None` when nothing changed.
    pub fn record_failure(
        &self,
        upstream_id: Uuid,
        endpoint: &str,
    ) -> Option<crate::domain::ports::metrics::BreakerState> {
        let threshold = self.failure_threshold;
        let mut by_endpoint = self.by_endpoint.lock();
        let state = by_endpoint
            .entry((upstream_id, endpoint.to_owned()))
            .or_default();
        // Only the transition out of a *blocking* state is an event: a failure
        // recorded while the breaker is already open just extends the trip.
        let was_open = state.is_open(self.cooldown);
        state.consecutive_failures = state.consecutive_failures.saturating_add(1);
        state.probing = false;
        if state.consecutive_failures >= threshold {
            state.open();
            if !was_open {
                crate::infra::audit::breaker_transition(upstream_id, endpoint, "opened");
                return Some(crate::domain::ports::metrics::BreakerState::Open);
            }
        }
        None
    }

    /// Consecutive failures recorded for an endpoint (test helper).
    #[must_use]
    pub fn failures(&self, upstream_id: Uuid, endpoint: &str) -> u32 {
        self.by_endpoint
            .lock()
            .get(&(upstream_id, endpoint.to_owned()))
            .map_or(0, |state| state.consecutive_failures)
    }
}

/// Map an open breaker to the wire error.
#[must_use]
pub fn breaker_open() -> OagwError {
    OagwError::CircuitBreakerOpen
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_after_threshold_and_resets_on_success() {
        let registry = CircuitBreakerRegistry::new(3, Duration::from_secs(30));
        let id = uuid::Uuid::new_v4();
        for _ in 0..3 {
            registry.record_failure(id, "a.example.com");
        }
        assert_eq!(registry.check(id, "a.example.com"), BreakerCheck::Open);
        registry.record_success(id, "a.example.com");
        assert_eq!(registry.check(id, "a.example.com"), BreakerCheck::Allowed);
        assert_eq!(registry.failures(id, "a.example.com"), 0);
    }

    #[test]
    fn keys_are_per_endpoint() {
        let registry = CircuitBreakerRegistry::new(2, Duration::from_secs(30));
        let id = uuid::Uuid::new_v4();
        registry.record_failure(id, "a.example.com");
        registry.record_failure(id, "b.example.com");
        assert_eq!(registry.check(id, "a.example.com"), BreakerCheck::Allowed);
    }

    #[test]
    fn half_open_after_cooldown() {
        let registry = CircuitBreakerRegistry::new(1, Duration::from_millis(1));
        let id = uuid::Uuid::new_v4();
        registry.record_failure(id, "a.example.com");
        assert_eq!(registry.check(id, "a.example.com"), BreakerCheck::Open);
        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(registry.check(id, "a.example.com"), BreakerCheck::Allowed);
        registry.record_failure(id, "a.example.com");
        assert_eq!(registry.check(id, "a.example.com"), BreakerCheck::Open);
    }
}
