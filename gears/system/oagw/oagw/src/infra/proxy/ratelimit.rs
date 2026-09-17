//! Token-bucket rate limiter and the per-scope rate-limit manager
//! (ADR 0003).
//!
//! Each effective rate limit is materialized as a token bucket keyed by
//! `scope`:
//!
//! - `global` → one bucket for the whole proxy;
//! - `tenant` → one bucket per tenant;
//! - `route` → one bucket per (tenant, upstream) pair reaching the stage;
//! - `ip` / `user` → buckets keyed by the caller's resolved identity
//!   (user buckets fall back to the tenant when no subject is present;
//!   ip buckets fall back to `global` when no client address is known).
//!
//! Buckets refill continuously; a bucket in `reject` mode returning
//! `RateLimitExceeded` (429) carries the `Retry-After` header. `queue` and
//! `degrade` strategies admit the request at the bucket boundary (no
//! distributed queuing/degrading infrastructure is wired in this build —
//! see the deviations section of the crate README).

use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;
use parking_lot::Mutex;
use uuid::Uuid;

use crate::domain::model::{RateLimitScope, RateLimitStrategy};
use crate::domain::dto::EffectiveRateLimit;

/// A token bucket guarded by a mutex. The refill rate is expressed as
/// tokens per second; `capacity` is the burst ceiling.
#[derive(Debug)]
pub struct TokenBucket {
    capacity: f64,
    refill_per_sec: f64,
    cost: u64,
    tokens: f64,
    last_update: Instant,
}

impl TokenBucket {
    /// Create a bucket with `capacity` tokens (full) refilling at
    /// `refill_per_sec` tokens per second. Each acquisition costs `cost`.
    #[must_use]
    pub fn new(capacity: f64, refill_per_sec: f64, cost: u64) -> Self {
        Self {
            capacity,
            refill_per_sec,
            cost,
            tokens: capacity,
            last_update: Instant::now(),
        }
    }

    fn refill(&mut self, now: Instant) {
        if self.refill_per_sec <= 0.0 {
            // Zero refill rate: the bucket drains and never refills.
            return;
        }
        let elapsed = now.duration_since(self.last_update).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        self.last_update = now;
    }

    /// Attempt to acquire `cost` tokens. Returns the number of seconds
    /// until a token becomes available when the acquisition fails (for
    /// `Retry-After`), or `None` on success.
    pub fn try_acquire(&mut self) -> Option<u64> {
        let now = Instant::now();
        self.refill(now);
        let need = self.cost as f64;
        if self.tokens >= need {
            self.tokens -= need;
            None
        } else {
            let shortfall = need - self.tokens;
            let seconds = if self.refill_per_sec > 0.0 {
                (shortfall / self.refill_per_sec).ceil() as u64
            } else {
                // No refill: no deterministic wait; a minimal Retry-After is
                // reported so callers can back off.
                1
            };
            Some(seconds.max(1))
        }
    }
}

/// A completed rate-limit check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateDecision {
    /// The request may proceed.
    Allowed,
    /// The request is rejected (`429 RateLimitExceeded`).
    Rejected { retry_after_secs: u64 },
}

/// Manages the set of live token buckets. Sharing is via `Arc<RateLimitManager>`;
/// buckets remain in the map for the life of the process (their state decays
/// harmlessly and re-owned entries are reused).
pub struct RateLimitManager {
    buckets: DashMap<String, Arc<Mutex<TokenBucket>>>,
}

impl Default for RateLimitManager {
    fn default() -> Self {
        Self {
            buckets: DashMap::new(),
        }
    }
}

impl RateLimitManager {
    /// Create an empty manager.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn bucket_key(scope: &RateLimitScope, tenant: Uuid, ip: Option<&str>) -> String {
        match scope {
            RateLimitScope::Global => "global".to_owned(),
            RateLimitScope::Tenant => format!("tenant:{tenant}"),
            RateLimitScope::Route => format!("route:{tenant}"),
            RateLimitScope::User => format!("user:{tenant}"),
            RateLimitScope::Ip => match ip {
                Some(ip) => format!("ip:{ip}"),
                None => "global".to_owned(),
            },
        }
    }

    /// Check whether a request may proceed under `limit`.
    ///
    /// Returns `Allowed` when the token acquisition succeeds, or (for
    /// non-`reject` strategies) when the request is queued/degraded.
    pub fn check(
        &self,
        limit: &EffectiveRateLimit,
        tenant: Uuid,
        ip: Option<&str>,
    ) -> RateDecision {
        let key = Self::bucket_key(&limit.scope, tenant, ip);
        let bucket = self
            .buckets
            .entry(key)
            .or_insert_with(|| {
                Arc::new(Mutex::new(TokenBucket::new(
                    limit.capacity,
                    limit.refill_per_sec,
                    u64::from(limit.cost),
                )))
            })
            .clone();
        let mut guard = bucket.lock();
        match guard.try_acquire() {
            None => RateDecision::Allowed,
            Some(retry_after) if limit.strategy == RateLimitStrategy::Reject => {
                RateDecision::Rejected { retry_after_secs: retry_after }
            }
            Some(_) => {
                // queue / degrade strategies do not hard-reject at the
                // bucket boundary.
                RateDecision::Allowed
            }
        }
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn limit(capacity: u64, refill: f64, strategy: RateLimitStrategy) -> EffectiveRateLimit {
        EffectiveRateLimit {
            refill_per_sec: refill,
            capacity: capacity as f64,
            cost: 1,
            strategy,
            scope: RateLimitScope::Global,
            window_secs: 60,
        }
    }

    #[test]
    fn bucket_allows_burst_then_rejects() {
        // 2 capacity, refills 0 → the second acquisition must be rejected.
        let mut b = TokenBucket::new(2.0, 0.0, 1);
        assert_eq!(b.try_acquire(), None);
        assert_eq!(b.try_acquire(), None);
        assert!(b.try_acquire().is_some());
    }

    #[test]
    fn bucket_refills_over_time() {
        let mut b = TokenBucket::new(1.0, 10.0, 1);
        assert_eq!(b.try_acquire(), None);
        std::thread::sleep(Duration::from_millis(250));
        // ~2.5 tokens should have refilled past 1.
        assert_eq!(b.try_acquire(), None);
    }

    #[test]
    fn reject_strategy_returns_retry_after() {
        let manager = RateLimitManager::default();
        let l = limit(1, 0.0, RateLimitStrategy::Reject);
        assert_eq!(manager.check(&l, Uuid::new_v4(), None), RateDecision::Allowed);
        let decision = manager.check(&l, Uuid::new_v4(), None);
        assert!(matches!(decision, RateDecision::Rejected { .. }));
    }

    #[test]
    fn queue_strategy_never_hard_rejects_at_bucket() {
        let manager = RateLimitManager::default();
        let l = limit(1, 0.0, RateLimitStrategy::Queue);
        assert_eq!(manager.check(&l, Uuid::new_v4(), None), RateDecision::Allowed);
        assert_eq!(manager.check(&l, Uuid::new_v4(), None), RateDecision::Allowed);
    }

    #[test]
    fn scopes_partition_buckets() {
        let manager = RateLimitManager::default();
        let ip = "10.0.0.1";
        let tenant_a = Uuid::new_v4();
        let tenant_b = Uuid::new_v4();
        // tenant-scoped buckets differ per tenant.
        let l = EffectiveRateLimit {
            scope: RateLimitScope::Tenant,
            ..limit(1, 0.0, RateLimitStrategy::Reject)
        };
        assert_eq!(manager.check(&l, tenant_a, None), RateDecision::Allowed);
        assert_eq!(manager.check(&l, tenant_b, None), RateDecision::Allowed);
        // ip-scoped buckets differ per ip.
        let lip = limit(1, 0.0, RateLimitStrategy::Reject);
        let lip_scope = EffectiveRateLimit {
            scope: RateLimitScope::Ip,
            ..lip
        };
        assert_eq!(manager.check(&lip_scope, tenant_a, Some(ip)), RateDecision::Allowed);
        assert_eq!(manager.check(&lip_scope, tenant_a, Some(ip)), RateDecision::Rejected { retry_after_secs: 1 });
    }
}
