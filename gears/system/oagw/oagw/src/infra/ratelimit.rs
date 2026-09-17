//! Local token-bucket rate limiting (ADR 0003).
//!
//! Each keyed bucket is a token bucket: `rate` tokens refill every
//! `window`; the bucket holds at most `capacity` (burst) tokens; each
//! request consumes `cost` tokens. Bucket state lives in [`DashMap`]
//! keyed by a hash of `(alias, counter-scope)` so per-tenant and global
//! counters coexist. Denials report `Retry-After` in seconds.
//!
//! Uses `tokio::time::Instant` as the clock so refill behavior is unit
//! testable with tokio's paused clock; under a real runtime it behaves
//! identically to `std::time::Instant`.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::Duration;

use dashmap::DashMap;
use tokio::time::Instant;
use uuid::Uuid;

use crate::domain::models::{RateLimitScope, ResolvedRateLimit};
use crate::domain::ratelimit::RateLimitDecision;

/// A single token-bucket state.
#[derive(Debug, Clone)]
struct Bucket {
    tokens: u64,
    last_refill: Instant,
}

/// Rate-limiter over a bounded set of in-memory buckets.
///
/// The bucket count is bounded by `max_buckets`: when a new key would
/// exceed the cap, the least-recently-refilled bucket is evicted.
#[derive(Default)]
pub struct RateLimiter {
    buckets: DashMap<u64, Bucket>,
    /// Max live buckets; beyond this, the LRU bucket is evicted.
    max_buckets: usize,
}

impl RateLimiter {
    /// Build a limiter that keeps at most `max_buckets` buckets.
    #[must_use]
    pub fn with_capacity(max_buckets: usize) -> Self {
        Self {
            buckets: DashMap::new(),
            max_buckets: if max_buckets == 0 { 10_000 } else { max_buckets },
        }
    }

    /// Hash the `(alias, tenant, scope)` tuple into a bucket key.
    #[must_use]
    pub fn key(alias: &str, tenant_id: Uuid, scope: RateLimitScope) -> u64 {
        let mut hasher = DefaultHasher::new();
        match scope {
            RateLimitScope::Global => ("global", alias).hash(&mut hasher),
            RateLimitScope::Tenant
            | RateLimitScope::User
            | RateLimitScope::Ip
            | RateLimitScope::Route => {
                ("tenant", tenant_id, alias).hash(&mut hasher);
            }
        }
        hasher.finish()
    }

    /// Apply `limit` to the request identified by `key`.
    ///
    /// Returns `Allow{remaining}` with the tokens left in the bucket, or
    /// `Deny{retry_after_seconds}` when fewer than `cost` tokens remain.
    /// `retry_after` is the (ceil-rounded) time until `cost` tokens
    /// refill.
    #[must_use]
    pub fn check(&self, key: u64, limit: &ResolvedRateLimit) -> RateLimitDecision {
        // Fixed-point math: bucket tokens are stored scaled by `window`
        // (`tokens * window`), so refill, burst, and cost all stay integer.
        // `rate` tokens refill per full `window`; that is `rate` scaled
        // units per elapsed second.
        let window = limit.window_secs.max(1);
        let rate = u64::from(limit.rate.max(1));
        let capacity_scaled = u64::from(limit.capacity.max(1)).saturating_mul(window);
        let cost_scaled = u64::from(limit.cost.max(1)).saturating_mul(window);
        let now = Instant::now();

        // Evict the LRU bucket before inserting a new key beyond the cap.
        if !self.buckets.contains_key(&key) && self.buckets.len() >= self.max_buckets {
            let oldest = self
                .buckets
                .iter()
                .min_by_key(|e| e.last_refill)
                .map(|e| *e.key());
            if let Some(oldest) = oldest {
                self.buckets.remove(&oldest);
            }
        }

        let mut entry = self
            .buckets
            .entry(key)
            .or_insert_with(|| Bucket {
                tokens: capacity_scaled,
                last_refill: now,
            });
        let bucket = entry.value_mut();
        let elapsed = now.saturating_duration_since(bucket.last_refill).as_secs();
        bucket.tokens = (bucket.tokens + elapsed.saturating_mul(rate)).min(capacity_scaled);
        bucket.last_refill = now;

        if bucket.tokens >= cost_scaled {
            bucket.tokens -= cost_scaled;
            return RateLimitDecision::Allow {
                remaining: bucket.tokens.div_euclid(window),
            };
        }
        let need = cost_scaled - bucket.tokens;
        let seconds = need.div_ceil(rate);
        RateLimitDecision::Deny {
            retry_after_seconds: seconds.max(1),
        }
    }

    /// Expire buckets with no activity for `idle` — called periodically
    /// to bound memory.
    pub fn prune_idle(&self, idle: Duration) {
        if let Some(cutoff) = Instant::now().checked_sub(idle) {
            self.buckets.retain(|_, b| b.last_refill >= cutoff);
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use std::time::Duration;

    fn limit(rate: u32, window_secs: u64, capacity: u32, cost: u32) -> ResolvedRateLimit {
        ResolvedRateLimit {
            rate,
            window_secs,
            capacity,
            cost,
        }
    }

    #[tokio::test]
    async fn burst_capacity_allows_burst_then_denies() {
        tokio::time::pause();
        let limiter = RateLimiter::with_capacity(100);
        let key = 1;
        let rl = limit(2, 1, 3, 1); // 2/s refill, burst 3
        for _ in 0..3 {
            assert!(matches!(
                limiter.check(key, &rl),
                RateLimitDecision::Allow { .. }
            ));
        }
        // Fourth exceeds the burst capacity → denied with retry_after >= 1.
        let decision = limiter.check(key, &rl);
        assert!(matches!(
            decision,
            RateLimitDecision::Deny { retry_after_seconds: n } if n >= 1
        ));
    }

    #[tokio::test]
    async fn refill_restores_tokens_over_time() {
        tokio::time::pause();
        let limiter = RateLimiter::with_capacity(100);
        let key = 2;
        let rl = limit(10, 1, 10, 5); // 10/s, burst 10, cost 5 → 2 admissions
        assert!(matches!(limiter.check(key, &rl), RateLimitDecision::Allow { .. }));
        assert!(matches!(limiter.check(key, &rl), RateLimitDecision::Allow { .. }));
        assert!(matches!(limiter.check(key, &rl), RateLimitDecision::Deny { .. }));

        tokio::time::advance(Duration::from_secs(1)).await;
        // Refilled to full burst → admitted again with remaining >= 5.
        assert!(matches!(
            limiter.check(key, &rl),
            RateLimitDecision::Allow { remaining } if remaining >= 5
        ));
    }

    #[tokio::test]
    async fn distinct_keys_are_independent() {
        let limiter = RateLimiter::with_capacity(100);
        let rl = limit(1, 1, 1, 1);
        assert!(matches!(
            limiter.check(11, &rl),
            RateLimitDecision::Allow { .. }
        ));
        assert!(matches!(limiter.check(12, &rl), RateLimitDecision::Allow { .. }));
    }

    #[test]
    fn key_namespacing_separates_global_and_tenant() {
        let tid = Uuid::new_v4();
        assert_ne!(
            RateLimiter::key("a", tid, RateLimitScope::Global),
            RateLimiter::key("a", tid, RateLimitScope::Tenant)
        );
        assert_eq!(
            RateLimiter::key("a", tid, RateLimitScope::Tenant),
            RateLimiter::key("a", tid, RateLimitScope::Tenant)
        );
    }
}
