//! Per-instance token buckets (ADR 0003, ADR 0006).
//!
//! Rate limiting lives in the Data Plane because that is the only layer with
//! the full request context. State is per-instance for the MVP — distributed
//! coordination is deferred.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use parking_lot::Mutex;
use uuid::Uuid;

use crate::domain::model::{RateLimitAlgorithm, RateLimitConfig, RateLimitScope};

/// Outcome of one limiter check.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateLimitVerdict {
    pub allowed: bool,
    /// Configured sustained rate, for `X-RateLimit-Limit`.
    pub limit: u32,
    /// Whole tokens left in the bucket, for `X-RateLimit-Remaining`.
    pub remaining: u32,
    /// Seconds until enough tokens have replenished, for `Retry-After`.
    pub retry_after_secs: u64,
    /// Seconds until the bucket is full again, for `X-RateLimit-Reset`.
    pub reset_after_secs: u64,
    /// Fraction of the bucket consumed, in `[0.0, 1.0]`.
    pub usage_ratio: f64,
}

/// Classic token bucket: `tokens` replenish continuously up to `capacity`.
#[derive(Debug)]
struct TokenBucket {
    tokens: f64,
    capacity: f64,
    refill_per_second: f64,
    last_update: Instant,
}

impl TokenBucket {
    fn new(capacity: f64, refill_per_second: f64, now: Instant) -> Self {
        Self {
            tokens: capacity,
            capacity,
            refill_per_second,
            last_update: now,
        }
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last_update).as_secs_f64();
        if elapsed > 0.0 {
            self.tokens = (self.tokens + elapsed * self.refill_per_second).min(self.capacity);
            self.last_update = now;
        }
    }

    /// Re-shape the bucket when the effective configuration changed, keeping
    /// the consumed fraction so a config edit cannot be used to reset a
    /// counter.
    fn reconfigure(&mut self, capacity: f64, refill_per_second: f64) {
        if (self.capacity - capacity).abs() > f64::EPSILON {
            let consumed_ratio = 1.0 - (self.tokens / self.capacity).clamp(0.0, 1.0);
            self.capacity = capacity;
            self.tokens = capacity * (1.0 - consumed_ratio);
        }
        self.refill_per_second = refill_per_second;
    }

    fn try_acquire(&mut self, cost: f64, now: Instant) -> RateLimitVerdict {
        self.refill(now);
        let allowed = self.tokens >= cost;
        if allowed {
            self.tokens -= cost;
        }
        let deficit = (cost - self.tokens).max(0.0);
        let retry_after_secs = if allowed || self.refill_per_second <= 0.0 {
            0
        } else {
            (deficit / self.refill_per_second).ceil().max(1.0) as u64
        };
        let reset_after_secs = if self.refill_per_second <= 0.0 {
            0
        } else {
            (((self.capacity - self.tokens).max(0.0)) / self.refill_per_second).ceil() as u64
        };
        RateLimitVerdict {
            allowed,
            limit: 0,
            remaining: self.tokens.max(0.0).floor() as u32,
            retry_after_secs,
            reset_after_secs,
            usage_ratio: 1.0 - (self.tokens / self.capacity).clamp(0.0, 1.0),
        }
    }
}

/// Identity a counter is kept against.
#[derive(Debug, Clone, Copy)]
pub struct RateLimitSubject {
    pub tenant_id: Uuid,
    pub subject_id: Uuid,
    pub upstream_id: Uuid,
    pub route_id: Uuid,
}

/// All live buckets, keyed by `{resource}:{id}:{scope}:{scope_id}` so a
/// deleted resource's counters can be dropped by prefix.
#[derive(Default)]
pub struct RateLimiterRegistry {
    buckets: DashMap<String, Arc<Mutex<TokenBucket>>>,
}

impl std::fmt::Debug for RateLimiterRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RateLimiterRegistry")
            .field("buckets", &self.buckets.len())
            .finish()
    }
}

impl RateLimiterRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Build the counter key for `config` and `subject`.
    #[must_use]
    pub fn key_for(
        config: &RateLimitConfig,
        subject: &RateLimitSubject,
        client_ip: Option<&str>,
    ) -> String {
        let (scope_name, scope_id) = match config.scope {
            RateLimitScope::Global => ("global", "-".to_owned()),
            RateLimitScope::Tenant => ("tenant", subject.tenant_id.to_string()),
            RateLimitScope::User => ("user", subject.subject_id.to_string()),
            RateLimitScope::Ip => ("ip", client_ip.unwrap_or("unknown").to_owned()),
            RateLimitScope::Route => ("route", subject.route_id.to_string()),
        };
        format!(
            "upstream:{}:{scope_name}:{scope_id}",
            subject.upstream_id
        )
    }

    /// Check and consume `config.cost` tokens.
    #[must_use]
    pub fn check(
        &self,
        config: &RateLimitConfig,
        subject: &RateLimitSubject,
        client_ip: Option<&str>,
    ) -> RateLimitVerdict {
        self.check_at(config, subject, client_ip, Instant::now())
    }

    /// [`Self::check`] with an injectable clock, for tests.
    #[must_use]
    pub fn check_at(
        &self,
        config: &RateLimitConfig,
        subject: &RateLimitSubject,
        client_ip: Option<&str>,
        now: Instant,
    ) -> RateLimitVerdict {
        let key = Self::key_for(config, subject, client_ip);
        let refill = config.refill_per_second();
        // A sliding window must not permit a boundary burst, so the bucket is
        // sized to the sustained rate and `burst.capacity` is ignored.
        let capacity = match config.algorithm {
            RateLimitAlgorithm::TokenBucket => f64::from(config.capacity()),
            RateLimitAlgorithm::SlidingWindow => f64::from(config.sustained.rate.max(1)),
        };

        let bucket = self
            .buckets
            .entry(key)
            .or_insert_with(|| Arc::new(Mutex::new(TokenBucket::new(capacity, refill, now))))
            .clone();

        let mut guard = bucket.lock();
        guard.reconfigure(capacity, refill);
        let mut verdict = guard.try_acquire(f64::from(config.cost.max(1)), now);
        verdict.limit = config.sustained.rate;
        verdict
    }

    /// Drop every counter belonging to `upstream_id`.
    pub fn forget_upstream(&self, upstream_id: Uuid) {
        let prefix = format!("upstream:{upstream_id}:");
        self.buckets.retain(|key, _| !key.starts_with(&prefix));
    }

    /// Wait for capacity within `budget`, then answer.
    ///
    /// Backs the `queue` strategy: a short bounded wait, never an unbounded
    /// one, so a saturated upstream cannot pile up in-flight requests.
    pub async fn acquire_queued(
        &self,
        config: &RateLimitConfig,
        subject: &RateLimitSubject,
        client_ip: Option<&str>,
        budget: Duration,
    ) -> RateLimitVerdict {
        let deadline = Instant::now() + budget;
        loop {
            let verdict = self.check(config, subject, client_ip);
            if verdict.allowed {
                return verdict;
            }
            let now = Instant::now();
            if now >= deadline {
                return verdict;
            }
            let step = Duration::from_millis(20).min(deadline.saturating_duration_since(now));
            tokio::time::sleep(step).await;
        }
    }
}

#[cfg(test)]
#[path = "rate_limit_tests.rs"]
mod tests;
