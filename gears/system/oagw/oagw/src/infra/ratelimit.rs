//! In-memory token-bucket rate limiting (ADR-0003 §"Token Bucket Algorithm").
//!
//! Each effective rate limit is materialized as a per(scope-key) token bucket
//! with `refill_rate` tokens/sec and `capacity` burst. Buckets are stored in a
//! DashMap keyed by `(scope, resource, scope_id)`; entries idle for longer
//! than their bucket window are evicted opportunistically so deleted upstreams
//! do not leak memory indefinitely (ADR-0003 Redis key layout analog).

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;
use parking_lot::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::domain::error::RateLimitHeaders;

/// Effective per-second refill + capacity describing one configured bucket.
#[derive(Debug, Clone, Copy)]
pub struct BucketParams {
    /// Tokens replenished per second.
    pub refill_per_sec: f64,
    /// Bucket capacity (burst).
    pub capacity: u64,
    /// Tokens consumed per request.
    pub cost: u64,
}

/// Outcome of an acquire attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcquireOutcome {
    /// Request allowed.
    Allowed,
    /// Request rejected due to insufficient tokens.
    Rejected,
}

/// A single token bucket.
struct TokenBucket {
    tokens: f64,
    last_refill: Instant,
    capacity: f64,
    refill_per_sec: f64,
}

impl TokenBucket {
    fn new(capacity: f64, refill_per_sec: f64) -> Self {
        Self {
            tokens: capacity,
            last_refill: Instant::now(),
            capacity,
            refill_per_sec,
        }
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        if elapsed > 0.0 {
            self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
            self.last_refill = now;
        }
    }

    /// Seconds until `cost` tokens are available (0 when available now).
    fn seconds_until(&self, cost: f64) -> f64 {
        let needed = cost - self.tokens;
        if needed <= 0.0 {
            0.0
        } else {
            needed / self.refill_per_sec.max(f64::EPSILON)
        }
    }
}

/// Shared rate limiter for the data plane.
#[derive(Default)]
pub struct RateLimiter {
    buckets: DashMap<u64, Arc<RwLock<TokenBucket>>>,
}

/// A rate-limit key desribing the counter scope for one acquire.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RateKey {
    /// Scope discriminator (`tenant`, `user`, `global`, `ip`, `route`).
    pub scope: &'static str,
    /// Resource being limited (`upstream:{id}` or `route:{id}`).
    pub resource: String,
    /// Identity within the scope.
    pub scope_id: String,
}

impl RateLimiter {
    /// Create a new limiter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Try to acquire `cost` tokens from the bucket identified by `key`.
    ///
    /// Returns the post-acquire rate-limit headers on success and on failure
    /// (rejection carries `retry_after_secs` and remaining = 0).
    pub fn acquire(
        &self,
        key: &RateKey,
        params: BucketParams,
    ) -> (AcquireOutcome, RateLimitHeaders) {
        let hash = hash_key(key);

        if self.buckets.len() >= 100_000 {
            self.evict_idle();
        }

        let entry = self.buckets.entry(hash).or_insert_with(|| {
            Arc::new(RwLock::new(TokenBucket::new(
                params.capacity as f64,
                params.refill_per_sec,
            )))
        });

        let bucket = entry.value().clone();
        let mut inner = bucket.write();

        inner.refill();
        let cost = params.cost as f64;
        let now_epoch = now_epoch_secs();
        if inner.tokens >= cost {
            inner.tokens -= cost;
            let remaining = inner.tokens.floor().max(0.0) as u64;
            let reset = now_epoch + inner.seconds_until(0.0).max(0.0).ceil() as u64;
            (
                AcquireOutcome::Allowed,
                RateLimitHeaders {
                    limit: params.capacity,
                    remaining,
                    reset_epoch_secs: reset,
                    retry_after_secs: 0,
                },
            )
        } else {
            let wait = inner.seconds_until(cost).ceil() as u64;
            (
                AcquireOutcome::Rejected,
                RateLimitHeaders {
                    limit: params.capacity,
                    remaining: 0,
                    reset_epoch_secs: now_epoch + wait,
                    retry_after_secs: wait,
                },
            )
        }
    }

    /// Evict buckets untouched for more than one day (bounded bookkeeping).
    fn evict_idle(&self) {
        let cutoff = Instant::now().checked_sub(std::time::Duration::from_secs(86_400));
        if let Some(cutoff) = cutoff {
            self.buckets.retain(|_, b| {
                let g = b.read();
                g.last_refill >= cutoff
            });
        }
    }
}

fn hash_key(key: &RateKey) -> u64 {
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    hasher.finish()
}

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn key(scope: &'static str) -> RateKey {
        RateKey {
            scope,
            resource: "upstream:test".to_owned(),
            scope_id: "tenant-1".to_owned(),
        }
    }

    #[test]
    fn burst_allowed_up_to_capacity() {
        let limiter = RateLimiter::new();
        let params = BucketParams {
            refill_per_sec: 1.0,
            capacity: 5,
            cost: 1,
        };
        for _ in 0..5 {
            let (outcome, _) = limiter.acquire(&key("tenant"), params);
            assert_eq!(outcome, AcquireOutcome::Allowed);
        }
        let (outcome, headers) = limiter.acquire(&key("tenant"), params);
        assert_eq!(outcome, AcquireOutcome::Rejected);
        assert!(headers.retry_after_secs >= 1);
    }

    #[test]
    fn cost_above_one_consumes_multiple_tokens() {
        let limiter = RateLimiter::new();
        let params = BucketParams {
            refill_per_sec: 10.0,
            capacity: 10,
            cost: 7,
        };
        let (a, _) = limiter.acquire(&key("tenant"), params);
        assert_eq!(a, AcquireOutcome::Allowed);
        let (b, _) = limiter.acquire(&key("tenant"), params);
        assert_eq!(b, AcquireOutcome::Rejected);
    }

    #[test]
    fn allowed_responses_carry_rate_limit_headers() {
        let limiter = RateLimiter::new();
        let params = BucketParams {
            refill_per_sec: 60.0,
            capacity: 4,
            cost: 1,
        };
        let now = now_epoch_secs();
        for expected_remaining in (0..4).rev() {
            let (outcome, headers) = limiter.acquire(&key("tenant"), params);
            assert_eq!(outcome, AcquireOutcome::Allowed);
            assert_eq!(headers.limit, 4, "x-ratelimit-limit = capacity");
            assert_eq!(
                headers.remaining, expected_remaining,
                "remaining decrements"
            );
            assert!(headers.reset_epoch_secs >= now, "reset is now-or-future");
            assert_eq!(
                headers.retry_after_secs, 0,
                "success carries no retry-after"
            );
        }
        let (outcome, headers) = limiter.acquire(&key("tenant"), params);
        assert_eq!(outcome, AcquireOutcome::Rejected);
        assert_eq!(headers.limit, 4);
        assert_eq!(headers.remaining, 0, "rejected leaves remaining at 0");
        assert!(
            headers.retry_after_secs >= 1,
            "retry-after is at least a second"
        );
        assert_eq!(headers.reset_epoch_secs, now + headers.retry_after_secs);
    }

    #[test]
    fn buckets_are_independent_per_scope_key() {
        let limiter = RateLimiter::new();
        let params = BucketParams {
            refill_per_sec: 1.0,
            capacity: 1,
            cost: 1,
        };
        let tenant_a = RateKey {
            scope: "tenant",
            resource: "upstream:t1".to_owned(),
            scope_id: "tenant-1".to_owned(),
        };
        let tenant_b = RateKey {
            scope: "tenant",
            resource: "upstream:t1".to_owned(),
            scope_id: "tenant-2".to_owned(),
        };
        let (a, _) = limiter.acquire(&tenant_a, params);
        assert_eq!(a, AcquireOutcome::Allowed);
        // A different tenant still has its own full bucket.
        let (b, _) = limiter.acquire(&tenant_b, params);
        assert_eq!(
            b,
            AcquireOutcome::Allowed,
            "per-tenant buckets are independent"
        );
        // Re-using the exhausted key rejects.
        let (a2, _) = limiter.acquire(&tenant_a, params);
        assert_eq!(a2, AcquireOutcome::Rejected);

        // Route-scoped keys are separate from tenant-scoped ones.
        let route_key = RateKey {
            scope: "route",
            resource: "route:r1".to_owned(),
            scope_id: "route-1".to_owned(),
        };
        let (r, _) = limiter.acquire(&route_key, params);
        assert_eq!(
            r,
            AcquireOutcome::Allowed,
            "different scope => different bucket"
        );
    }

    #[test]
    fn tokens_refill_over_time() {
        let limiter = RateLimiter::new();
        let params = BucketParams {
            refill_per_sec: 100.0,
            capacity: 1,
            cost: 1,
        };
        let (a, _) = limiter.acquire(&key("tenant"), params);
        assert_eq!(a, AcquireOutcome::Allowed);
        let (b, _) = limiter.acquire(&key("tenant"), params);
        assert_eq!(b, AcquireOutcome::Rejected);

        // 30ms at 100 tokens/sec refills ~3 tokens; capacity 1 caps it at 1.
        std::thread::sleep(std::time::Duration::from_millis(30));
        let (c, headers) = limiter.acquire(&key("tenant"), params);
        assert_eq!(
            c,
            AcquireOutcome::Allowed,
            "bucket refills after the window"
        );
        assert_eq!(headers.remaining, 0, "refill deltas accumulate then drain");
    }

    #[test]
    fn rejection_retry_after_matches_seconds_until_available() {
        let limiter = RateLimiter::new();
        // 1 token/sec; consume the only token, then the next request must wait.
        let params = BucketParams {
            refill_per_sec: 1.0,
            capacity: 1,
            cost: 1,
        };
        let (a, _) = limiter.acquire(&key("tenant"), params);
        assert_eq!(a, AcquireOutcome::Allowed);
        let (b, headers) = limiter.acquire(&key("tenant"), params);
        assert_eq!(b, AcquireOutcome::Rejected);
        // With zero tokens left and 1/sec refill, ~1s until one token exists.
        assert!(
            (1..=2).contains(&headers.retry_after_secs),
            "retry-after ~1s, got {}",
            headers.retry_after_secs
        );
    }
}
