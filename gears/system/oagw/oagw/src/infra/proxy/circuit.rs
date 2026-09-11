// Updated: 2026-09-01 by Constructor Tech
//! Circuit breaker for the Data Plane.
//!
//! One breaker per upstream, keyed by upstream id. Consecutive upstream
//! failures inside the window trip it; while it is open every request to that
//! upstream is refused locally with a 503 rather than dialled, and it closes
//! itself after `open_duration`.

use std::time::Instant;

use dashmap::DashMap;

use crate::config::CircuitBreakerConfig;

/// The breaker's state, exposed for tests and for an operator's debugging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Closed,
    Open,
}

/// Whether a request may be dialled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Proceed,
    /// The breaker is open; refuse locally.
    Reject,
}

#[derive(Debug)]
struct Breaker {
    failures: u32,
    window_started: Instant,
    opened_at: Option<Instant>,
}

impl Breaker {
    fn new() -> Self {
        Self {
            failures: 0,
            window_started: Instant::now(),
            opened_at: None,
        }
    }

    fn state(&self, cfg: &CircuitBreakerConfig) -> State {
        match self.opened_at {
            Some(at) if at.elapsed() < cfg.open_duration => State::Open,
            _ => State::Closed,
        }
    }
}

/// The breaker table.
#[derive(Debug)]
pub struct CircuitBreakers {
    breakers: DashMap<String, Breaker>,
    config: CircuitBreakerConfig,
}

impl CircuitBreakers {
    #[must_use]
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            breakers: DashMap::new(),
            config,
        }
    }

    #[must_use]
    pub fn config(&self) -> &CircuitBreakerConfig {
        &self.config
    }

    /// Ask before dialling. A disabled breaker always proceeds.
    pub fn before(&self, upstream: &str) -> Decision {
        if !self.config.enabled {
            return Decision::Proceed;
        }
        match self.breakers.get(upstream) {
            Some(b) if b.state(&self.config) == State::Open => Decision::Reject,
            _ => Decision::Proceed,
        }
    }

    /// Record an upstream failure.
    pub fn on_failure(&self, upstream: &str) {
        if !self.config.enabled {
            return;
        }
        let mut entry = self
            .breakers
            .entry(upstream.to_owned())
            .or_insert_with(Breaker::new);
        let b = entry.value_mut();
        if b.window_started.elapsed() > self.config.window {
            b.failures = 0;
            b.window_started = Instant::now();
        }
        b.failures += 1;
        if b.failures >= self.config.failure_threshold {
            b.opened_at = Some(Instant::now());
        }
    }

    /// Record an upstream success, closing the breaker.
    pub fn on_success(&self, upstream: &str) {
        if let Some(mut entry) = self.breakers.get_mut(upstream) {
            let b = entry.value_mut();
            b.failures = 0;
            b.opened_at = None;
            b.window_started = Instant::now();
        }
    }

    #[cfg(test)]
    fn state_of(&self, upstream: &str) -> State {
        self.breakers
            .get(upstream)
            .map(|b| b.state(&self.config))
            .unwrap_or(State::Closed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn cfg() -> CircuitBreakerConfig {
        CircuitBreakerConfig {
            enabled: true,
            failure_threshold: 3,
            window: Duration::from_secs(30),
            open_duration: Duration::from_millis(50),
        }
    }

    #[test]
    fn trips_after_the_threshold_of_consecutive_failures() {
        let cb = CircuitBreakers::new(cfg());
        for _ in 0..2 {
            cb.on_failure("u");
        }
        assert_eq!(cb.state_of("u"), State::Closed);
        assert_eq!(cb.before("u"), Decision::Proceed);
        cb.on_failure("u");
        assert_eq!(cb.state_of("u"), State::Open);
        assert_eq!(cb.before("u"), Decision::Reject);
    }

    #[test]
    fn a_success_resets_the_count() {
        let cb = CircuitBreakers::new(cfg());
        cb.on_failure("u");
        cb.on_failure("u");
        cb.on_success("u");
        for _ in 0..2 {
            cb.on_failure("u");
        }
        assert_eq!(cb.state_of("u"), State::Closed);
    }

    #[test]
    fn open_half_closes_after_the_open_duration() {
        let cb = CircuitBreakers::new(cfg());
        for _ in 0..3 {
            cb.on_failure("u");
        }
        assert_eq!(cb.before("u"), Decision::Reject);
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(cb.before("u"), Decision::Proceed);
    }

    #[test]
    fn a_disabled_breaker_never_trips() {
        let mut c = cfg();
        c.enabled = false;
        let cb = CircuitBreakers::new(c);
        for _ in 0..10 {
            cb.on_failure("u");
        }
        assert_eq!(cb.state_of("u"), State::Closed);
        assert_eq!(cb.before("u"), Decision::Proceed);
    }

    #[test]
    fn breakers_are_per_upstream() {
        let cb = CircuitBreakers::new(cfg());
        for _ in 0..3 {
            cb.on_failure("a");
        }
        assert_eq!(cb.before("b"), Decision::Proceed);
    }
}
