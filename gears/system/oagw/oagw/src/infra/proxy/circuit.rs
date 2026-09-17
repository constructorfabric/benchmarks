//! Per-upstream circuit breaker (DESIGN "resilience").
//!
//! The breaker is *core data-plane logic*, not a plugin: it watches the
//! transport layer only, so a misbehaving upstream cannot exhaust the gateway.
//!
//! * **Closed** — requests flow; consecutive transport failures are counted.
//! * **Open** — the breaker refuses requests with
//!   [`DomainError::CircuitBreakerOpen`] until the cooldown elapses.
//! * **Half-open** — after the cooldown a single probe is allowed; success
//!   closes the breaker, failure re-opens it.
//!
//! Only *transport* failures (connection refused, DNS, TLS, timeouts) are
//! counted. An upstream answering `500` is reachable and does not trip the
//! breaker.

use std::time::{Duration, Instant};

use dashmap::DashMap;
use uuid::Uuid;

use crate::domain::error::DomainError;

/// Consecutive transport failures that open the breaker.
pub const FAILURE_THRESHOLD: u32 = 8;
/// How long an open breaker refuses requests before half-opening.
pub const COOLDOWN: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Closed,
    Open { since: Instant },
    HalfOpen,
}

#[derive(Debug, Clone)]
struct Breaker {
    phase: Phase,
    failures: u32,
}

/// The circuit breakers of the data plane, keyed by upstream id.
pub struct CircuitBreakers {
    breakers: DashMap<Uuid, Breaker>,
    threshold: u32,
    cooldown: Duration,
}

impl Default for CircuitBreakers {
    fn default() -> Self {
        Self::new(FAILURE_THRESHOLD, COOLDOWN)
    }
}

impl CircuitBreakers {
    /// A breaker set with an explicit failure threshold and cooldown.
    #[must_use]
    pub fn new(threshold: u32, cooldown: Duration) -> Self {
        Self {
            breakers: DashMap::new(),
            threshold: threshold.max(1),
            cooldown,
        }
    }

    /// True when a request to `upstream_id` may proceed.
    #[must_use]
    pub fn allows(&self, upstream_id: Uuid, now: Instant) -> bool {
        let Some(mut breaker) = self.breakers.get_mut(&upstream_id) else {
            return true;
        };
        match breaker.phase {
            Phase::Closed | Phase::HalfOpen => true,
            Phase::Open { since } => {
                if now.duration_since(since) >= self.cooldown {
                    breaker.phase = Phase::HalfOpen;
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Seconds until the breaker half-opens, when it is open.
    #[must_use]
    pub fn retry_after(&self, upstream_id: Uuid, now: Instant) -> Option<u64> {
        let breaker = self.breakers.get(&upstream_id)?;
        match breaker.phase {
            Phase::Open { since } => {
                let elapsed = now.duration_since(since);
                Some(self.cooldown.saturating_sub(elapsed).as_secs().max(1))
            }
            _ => None,
        }
    }

    /// Record a successful exchange.
    pub fn record_success(&self, upstream_id: Uuid) {
        if let Some(mut breaker) = self.breakers.get_mut(&upstream_id) {
            breaker.failures = 0;
            breaker.phase = Phase::Closed;
        }
    }

    /// Record a transport failure.
    pub fn record_failure(&self, upstream_id: Uuid, now: Instant) {
        let mut breaker = self.breakers.entry(upstream_id).or_insert(Breaker {
            phase: Phase::Closed,
            failures: 0,
        });
        breaker.failures += 1;
        if breaker.failures >= self.threshold || breaker.phase == Phase::HalfOpen {
            breaker.phase = Phase::Open { since: now };
            breaker.failures = 0;
        }
    }

    /// The error to surface when the breaker is open.
    #[must_use]
    pub fn error(&self, upstream_id: Uuid, now: Instant) -> DomainError {
        DomainError::CircuitBreakerOpen {
            detail: format!(
                "upstream {upstream_id} is temporarily unavailable after repeated transport failures"
            ),
            retry_after_seconds: self.retry_after(upstream_id, now),
        }
    }

    /// Number of tracked upstreams.
    #[must_use]
    pub fn len(&self) -> usize {
        self.breakers.len()
    }

    /// True when no upstream is tracked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.breakers.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_closed() {
        let breakers = CircuitBreakers::default();
        assert!(breakers.allows(Uuid::new_v4(), Instant::now()));
        assert!(breakers.is_empty());
    }

    #[test]
    fn opens_after_the_threshold_and_half_opens_after_the_cooldown() {
        let breakers = CircuitBreakers::new(2, Duration::from_secs(10));
        let upstream = Uuid::new_v4();
        let now = Instant::now();
        breakers.record_failure(upstream, now);
        assert!(breakers.allows(upstream, now));
        assert!(breakers.error(upstream, now).retry_after().is_none());

        // The second consecutive failure reaches the threshold and opens it.
        breakers.record_failure(upstream, now);
        assert!(!breakers.allows(upstream, now));
        assert_eq!(
            breakers.error(upstream, now).retry_after(),
            Some(Duration::from_secs(10))
        );

        // After the cooldown the breaker half-opens.
        assert!(breakers.allows(upstream, now + Duration::from_secs(11)));
        breakers.record_success(upstream);
        assert!(breakers.allows(upstream, now + Duration::from_secs(11)));
    }

    #[test]
    fn a_failure_while_half_open_reopens() {
        let breakers = CircuitBreakers::new(2, Duration::from_secs(1));
        let upstream = Uuid::new_v4();
        let now = Instant::now();
        breakers.record_failure(upstream, now);
        breakers.record_failure(upstream, now);
        breakers.record_failure(upstream, now);
        assert!(breakers.allows(upstream, now + Duration::from_secs(2)));
        breakers.record_failure(upstream, now + Duration::from_secs(2));
        assert!(!breakers.allows(upstream, now + Duration::from_secs(2)));
    }
}
