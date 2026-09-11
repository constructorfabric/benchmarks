// Updated: 2026-09-01 by Constructor Tech
//! Rate limiting for the Data Plane.
//!
//! Two algorithms, both sharded by the key the route's `rate_limit.scope`
//! selects. The token bucket is the default and the only one that can absorb a
//! burst; the sliding window is the one that guarantees a steady ceiling.
//!
//! Buckets live in a [`DashMap`] so a hot key costs one lock and an idle key
//! costs none. They are never swept: an entry is a handful of floats and the
//! key space is bounded by tenants times routes.

use std::time::{Duration, Instant};

use dashmap::DashMap;

use crate::domain::dto::RateLimitAlgorithm;

/// What one permit costs and how fast they are minted.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limit {
    /// Sustained permits per second.
    pub rate: f64,
    /// Permits that may be spent at once.
    pub capacity: f64,
    pub algorithm: RateLimitAlgorithm,
}

impl Limit {
    #[must_use]
    pub fn new(rate: f64, capacity: f64, algorithm: RateLimitAlgorithm) -> Self {
        Self {
            rate: rate.max(0.0),
            capacity: capacity.max(0.0),
            algorithm,
        }
    }

    /// Permits minted over a span, floored so a zero rate mints nothing.
    fn refill(&self, elapsed: Duration) -> f64 {
        self.rate * elapsed.as_secs_f64()
    }
}

/// A decision, with the headers a 429 would carry.
#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    pub allowed: bool,
    /// Whole seconds until a permit is available; zero when the request passed.
    pub retry_after: u64,
    /// Permits left in the bucket after this request.
    pub remaining: u64,
    /// When the bucket is full again. `X-RateLimit-Reset` is derived from it,
    /// so a client knows when its budget comes back rather than only when the
    /// next single permit does.
    pub reset: Instant,
}

/// The limiter table.
#[derive(Debug, Default)]
pub struct RateLimiter {
    buckets: DashMap<String, Bucket>,
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    /// Window counters, for the sliding-window algorithm.
    started: Instant,
    spent: f64,
    last: Instant,
}

impl RateLimiter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Spend one permit from `key`.
    pub fn check(&self, key: &str, limit: &Limit) -> Verdict {
        let now = Instant::now();
        let mut entry = self
            .buckets
            .entry(key.to_owned())
            .or_insert_with(|| Bucket {
                tokens: limit.capacity,
                started: now,
                spent: 0.0,
                last: now,
            });
        let bucket = entry.value_mut();
        let elapsed = now.saturating_duration_since(bucket.last);
        bucket.last = now;

        match limit.algorithm {
            RateLimitAlgorithm::TokenBucket => {
                bucket.tokens = (bucket.tokens + limit.refill(elapsed)).min(limit.capacity);
                let full = now + refill_time(limit, bucket.tokens);
                if limit.capacity > 0.0 && bucket.tokens >= 1.0 {
                    bucket.tokens -= 1.0;
                    Verdict {
                        allowed: true,
                        retry_after: 0,
                        remaining: bucket.tokens.floor() as u64,
                        reset: full,
                    }
                } else {
                    let missing = 1.0_f64 - bucket.tokens;
                    let wait = if limit.rate > 0.0 {
                        (missing / limit.rate).ceil().max(1.0) as u64
                    } else {
                        1
                    };
                    Verdict {
                        allowed: false,
                        retry_after: wait,
                        remaining: bucket.tokens.floor() as u64,
                        reset: full,
                    }
                }
            }
            RateLimitAlgorithm::SlidingWindow => {
                let window = window_for(limit.capacity, limit.rate);
                if now.saturating_duration_since(bucket.started) >= window {
                    bucket.started = now;
                    bucket.spent = 0.0;
                }
                let full = bucket.started + window;
                if bucket.spent + 1.0 <= limit.capacity {
                    bucket.spent += 1.0;
                    Verdict {
                        allowed: true,
                        retry_after: 0,
                        remaining: (limit.capacity - bucket.spent).max(0.0) as u64,
                        reset: full,
                    }
                } else {
                    let remaining_window = window
                        .saturating_sub(now.saturating_duration_since(bucket.started))
                        .as_secs();
                    Verdict {
                        allowed: false,
                        retry_after: remaining_window.max(1),
                        remaining: 0,
                        reset: full,
                    }
                }
            }
        }
    }

    /// Drop every bucket. Used by tests and by a config reload.
    pub fn clear(&self) {
        self.buckets.clear();
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.buckets.len()
    }
}

/// How long the bucket needs to fill again, given what is in it.
///
/// A drained bucket refills at the sustained rate; a full one is already
/// there. A zero rate never fills, so the answer is "not in this lifetime" —
/// reported as now, because a client cannot act on an eternity.
#[must_use]
fn refill_time(limit: &Limit, tokens: f64) -> Duration {
    if limit.rate <= 0.0 || tokens >= limit.capacity {
        return Duration::ZERO;
    }
    Duration::from_secs_f64((limit.capacity - tokens) / limit.rate)
}

/// The window the sliding-window algorithm counts over, derived from the
/// configured rate so that `rate` and `capacity` describe the same budget.
fn window_for(capacity: f64, rate: f64) -> Duration {
    if rate <= 0.0 {
        Duration::from_secs(1)
    } else {
        Duration::from_secs_f64((capacity / rate).clamp(0.001, 86_400.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bucket(rate: f64, capacity: f64) -> Limit {
        Limit::new(rate, capacity, RateLimitAlgorithm::TokenBucket)
    }

    #[test]
    fn full_bucket_admits_a_burst_then_rejects() {
        let limiter = RateLimiter::new();
        let limit = bucket(1.0, 3.0);
        for i in 0..3 {
            let v = limiter.check("k", &limit);
            assert!(v.allowed, "request {i} should pass");
        }
        let v = limiter.check("k", &limit);
        assert!(!v.allowed);
        assert_eq!(v.retry_after, 1);
    }

    #[test]
    fn buckets_are_independent_per_key() {
        let limiter = RateLimiter::new();
        let limit = bucket(1.0, 1.0);
        assert!(limiter.check("a", &limit).allowed);
        assert!(!limiter.check("a", &limit).allowed);
        assert!(limiter.check("b", &limit).allowed);
        assert_eq!(limiter.len(), 2);
    }

    #[test]
    fn zero_capacity_never_admits() {
        let limiter = RateLimiter::new();
        let limit = bucket(10.0, 0.0);
        assert!(!limiter.check("z", &limit).allowed);
    }

    #[test]
    fn zero_rate_admits_the_burst_only() {
        let limiter = RateLimiter::new();
        let limit = bucket(0.0, 2.0);
        assert!(limiter.check("r", &limit).allowed);
        assert!(limiter.check("r", &limit).allowed);
        let v = limiter.check("r", &limit);
        assert!(!v.allowed);
        assert_eq!(v.retry_after, 1);
    }

    #[test]
    fn sliding_window_is_a_hard_ceiling() {
        let limiter = RateLimiter::new();
        let limit = Limit::new(10.0, 5.0, RateLimitAlgorithm::SlidingWindow);
        for i in 0..5 {
            assert!(limiter.check("w", &limit).allowed, "request {i}");
        }
        let v = limiter.check("w", &limit);
        assert!(!v.allowed);
        assert!(v.retry_after <= 1, "window is under a second");
    }

    #[test]
    fn refill_recovers_after_a_pause() {
        let limiter = RateLimiter::new();
        let limit = bucket(1000.0, 1.0);
        assert!(limiter.check("t", &limit).allowed);
        assert!(!limiter.check("t", &limit).allowed);
        std::thread::sleep(Duration::from_millis(20));
        assert!(
            limiter.check("t", &limit).allowed,
            "refill should have minted a permit"
        );
    }
}
