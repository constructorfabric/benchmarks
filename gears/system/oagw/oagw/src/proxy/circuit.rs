//! Circuit breaker.
//!
//! A host that accumulates `threshold` failures within `window` is opened for
//! `window`; while open the data plane answers 503 `circuit_breaker.open.v1`
//! without dialling the upstream.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

#[derive(Debug, Clone)]
struct Breaker {
    failures: Vec<Instant>,
    opened_at: Option<Instant>,
}

impl Breaker {
    fn new() -> Self {
        Self {
            failures: Vec::new(),
            opened_at: None,
        }
    }
}

/// Circuit breaker registry.
#[derive(Default)]
pub struct CircuitBreaker {
    breakers: Mutex<HashMap<String, Breaker>>,
}

impl CircuitBreaker {
    /// Empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the breaker for a host is open.
    #[must_use]
    pub fn is_open(&self, host: &str, threshold: u32, window: Duration) -> bool {
        let mut breakers = self.breakers.lock();
        let now = Instant::now();
        let breaker = breakers.entry(host.to_owned()).or_insert_with(Breaker::new);
        if let Some(opened_at) = breaker.opened_at {
            if now.duration_since(opened_at) >= window {
                *breaker = Breaker::new();
                return false;
            }
            return true;
        }
        breaker.failures.retain(|t| now.duration_since(*t) < window);
        if breaker.failures.len() >= usize::try_from(threshold).unwrap_or(usize::MAX) {
            breaker.opened_at = Some(now);
            return true;
        }
        false
    }

    /// Records a failure against a host.
    pub fn record_failure(&self, host: &str, threshold: u32, window: Duration) {
        let mut breakers = self.breakers.lock();
        let now = Instant::now();
        let breaker = breakers.entry(host.to_owned()).or_insert_with(Breaker::new);
        breaker.failures.retain(|t| now.duration_since(*t) < window);
        breaker.failures.push(now);
        if breaker.failures.len() >= usize::try_from(threshold).unwrap_or(u32::MAX as usize) {
            breaker.opened_at = Some(now);
        }
    }

    /// Records a success, resetting the breaker.
    pub fn record_success(&self, host: &str) {
        let mut breakers = self.breakers.lock();
        if let Some(breaker) = breakers.get_mut(host) {
            *breaker = Breaker::new();
        }
    }
}

/// Convenience constructor for a shared breaker registry.
#[must_use]
pub fn shared() -> Arc<CircuitBreaker> {
    Arc::new(CircuitBreaker::new())
}
