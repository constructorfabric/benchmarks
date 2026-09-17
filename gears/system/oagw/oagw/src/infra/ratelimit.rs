//! In-process token-bucket rate limiting.
//!
//! One bucket per `(rate-limit config, scope key)` tuple, with
//! continuous (time-proportional) refill. The effective rate for a
//! request is computed by the proxy (DESIGN: `min(selected, route,
//! ancestors)`); this module only decides admit/reject for a single
//! merged configuration.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use dashmap::DashMap;

use crate::domain::model::RateLimit;

/// The hard payload floor for a single bucket (avoids division by zero
/// and degenerate zero-rate configs).
const MIN_TPS: f64 = 1e-9;

/// Sharded token buckets keyed by `(scope_key, config_hash)`.
pub struct RateLimiter {
    buckets: DashMap<u64, TokenBucket>,
}

/// A single token bucket with continuous refill.
struct TokenBucket {
    tokens: f64,
    capacity: f64,
    tps: f64,
    last_millis: u64,
}

impl TokenBucket {
    fn new(rate: &RateLimit, now_millis: u64) -> Self {
        Self {
            tokens: rate.capacity() as f64,
            capacity: rate.capacity() as f64,
            tps: rate.tps().max(MIN_TPS),
            last_millis: now_millis,
        }
    }

    fn refill(&mut self, now_millis: u64) {
        if now_millis <= self.last_millis {
            return;
        }
        let elapsed_secs = (now_millis - self.last_millis) as f64 / 1000.0;
        self.tokens = (self.tokens + self.tps * elapsed_secs).min(self.capacity);
        self.last_millis = now_millis;
    }
}

impl RateLimiter {
    /// Create a new, empty limiter.
    #[must_use]
    pub fn new() -> Self {
        Self {
            buckets: DashMap::new(),
        }
    }

    /// Attempt to admit one request (cost from `rate.cost`).
    ///
    /// `scope_key` names the counter's identity dimension (tenant,
    /// user, IP, … — see DESIGN `rate_limit.scope`); `config_hash`
    /// distinguishes different rate-limit configurations sharing the
    /// same scope value. On rejection returns the `Retry-After`
    /// duration in whole seconds (≥ 1).
    pub fn check_and_take(
        &self,
        scope_key: &str,
        rate: &RateLimit,
        now_millis: u64,
    ) -> Result<(), u64> {
        let key = hash_pair(scope_key, rate);
        let mut bucket = self
            .buckets
            .entry(key)
            .or_insert_with(|| TokenBucket::new(rate, now_millis));
        bucket.refill(now_millis);
        let cost = rate.cost.max(1) as f64;
        if bucket.tokens >= cost {
            bucket.tokens -= cost;
            Ok(())
        } else {
            let deficit = cost - bucket.tokens;
            let retry_secs = (deficit / bucket.tps).ceil().max(1.0) as u64;
            Err(retry_secs)
        }
    }

    /// Deterministic counter key for a (scope, config) pair.
    ///
    /// # Panics
    /// Never — hashing cannot fail.
    #[must_use]
    pub fn bucket_key(scope_key: &str, rate: &RateLimit) -> u64 {
        hash_pair(scope_key, rate)
    }
}

fn hash_pair(scope_key: &str, rate: &RateLimit) -> u64 {
    let mut hasher = DefaultHasher::new();
    scope_key.hash(&mut hasher);
    rate.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rate(capacity: u64, tps: f64) -> RateLimit {
        RateLimit {
            sharing: crate::domain::model::SharingMode::Private,
            algorithm: crate::domain::model::Algorithm::TokenBucket,
            sustained: crate::domain::model::SustainedRate {
                rate: tps as u64,
                window: crate::domain::model::RateWindow::Second,
            },
            burst: Some(crate::domain::model::BurstCapacity { capacity }),
            scope: crate::domain::model::RateScope::Tenant,
            strategy: crate::domain::model::RateStrategy::Reject,
            cost: 1,
        }
    }

    #[test]
    fn burst_capacity_absorbs_initial_burst() {
        let limiter = RateLimiter::new();
        // capacity 3 — three immediate admits.
        for _ in 0..3 {
            assert!(limiter.check_and_take("ten:1", &rate(3, 1.0), 1_000).is_ok());
        }
        assert!(limiter.check_and_take("ten:1", &rate(3, 1.0), 1_000).is_err());
    }

    #[test]
    fn rejection_reports_retry_after() {
        let limiter = RateLimiter::new();
        for _ in 0..2 {
            limiter.check_and_take("ten:1", &rate(2, 0.5), 1_000).unwrap();
        }
        let retry = limiter.check_and_take("ten:1", &rate(2, 0.5), 1_000);
        let secs = retry.expect_err("bucket should be dry");
        assert!(secs >= 1);
    }

    #[test]
    fn bucket_recovers_after_refill() {
        let limiter = RateLimiter::new();
        limiter.check_and_take("ten:1", &rate(1, 1.0), 1_000).unwrap();
        assert!(limiter.check_and_take("ten:1", &rate(1, 1.0), 1_000).is_err());
        // One second later the bucket has a full token again.
        assert!(limiter.check_and_take("ten:1", &rate(1, 1.0), 2_000).is_ok());
    }

    #[test]
    fn scope_keys_are_independent() {
        let limiter = RateLimiter::new();
        limiter.check_and_take("ten:a", &rate(1, 1.0), 1_000).unwrap();
        // Same config, different scope → fresh bucket.
        assert!(limiter.check_and_take("ten:b", &rate(1, 1.0), 1_000).is_ok());
    }
}
