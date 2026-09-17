//! Token-bucket rate limiting (ADR-0003).
//!
//! One bucket per `(config, scope-key)` pair, replenished lazily on access:
//! `tokens = min(capacity, tokens + elapsed * refill_rate)`. Buckets live in
//! the data plane (in-process, <1ms checks); hierarchical limits are resolved
//! before a bucket is acquired via `min(ancestor, descendant)`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;

use crate::domain::error::OagwError;
use crate::domain::model::RateLimitConfig;

/// Outcome of an acquisition attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateDecision {
    /// `true` when the cost was admitted.
    pub allowed: bool,
    /// Bucket capacity.
    pub limit: u64,
    /// Tokens left after the attempt.
    pub remaining: u64,
    /// Unix seconds when the bucket is fully replenished.
    pub reset_epoch_secs: u64,
    /// Seconds until at least `cost` tokens are available (0 when allowed).
    pub retry_after_secs: u64,
}

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    capacity: f64,
    refill_per_second: f64,
    last_update: Instant,
}

impl Bucket {
    fn new(capacity: u64, refill_per_second: f64) -> Self {
        Self {
            tokens: capacity as f64,
            capacity: capacity as f64,
            refill_per_second,
            last_update: Instant::now(),
        }
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_update).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_per_second).min(self.capacity);
        self.last_update = now;
    }

    fn try_acquire(&mut self, cost: u64) -> bool {
        self.refill();
        if self.tokens >= cost as f64 {
            self.tokens -= cost as f64;
            true
        } else {
            false
        }
    }

    fn retry_after(&self, cost: u64) -> u64 {
        let deficit = cost as f64 - self.tokens;
        if deficit <= 0.0 || self.refill_per_second <= 0.0 {
            return 0;
        }
        (deficit / self.refill_per_second).ceil() as u64
    }
}

/// Horizon reported for a bucket that cannot refill within any representable
/// window (`rate == 0`, or a deficit so large the quotient loses meaning).
///
/// The reset instant is added to the current epoch, so an unbounded quotient
/// would overflow the `u64` addition; clamping keeps the header well-formed
/// and still tells the caller "not any time soon".
pub const RESET_HORIZON_SECS: u64 = 365 * 24 * 60 * 60;

/// Seconds until the bucket is full again.
///
/// A zero refill rate never replenishes, so it reports the full horizon
/// instead of dividing by zero.
#[must_use]
fn secs_to_full(remaining: u64, capacity: u64, refill_per_second: f64) -> u64 {
    if remaining >= capacity {
        return 0;
    }
    if refill_per_second <= 0.0 || !refill_per_second.is_finite() {
        return RESET_HORIZON_SECS;
    }
    let deficit = capacity as f64 - remaining as f64;
    let secs = (deficit / refill_per_second).ceil();
    if !secs.is_finite() {
        return RESET_HORIZON_SECS;
    }
    secs.clamp(0.0, RESET_HORIZON_SECS as f64) as u64
}

/// Token-bucket store keyed by an opaque scope string.
#[derive(Debug, Default)]
pub struct RateLimiter {
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl RateLimiter {
    /// Creates an empty limiter.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Attempts to consume `cost` tokens for `scope_key`.
    #[must_use]
    pub fn try_acquire(&self, config: &RateLimitConfig, scope_key: &str, cost: u64) -> RateDecision {
        let capacity = config.capacity();
        let refill = config.refill_per_second();
        let mut buckets = self.buckets.lock();
        let bucket = buckets
            .entry(scope_key.to_owned())
            .or_insert_with(|| Bucket::new(capacity, refill));
        // A config change may alter capacity/refill; keep the bucket aligned.
        bucket.capacity = capacity as f64;
        bucket.refill_per_second = refill;

        let allowed = bucket.try_acquire(cost);
        let remaining = bucket.tokens.floor().max(0.0) as u64;
        let retry_after_secs = if allowed {
            0
        } else {
            bucket.retry_after(cost).max(1)
        };
        let secs_to_full = secs_to_full(remaining, capacity, refill);
        RateDecision {
            allowed,
            limit: capacity,
            remaining,
            reset_epoch_secs: current_epoch_secs().saturating_add(secs_to_full),
            retry_after_secs,
        }
    }

    /// Drops all buckets (used by tests).
    pub fn clear(&self) {
        self.buckets.lock().clear();
    }
}

/// Unix seconds now.
#[must_use]
pub fn current_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Builds the scope key for a rate-limit configuration.
///
/// Keys are `config-id:scope-kind:scope-value`; the config id keeps buckets
/// from different upstreams/routes from colliding.
#[must_use]
pub fn scope_key(config_id: &str, config: &RateLimitConfig, parts: &[&str]) -> String {
    let mut key = format!("{}:{:?}", config_id, config.scope);
    for part in parts {
        key.push(':');
        key.push_str(part);
    }
    key
}

/// Turns a rejected decision into a 429 error with retry guidance.
#[must_use]
pub fn rejection_error(decision: &RateDecision) -> OagwError {
    OagwError::RateLimitExceeded {
        retry_after_secs: decision.retry_after_secs.max(1),
        limit: decision.limit,
        remaining: decision.remaining,
        reset_epoch_secs: decision.reset_epoch_secs,
    }
}

/// Applies `min(ancestor, descendant)` to a chain of rate-limit configs
/// (nearest ancestor first), honouring `private` sharing.
///
/// `chain[0]` is the farthest ancestor and the last element is the tenant or
/// upstream being evaluated; that last config is always the base — its own
/// `sharing` never hides it from itself.
#[must_use]
pub fn effective_rate_limit(
    chain: &[Option<crate::domain::model::RateLimitConfig>],
) -> Option<crate::domain::model::RateLimitConfig> {
    let nearest_last = chain.iter().rev().filter_map(Option::as_ref).collect::<Vec<_>>();
    let mut configs = nearest_last.into_iter();
    let mut merged = configs.next()?.clone();
    for ancestor in configs {
        if let Some(next) = crate::domain::merge::merge_rate_limit(Some(ancestor), Some(&merged)) {
            merged = next;
        }
    }
    Some(merged)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;
    use super::*;
    use crate::domain::model::{BurstCapacity, RateScope, SustainedRate, RateWindow};

    fn config(rate: u64, capacity: u64) -> RateLimitConfig {
        RateLimitConfig {
            sustained: SustainedRate {
                rate,
                window: RateWindow::Second,
            },
            burst: Some(BurstCapacity { capacity }),
            scope: RateScope::Tenant,
            ..RateLimitConfig::default()
        }
    }

    #[test]
    fn allows_burst_up_to_capacity() {
        let limiter = RateLimiter::new();
        let cfg = config(1, 3);
        for _ in 0..3 {
            assert!(limiter.try_acquire(&cfg, "t1", 1).allowed);
        }
        let fourth = limiter.try_acquire(&cfg, "t1", 1);
        assert!(!fourth.allowed);
        assert_eq!(fourth.limit, 3);
        assert!(fourth.retry_after_secs >= 1);
    }

    #[test]
    fn scopes_are_isolated() {
        let limiter = RateLimiter::new();
        let cfg = config(1, 1);
        assert!(limiter.try_acquire(&cfg, "tenant-a", 1).allowed);
        assert!(limiter.try_acquire(&cfg, "tenant-b", 1).allowed);
        assert!(!limiter.try_acquire(&cfg, "tenant-a", 1).allowed);
    }

    #[test]
    fn tokens_replenish_over_time() {
        let limiter = RateLimiter::new();
        let cfg = config(10, 1);
        assert!(limiter.try_acquire(&cfg, "k", 1).allowed);
        std::thread::sleep(Duration::from_millis(150));
        assert!(limiter.try_acquire(&cfg, "k", 1).allowed);
    }

    #[test]
    fn cost_is_weighted() {
        let limiter = RateLimiter::new();
        let mut cfg = config(1, 5);
        cfg.cost = 3;
        assert!(limiter.try_acquire(&cfg, "k", 3).allowed);
        assert!(!limiter.try_acquire(&cfg, "k", 3).allowed);
    }

    #[test]
    fn hierarchical_min_wins() {
        let ancestor = config(10_000, 1_000);
        let descendant = config(100, 50);
        let effective = effective_rate_limit(&[Some(ancestor), Some(descendant)]).unwrap();
        assert_eq!(effective.sustained.rate, 100);
        assert_eq!(effective.capacity(), 50);
    }
}
