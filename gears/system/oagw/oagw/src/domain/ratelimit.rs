// Created: 2026-09-01 by Constructor Tech
//! Rate-limit counters.
//!
//! `docs/ADR/0003-rate-limiting.md`. Two algorithms are implemented: a
//! token bucket (bursts allowed) and a sliding window (no boundary burst).
//! Counters are keyed by the resolved scope and held per tenant.

use std::collections::BTreeMap;
use std::time::Instant;

use parking_lot::Mutex;

use super::model::{RateAlgorithm, RateLimit, RateScope};

/// The key a counter lives under.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct CounterKey {
    /// Tenant that owns the counter.
    pub tenant_id: String,
    /// Subject, client IP or route id, depending on the scope.
    pub subject: String,
    /// Route id, when the scope is per-route.
    pub route_id: String,
}

impl CounterKey {
    /// Build the key for `scope`.
    #[must_use]
    pub fn build(scope: RateScope, tenant_id: &str, subject: &str, route_id: &str) -> Self {
        let (t, s, r) = match scope {
            RateScope::Global => (String::new(), String::new(), String::new()),
            RateScope::Tenant => (tenant_id.to_owned(), String::new(), String::new()),
            RateScope::User => (tenant_id.to_owned(), subject.to_owned(), String::new()),
            RateScope::Ip => (tenant_id.to_owned(), subject.to_owned(), String::new()),
            RateScope::Route => (
                tenant_id.to_owned(),
                subject.to_owned(),
                route_id.to_owned(),
            ),
        };
        Self {
            tenant_id: t,
            subject: s,
            route_id: r,
        }
    }
}

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    last: Instant,
    /// Timestamps of accepted requests, for the sliding window.
    window: Vec<Instant>,
}

impl Bucket {
    fn new(capacity: f64) -> Self {
        Self {
            tokens: capacity,
            last: Instant::now(),
            window: Vec::new(),
        }
    }
}

/// The rate limiter.
#[derive(Debug)]
pub struct RateLimiter {
    buckets: Mutex<BTreeMap<CounterKey, Bucket>>,
}

/// Outcome of a rate-limit check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    /// Whether the request may proceed.
    pub allowed: bool,
    /// Remaining tokens in the current window.
    pub remaining: u64,
    /// Seconds until the bucket refills enough for one more request.
    pub retry_after_secs: u64,
    /// Seconds until the window fully resets.
    pub reset_secs: u64,
    /// Configured capacity.
    pub limit: u64,
}

impl RateLimiter {
    /// An empty limiter.
    #[must_use]
    pub fn new() -> Self {
        Self {
            buckets: Mutex::new(BTreeMap::new()),
        }
    }

    /// Check `key` against `config`, consuming `cost` tokens when allowed.
    #[must_use]
    pub fn check(&self, key: &CounterKey, config: &RateLimit) -> Decision {
        let capacity = config.capacity().max(1) as f64;
        let refill = config.refill_per_sec().max(f64::EPSILON);
        let cost = config.cost.max(1) as f64;
        let now = Instant::now();
        let window = config.sustained.window.duration();

        let mut buckets = self.buckets.lock();
        let bucket = buckets
            .entry(key.clone())
            .or_insert_with(|| Bucket::new(capacity));

        match config.algorithm {
            RateAlgorithm::TokenBucket => {
                let elapsed = now.duration_since(bucket.last).as_secs_f64();
                bucket.tokens = (bucket.tokens + elapsed * refill).min(capacity);
                bucket.last = now;
                if bucket.tokens >= cost {
                    bucket.tokens -= cost;
                    let remaining = bucket.tokens.floor().max(0.0) as u64;
                    let retry = if remaining >= 1 {
                        0.0
                    } else {
                        (cost - bucket.tokens).max(0.0) / refill
                    };
                    Decision {
                        allowed: true,
                        remaining,
                        retry_after_secs: retry.ceil() as u64,
                        reset_secs: ((capacity - bucket.tokens) / refill).ceil() as u64,
                        limit: config.capacity(),
                    }
                } else {
                    let retry = (cost - bucket.tokens).max(0.0) / refill;
                    Decision {
                        allowed: false,
                        remaining: 0,
                        retry_after_secs: retry.ceil().max(1.0) as u64,
                        reset_secs: ((capacity - bucket.tokens) / refill).ceil() as u64,
                        limit: config.capacity(),
                    }
                }
            }
            RateAlgorithm::SlidingWindow => {
                bucket.window.retain(|t| now.duration_since(*t) < window);
                if bucket.window.len() < config.sustained.rate as usize {
                    bucket.window.push(now);
                    let remaining = (config.sustained.rate as usize - bucket.window.len()) as u64;
                    Decision {
                        allowed: true,
                        remaining,
                        retry_after_secs: 0,
                        reset_secs: window.as_secs(),
                        limit: config.sustained.rate,
                    }
                } else {
                    let oldest = bucket
                        .window
                        .first()
                        .copied()
                        .map(|t| now.duration_since(t).as_secs_f64());
                    let retry = oldest
                        .map(|age| (window.as_secs_f64() - age).ceil())
                        .unwrap_or(1.0);
                    Decision {
                        allowed: false,
                        remaining: 0,
                        retry_after_secs: retry.max(1.0) as u64,
                        reset_secs: window.as_secs(),
                        limit: config.sustained.rate,
                    }
                }
            }
        }
    }

    /// Forget every counter. Used by tests.
    pub fn clear(&self) {
        self.buckets.lock().clear();
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::model::{Burst, RateScope, RateWindow, SustainedRate};

    fn config(rate: u64, window: RateWindow, capacity: u64) -> RateLimit {
        RateLimit {
            sustained: SustainedRate { rate, window },
            burst: Some(Burst { capacity }),
            ..RateLimit::default()
        }
    }

    #[test]
    fn a_full_bucket_allows_a_burst_then_rejects() {
        let limiter = RateLimiter::new();
        let key = CounterKey::build(RateScope::Tenant, "t1", "", "");
        let cfg = config(5, RateWindow::Second, 5);
        for i in 0..5 {
            let d = limiter.check(&key, &cfg);
            assert!(d.allowed, "request {i} should pass");
        }
        let d = limiter.check(&key, &cfg);
        assert!(!d.allowed);
        assert!(d.retry_after_secs >= 1);
    }

    #[test]
    fn the_bucket_refills_over_time() {
        let limiter = RateLimiter::new();
        let key = CounterKey::build(RateScope::Tenant, "t1", "", "");
        let cfg = RateLimit {
            sustained: SustainedRate {
                rate: 100,
                window: RateWindow::Second,
            },
            burst: Some(Burst { capacity: 2 }),
            ..RateLimit::default()
        };
        assert!(limiter.check(&key, &cfg).allowed);
        assert!(limiter.check(&key, &cfg).allowed);
        assert!(!limiter.check(&key, &cfg).allowed);
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert!(limiter.check(&key, &cfg).allowed);
    }

    #[test]
    fn scopes_do_not_share_counters() {
        let limiter = RateLimiter::new();
        let cfg = config(1, RateWindow::Second, 1);
        let a = CounterKey::build(RateScope::User, "t1", "alice", "");
        let b = CounterKey::build(RateScope::User, "t1", "bob", "");
        assert!(limiter.check(&a, &cfg).allowed);
        assert!(!limiter.check(&a, &cfg).allowed);
        assert!(limiter.check(&b, &cfg).allowed);
    }

    #[test]
    fn cost_is_honoured() {
        let limiter = RateLimiter::new();
        let key = CounterKey::build(RateScope::Global, "", "", "");
        let cfg = RateLimit {
            sustained: SustainedRate {
                rate: 10,
                window: RateWindow::Second,
            },
            burst: Some(Burst { capacity: 10 }),
            cost: 5,
            ..RateLimit::default()
        };
        assert!(limiter.check(&key, &cfg).allowed);
        assert!(limiter.check(&key, &cfg).allowed);
        assert!(!limiter.check(&key, &cfg).allowed);
    }

    #[test]
    fn sliding_window_has_no_boundary_burst() {
        let limiter = RateLimiter::new();
        let key = CounterKey::build(RateScope::Tenant, "t1", "", "");
        let cfg = RateLimit {
            algorithm: RateAlgorithm::SlidingWindow,
            sustained: SustainedRate {
                rate: 3,
                window: RateWindow::Second,
            },
            ..RateLimit::default()
        };
        for i in 0..3 {
            assert!(limiter.check(&key, &cfg).allowed, "{i}");
        }
        assert!(!limiter.check(&key, &cfg).allowed);
    }

    #[test]
    fn a_minute_window_reports_reset_in_seconds() {
        let limiter = RateLimiter::new();
        let key = CounterKey::build(RateScope::Tenant, "t1", "", "");
        let cfg = config(2, RateWindow::Minute, 2);
        let d = limiter.check(&key, &cfg);
        assert!(d.allowed);
        assert_eq!(d.limit, 2);
        assert_eq!(d.remaining, 1);
        // A token bucket resets when it is full again: 1 token short at a
        // refill rate of 2/minute is 30s away.
        assert_eq!(d.reset_secs, 30);
    }
}
