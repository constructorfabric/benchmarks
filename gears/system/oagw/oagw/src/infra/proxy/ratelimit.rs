//! Dual-rate token-bucket rate limiter (ADR 0003).
//!
//! One in-memory bucket per scope key (`{scope}:{scope_id}`, shared across
//! all tenants/upstreams at the process level). A bucket refills at
//! `sustained.rate` tokens per window; the capacity (`burst.capacity`,
//! defaulting to the sustained rate) allows burst usage. Requests over the
//! limit yield a `429` with `Retry-After` and `X-RateLimit-*` headers.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::domain::models::RateLimitConfig;

/// A single token bucket (guarded by the limiter's mutex).
struct Bucket {
    tokens: f64,
    last: Instant,
    capacity: f64,
    refill_per_sec: f64,
}

impl Bucket {
    fn new(capacity: f64, refill_per_sec: f64) -> Self {
        Self {
            tokens: capacity,
            last: Instant::now(),
            capacity,
            refill_per_sec,
        }
    }

    fn take(&mut self, cost: f64) -> bool {
        self.refill();
        if self.tokens < cost {
            return false;
        }
        self.tokens -= cost;
        true
    }

    fn refill(&mut self) {
        let elapsed = self.last.elapsed().as_secs_f64();
        if elapsed > 0.0 {
            self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
            self.last = Instant::now();
        }
    }

    /// Seconds until the bucket is full again (for `Retry-After` / reset).
    fn seconds_until_full(&self) -> u64 {
        let deficit = self.capacity - self.tokens;
        if deficit <= 0.0 || self.refill_per_sec <= 0.0 {
            return 1;
        }
        (deficit / self.refill_per_sec).ceil() as u64
    }
}

/// Outcome of a rate-limit check.
#[derive(Debug, Clone, Copy)]
pub enum RateLimitOutcome {
    /// The request may proceed. `limit`/`remaining` feed `X-RateLimit-*`.
    Allowed { limit: u64, remaining: u64 },
    /// The request is rejected. `retry_after_secs` feeds `Retry-After` and
    /// the problem body; `limit`/`reset` feed `X-RateLimit-*`.
    Limited {
        limit: u64,
        retry_after_secs: u64,
        reset_epoch: u64,
    },
}

/// In-process token-bucket rate limiter shared by the data plane.
pub struct RateLimiter {
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl RateLimiter {
    /// Construct an empty limiter.
    #[must_use]
    pub fn new() -> Self {
        Self {
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Consume `cost` tokens from the bucket for `key` configured by `cfg`
    /// (effective rate/capacity already merged by the caller).
    #[must_use]
    pub fn check(&self, key: &str, rate: f64, capacity: f64, cost: u64) -> RateLimitOutcome {
        let mut buckets = self.buckets.lock().unwrap_or_else(|p| p.into_inner());
        let bucket = buckets
            .entry(key.to_owned())
            .or_insert_with(|| Bucket::new(capacity.max(1.0), rate.max(0.0)));
        let limit = bucket.capacity as u64;
        if !bucket.take(cost as f64) {
            let retry_after_secs = bucket.seconds_until_full();
            let reset_epoch = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() + retry_after_secs)
                .unwrap_or(retry_after_secs);
            return RateLimitOutcome::Limited {
                limit,
                retry_after_secs,
                reset_epoch,
            };
        }
        RateLimitOutcome::Allowed {
            limit,
            remaining: bucket.tokens.floor().max(0.0) as u64,
        }
    }
}

/// Convert a `RateLimitConfig`'s window into refill tokens per second.
#[must_use]
pub fn refill_per_second(rate: u64, window: crate::domain::models::RateLimitWindow) -> f64 {
    match window {
        crate::domain::models::RateLimitWindow::Second => rate as f64,
        crate::domain::models::RateLimitWindow::Minute => rate as f64 / 60.0,
        crate::domain::models::RateLimitWindow::Hour => rate as f64 / 3600.0,
        crate::domain::models::RateLimitWindow::Day => rate as f64 / 86_400.0,
    }
}

/// Effective (rate, capacity) pair for a config: capacity defaults to the
/// sustained rate per ADR 0003.
#[must_use]
pub fn effective_bucket(cfg: &RateLimitConfig) -> (f64, f64) {
    let rate = cfg.sustained.rate as f64;
    let capacity = cfg
        .burst
        .as_ref()
        .map(|b| b.capacity as f64)
        .unwrap_or(rate);
    (rate.max(1.0), capacity.max(1.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{BurstConfig, RateLimitConfig, RateLimitStrategy, SustainedRate};

    fn cfg(rate: u32, capacity: Option<u32>) -> RateLimitConfig {
        RateLimitConfig {
            sharing: crate::domain::models::SharingMode::Private,
            algorithm: crate::domain::models::RateLimitAlgorithm::TokenBucket,
            sustained: SustainedRate {
                rate,
                window: crate::domain::models::RateLimitWindow::Second,
            },
            burst: capacity.map(|c| BurstConfig { capacity: c }),
            scope: crate::domain::models::RateLimitScope::Tenant,
            strategy: RateLimitStrategy::Reject,
            cost: 1,
        }
    }

    #[test]
    fn allows_up_to_capacity() {
        let limiter = RateLimiter::new();
        let (rate, cap) = effective_bucket(&cfg(10, Some(5)));
        // Burst of `cap` passes, the next one is rejected.
        assert!(matches!(
            limiter.check("t:1", rate, cap, 1),
            RateLimitOutcome::Allowed { .. }
        ));
        for _ in 0..4 {
            assert!(matches!(
                limiter.check("t:1", rate, cap, 1),
                RateLimitOutcome::Allowed { .. }
            ));
        }
        assert!(matches!(
            limiter.check("t:1", rate, cap, 1),
            RateLimitOutcome::Limited { .. }
        ));
    }

    #[test]
    fn capacity_defaults_to_rate() {
        let limiter = RateLimiter::new();
        let (rate, cap) = effective_bucket(&cfg(3, None));
        assert_eq!(cap, 3.0);
        for _ in 0..3 {
            assert!(matches!(
                limiter.check("t:2", rate, cap, 1),
                RateLimitOutcome::Allowed { .. }
            ));
        }
        assert!(matches!(
            limiter.check("t:2", rate, cap, 1),
            RateLimitOutcome::Limited { .. }
        ));
    }

    #[test]
    fn keys_are_isolated() {
        let limiter = RateLimiter::new();
        let (rate, cap) = effective_bucket(&cfg(1, None));
        assert!(matches!(
            limiter.check("a", rate, cap, 1),
            RateLimitOutcome::Allowed { .. }
        ));
        assert!(matches!(
            limiter.check("b", rate, cap, 1),
            RateLimitOutcome::Allowed { .. }
        ));
    }

    #[test]
    fn limited_reports_limit_and_retry_after() {
        let limiter = RateLimiter::new();
        let (rate, cap) = effective_bucket(&cfg(2, None));
        let _ = limiter.check("t:3", rate, cap, 1);
        let _ = limiter.check("t:3", rate, cap, 1);
        match limiter.check("t:3", rate, cap, 1) {
            RateLimitOutcome::Limited {
                limit,
                retry_after_secs,
                ..
            } => {
                assert_eq!(limit, 2);
                assert!(retry_after_secs >= 1);
            }
            other => panic!("expected limited, got {other:?}"),
        }
    }

    #[test]
    fn window_conversions() {
        use crate::domain::models::RateLimitWindow as W;
        assert_eq!(refill_per_second(60, W::Second), 60.0);
        assert_eq!(refill_per_second(60, W::Minute), 1.0);
        assert_eq!(refill_per_second(60, W::Hour), 60.0 / 3600.0);
        assert_eq!(refill_per_second(60, W::Day), 60.0 / 86_400.0);
    }

    #[test]
    fn zero_duration_between_checks_ok() {
        let limiter = RateLimiter::new();
        let start = Instant::now();
        let (rate, cap) = effective_bucket(&cfg(1, None));
        assert!(matches!(
            limiter.check("t:4", rate, cap, 1),
            RateLimitOutcome::Allowed { .. }
        ));
        // Immediate second check must not refill past capacity.
        assert!(matches!(
            limiter.check("t:4", rate, cap, 1),
            RateLimitOutcome::Limited { .. }
        ));
        assert!(start.elapsed().as_millis() < 50);
    }
}
