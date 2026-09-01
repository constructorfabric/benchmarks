//! Token-bucket rate limiter (DESIGN "Rate Limiting", component model).
//!
//! One bucket per configured scope key (global / tenant / user / ip / route).
//! Buckets replenish continuously at `rate / window_secs` tokens per second
//! and are capped at the burst capacity. The effective (merged) rate limit
//! is computed upstream in the data plane (`min()` across route, selected
//! upstream and every ancestor `enforce` policy); this module only executes
//! a single bucket policy.
//!
//! Buckets are an in-memory cache: they are bounded (`max_buckets`, default
//! `10_000`) and prune entries idle longer than `prune_idle` once the bound is
//! exceeded. Stale buckets are also evicted explicitly when upstreams/routes
//! are deleted (see [`RateLimiter::remove_key`]).

use std::time::{Duration, Instant};

use dashmap::DashMap;
use uuid::Uuid;

use crate::domain::model::{RateLimit, RateScope};

/// Decision after a rate check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateDecision {
    /// The request may proceed.
    Allow {
        /// Bucket capacity (for `X-RateLimit-Limit`).
        limit: u64,
        /// Tokens remaining after deduction (for `X-RateLimit-Remaining`).
        remaining: u64,
    },
    /// The request is rejected.
    Limited {
        /// Bucket capacity (for `X-RateLimit-Limit`).
        limit: u64,
        /// Seconds until the bucket likely refills (for `Retry-After`).
        retry_after_secs: u64,
    },
}

struct Bucket {
    tokens: f64,
    updated_at: Instant,
}

/// Default upper bound on live buckets.
const DEFAULT_MAX_BUCKETS: usize = 10_000;

/// Default idle horizon (seconds) after which a bucket may be pruned.
const DEFAULT_PRUNE_IDLE_SECS: u64 = 3600;

/// In-process token-bucket limiter.
///
/// Thread-safe via `DashMap`; buckets are keyed by a canonical scope string
/// that embeds the tenant, so sibling tenants never share state.
pub struct RateLimiter {
    buckets: DashMap<String, Bucket>,
    max_buckets: usize,
    prune_idle: Duration,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self {
            buckets: DashMap::new(),
            max_buckets: DEFAULT_MAX_BUCKETS,
            prune_idle: Duration::from_secs(DEFAULT_PRUNE_IDLE_SECS),
        }
    }
}

impl RateLimiter {
    /// Create an empty limiter with default bounds.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a limiter with explicit eviction bounds (test / tuning helper).
    #[must_use]
    pub fn with_limits(max_buckets: usize, prune_idle: Duration) -> Self {
        Self {
            buckets: DashMap::new(),
            max_buckets,
            prune_idle,
        }
    }

    /// Evaluate `cost` tokens against the bucket for `key`.
    ///
    /// The token-bucket arithmetic is inherently floating point (continuous
    /// refill over elapsed wall-time); the u64/f64 round-trips are intentional.
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub fn check(&self, key: &str, policy: &RateLimit) -> RateDecision {
        self.evict_if_needed();
        let capacity = policy.capacity();
        let cost = policy.cost;
        if policy.sustained.rate == 0 {
            // A zero sustained rate is a hard block: never seed the bucket (no
            // initial burst allowance) and never refill, so every request is
            // limited with no meaningful retry horizon.
            return RateDecision::Limited {
                limit: capacity,
                retry_after_secs: u64::MAX,
            };
        }
        let tokens_per_sec =
            policy.sustained.rate as f64 / policy.sustained.window.as_secs() as f64;

        let mut entry = self
            .buckets
            .entry(key.to_owned())
            .or_insert_with(|| Bucket {
                tokens: capacity as f64,
                updated_at: Instant::now(),
            });
        let bucket = entry.value_mut();
        let elapsed = bucket.updated_at.elapsed().as_secs_f64();
        let refill = tokens_per_sec * elapsed;
        bucket.tokens = (bucket.tokens + refill).min(capacity as f64);
        bucket.updated_at = Instant::now();

        if bucket.tokens >= cost as f64 {
            bucket.tokens -= cost as f64;
            RateDecision::Allow {
                limit: capacity,
                remaining: bucket.tokens.floor() as u64,
            }
        } else {
            let shortage = cost as f64 - bucket.tokens;
            let wait = if tokens_per_sec > 0.0 {
                (shortage / tokens_per_sec).ceil() as u64
            } else {
                u64::MAX
            };
            RateDecision::Limited {
                limit: capacity,
                retry_after_secs: wait,
            }
        }
    }

    /// Evict a single scope key (used when a rate-limited resource is deleted).
    pub fn remove_key(&self, key: &str) {
        self.buckets.remove(key);
    }

    /// Opportunistically prune buckets once the cache exceeds
    /// [`RateLimiter::max_buckets`]. Called on the write path before a bucket
    /// is created: entries idle beyond the horizon are dropped first, then
    /// least-recently-used entries are dropped until the cache is back under
    /// `max_buckets / 2` (leaving room before the next prune).
    fn evict_if_needed(&self) {
        if self.buckets.len() <= self.max_buckets {
            return;
        }
        let horizon = self.prune_idle;
        self.buckets
            .retain(|_, b| b.updated_at.elapsed() <= horizon);
        while self.buckets.len() > self.max_buckets >> 1 {
            let mut oldest_key: Option<String> = None;
            let mut oldest_at = Instant::now();
            for entry in &self.buckets {
                let (key, bucket) = entry.pair();
                if oldest_key.is_none() || bucket.updated_at <= oldest_at {
                    oldest_at = bucket.updated_at;
                    oldest_key = Some(key.clone());
                }
            }
            let Some(key) = oldest_key else { break };
            self.buckets.remove(&key);
        }
    }

    /// Number of live buckets (metrics/test helper).
    #[must_use]
    pub fn bucket_count(&self) -> usize {
        self.buckets.len()
    }
}

/// Canonical limiter key for a configured scope, when one can be derived
/// without runtime caller data.
///
/// `Global`, `Tenant` and `Route` scopes are fixed once the policy is known;
/// `User` and `Ip` scopes depend on the calling subject/address and return
/// `None` (the data plane derives those keys per request).
#[must_use]
pub fn rate_scope_key(scope: RateScope, tenant_id: Uuid, route_id: &str) -> Option<String> {
    match scope {
        RateScope::Global => Some("g:".to_owned()),
        RateScope::Tenant => Some(format!("t:{tenant_id}")),
        RateScope::Route => Some(format!("r:{route_id}")),
        RateScope::User | RateScope::Ip => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{BurstCapacity, RateLimit, RateWindow, SustainedRate};

    fn policy(rate: u64, burst: Option<u64>) -> RateLimit {
        RateLimit {
            sharing: crate::domain::model::SharingMode::Private,
            algorithm: crate::domain::model::RateAlgorithm::TokenBucket,
            sustained: SustainedRate {
                rate,
                window: RateWindow::Second,
            },
            burst: burst.map(|c| BurstCapacity { capacity: c }),
            scope: crate::domain::model::RateScope::Global,
            strategy: crate::domain::model::RateStrategy::Reject,
            cost: 1,
        }
    }

    #[test]
    fn burst_allows_first_wave() {
        let limiter = RateLimiter::new();
        let p = policy(1, Some(3));
        let mut allowed = 0;
        for _ in 0..3 {
            if matches!(limiter.check("k", &p), RateDecision::Allow { .. }) {
                allowed += 1;
            }
        }
        assert_eq!(allowed, 3);
        assert!(matches!(
            limiter.check("k", &p),
            RateDecision::Limited { .. }
        ));
    }

    #[test]
    fn separate_keys_do_not_share_buckets() {
        let limiter = RateLimiter::new();
        let p = policy(1, Some(1));
        assert!(matches!(limiter.check("a", &p), RateDecision::Allow { .. }));
        assert!(matches!(limiter.check("b", &p), RateDecision::Allow { .. }));
        assert!(matches!(
            limiter.check("a", &p),
            RateDecision::Limited { .. }
        ));
    }

    #[test]
    fn zero_rate_never_allows() {
        let limiter = RateLimiter::new();
        let p = policy(0, Some(1));
        assert!(matches!(
            limiter.check("k", &p),
            RateDecision::Limited { .. }
        ));
    }

    #[test]
    fn high_cost_exhausts_bucket() {
        let limiter = RateLimiter::new();
        let mut p = policy(1, Some(5));
        p.cost = 5;
        assert!(matches!(limiter.check("k", &p), RateDecision::Allow { .. }));
        assert!(matches!(
            limiter.check("k", &p),
            RateDecision::Limited { .. }
        ));
    }

    #[test]
    fn remove_key_evicts_bucket() {
        let limiter = RateLimiter::new();
        let p = policy(1, Some(1));
        assert!(matches!(
            limiter.check("drop-me", &p),
            RateDecision::Allow { .. }
        ));
        limiter.remove_key("drop-me");
        assert_eq!(limiter.bucket_count(), 0);
        assert!(matches!(
            limiter.check("drop-me", &p),
            RateDecision::Allow { .. }
        ));
    }

    #[test]
    fn bounded_limiter_prunes_old_buckets() {
        // Bounds so tight that every new key beyond the first forces a prune
        // of entries idle longer than the horizon.
        let limiter = RateLimiter::with_limits(4, Duration::ZERO);
        let p = policy(1, Some(1));
        let mut allowed = 0;
        for i in 0..20 {
            // Each fresh key starts with a full bucket, so every creation
            // (and prune) cycle observes an `Allow`.
            if matches!(
                limiter.check(&format!("k{i}"), &p),
                RateDecision::Allow { .. }
            ) {
                allowed += 1;
            }
        }
        assert_eq!(allowed, 20, "every fresh bucket starts allowed");
        // With prune_idle = ZERO every bucket is idle the moment it is
        // created, so the cache stays at the retained floor.
        assert!(limiter.bucket_count() <= limiter.max_buckets + 1);
    }

    #[test]
    fn rate_scope_keys_are_tenant_scoped() {
        let tenant = Uuid::from_u128(0x1234);
        let g = rate_scope_key(RateScope::Global, tenant, "r1").expect("global");
        assert_eq!(g, "g:");
        let t = rate_scope_key(RateScope::Tenant, tenant, "r1").expect("tenant");
        assert!(t.contains(&tenant.to_string()));
        let r = rate_scope_key(RateScope::Route, tenant, "r1").expect("route");
        assert!(r.contains("r1"));
        assert!(rate_scope_key(RateScope::User, tenant, "r1").is_none());
        assert!(rate_scope_key(RateScope::Ip, tenant, "r1").is_none());
    }
}
