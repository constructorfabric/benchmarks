//! Token-bucket rate limiting (ADR-0003).
//!
//! Buckets are keyed by `(resource_id, scope_key)` in a
//! [`RateLimiterRegistry`]; the hierarchical merge applied by the control
//! plane is `min(ancestor, descendant)` for both the sustained rate and the
//! burst capacity.

use std::time::Duration;

use dashmap::DashMap;

use crate::domain::model::RateLimitConfig;

/// Outcome of a rate limit check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimitOutcome {
    /// The request is allowed; `remaining` tokens are left.
    Allowed { remaining: u32 },
    /// The request is rejected and the caller should wait this long.
    Rejected { retry_after_seconds: u64 },
}

/// `X-RateLimit-*` headers of a check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimitHeaders {
    /// Effective limit per window (`X-RateLimit-Limit`).
    pub limit: u32,
    /// Tokens left in the bucket (`X-RateLimit-Remaining`).
    pub remaining: u32,
    /// Seconds until the bucket refills (`X-RateLimit-Reset`).
    pub reset_seconds: u64,
}

impl RateLimitHeaders {
    /// Renders the header name/value pairs for a response.
    #[must_use]
    pub fn headers(self) -> Vec<(String, String)> {
        vec![
            ("X-RateLimit-Limit".to_owned(), self.limit.to_string()),
            (
                "X-RateLimit-Remaining".to_owned(),
                self.remaining.to_string(),
            ),
            (
                "X-RateLimit-Reset".to_owned(),
                self.reset_seconds.to_string(),
            ),
        ]
    }
}

/// A token bucket with sustained refill and burst capacity.
#[derive(Debug)]
pub struct TokenBucket {
    capacity: u32,
    sustained_rate: u32,
    window: Duration,
    cost: u32,
    tokens: f64,
    last_refill: std::time::Instant,
}

impl TokenBucket {
    /// Builds a bucket from a rate limit configuration.
    #[must_use]
    pub fn from_config(config: &RateLimitConfig, now: std::time::Instant) -> Self {
        Self::new(
            config.sustained.rate,
            config.capacity(),
            Duration::from_secs(config.sustained.window.seconds()),
            config.cost,
            now,
        )
    }

    /// Builds a bucket explicitly.
    #[must_use]
    pub fn new(
        sustained_rate: u32,
        capacity: u32,
        window: Duration,
        cost: u32,
        now: std::time::Instant,
    ) -> Self {
        Self {
            capacity,
            sustained_rate,
            window,
            cost,
            tokens: f64::from(capacity),
            last_refill: now,
        }
    }

    /// Effective limit reported in `X-RateLimit-Limit` (tokens per window).
    #[must_use]
    pub const fn limit(&self) -> u32 {
        self.sustained_rate
    }

    /// Maximum tokens the bucket can hold.
    #[must_use]
    pub const fn capacity(&self) -> u32 {
        self.capacity
    }

    /// Tokens currently in the bucket.
    #[must_use]
    pub fn tokens(&self) -> u32 {
        self.tokens.floor() as u32
    }

    /// Seconds until the bucket is full again.
    #[must_use]
    pub fn reset_seconds(&self, now: std::time::Instant) -> u64 {
        if self.tokens >= f64::from(self.capacity) {
            return 0;
        }
        let per_second = self.refill_per_second();
        if per_second <= 0.0 {
            return self.window.as_secs().max(1);
        }
        let missing = f64::from(self.capacity) - self.tokens;
        let seconds = missing / per_second;
        let elapsed = now.saturating_duration_since(self.last_refill);
        (seconds.max(0.0).ceil() as u64)
            .saturating_sub(elapsed.as_secs())
            .max(1)
    }

    /// Refill rate in tokens per second.
    fn refill_per_second(&self) -> f64 {
        let window_secs = self.window.as_secs_f64();
        if window_secs <= 0.0 {
            return 0.0;
        }
        f64::from(self.sustained_rate) / window_secs
    }

    /// Refills the bucket up to its capacity and tries to consume `cost`.
    pub fn try_acquire(&mut self, now: std::time::Instant) -> RateLimitOutcome {
        self.refill(now);
        if self.tokens >= f64::from(self.cost) {
            self.tokens -= f64::from(self.cost);
            RateLimitOutcome::Allowed {
                remaining: self.tokens.floor() as u32,
            }
        } else {
            let missing = f64::from(self.cost) - self.tokens;
            let per_second = self.refill_per_second();
            let retry_after = if per_second > 0.0 {
                (missing / per_second).ceil() as u64
            } else {
                1
            };
            RateLimitOutcome::Rejected {
                retry_after_seconds: retry_after.max(1),
            }
        }
    }

    fn refill(&mut self, now: std::time::Instant) {
        let elapsed = now.saturating_duration_since(self.last_refill);
        if elapsed.is_zero() {
            return;
        }
        let gained = elapsed.as_secs_f64() * self.refill_per_second();
        self.tokens = (self.tokens + gained).min(f64::from(self.capacity));
        self.last_refill = now;
    }
}

/// Hierarchical merge of a rate limit: `min(own, inherited)` for both the
/// sustained rate and the burst capacity (ADR-0003).
#[must_use]
pub fn merge_rate_limits(own: &RateLimitConfig, inherited: &RateLimitConfig) -> RateLimitConfig {
    let mut merged = own.clone();
    let own_capacity = own.capacity();
    let inherited_capacity = inherited.capacity();
    if inherited.sustained.rate < merged.sustained.rate {
        merged.sustained.rate = inherited.sustained.rate;
    }
    if inherited_capacity < own_capacity {
        merged.burst = Some(crate::domain::model::BurstConfig {
            capacity: inherited_capacity,
        });
    }
    merged
}

/// Key of a rate limit bucket.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RateLimitKey {
    /// Id of the resource the limit belongs to (upstream or route).
    pub resource_id: uuid::Uuid,
    /// Scope key (`tenant:<id>`, `user:<id>`, `ip:<addr>`, `global`, ...).
    pub scope_key: String,
}

/// Registry of token buckets keyed by `(resource_id, scope_key)`.
#[derive(Debug, Default)]
pub struct RateLimiterRegistry {
    buckets: DashMap<RateLimitKey, parking_lot::Mutex<TokenBucket>>,
}

impl RateLimiterRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Runs a check for `key`, creating the bucket when it is the first use.
    pub fn check(
        &self,
        key: &RateLimitKey,
        config: &RateLimitConfig,
        now: std::time::Instant,
    ) -> (RateLimitOutcome, RateLimitHeaders) {
        let bucket = self
            .buckets
            .entry(key.clone())
            .or_insert_with(|| parking_lot::Mutex::new(TokenBucket::from_config(config, now)));
        let mut guard = bucket.lock();
        let outcome = guard.try_acquire(now);
        let limit = guard.limit();
        let reset_seconds = guard.reset_seconds(now);
        let remaining = match outcome {
            RateLimitOutcome::Allowed { remaining } => remaining,
            RateLimitOutcome::Rejected { .. } => 0,
        };
        (
            outcome,
            RateLimitHeaders {
                limit,
                remaining,
                reset_seconds,
            },
        )
    }

    /// Number of live buckets.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buckets.len()
    }

    /// Whether the registry is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{BurstConfig, RateWindow, SharingMode, SustainedRate};
    use std::time::Instant;

    fn config(rate: u32, capacity: Option<u32>, cost: u32) -> RateLimitConfig {
        RateLimitConfig {
            sharing: SharingMode::Private,
            algorithm: crate::domain::model::RateLimitAlgorithm::TokenBucket,
            sustained: SustainedRate {
                rate,
                window: RateWindow::Second,
            },
            burst: capacity.map(|capacity| BurstConfig { capacity }),
            scope: crate::domain::model::RateScope::Tenant,
            strategy: crate::domain::model::RateStrategy::Reject,
            response_headers: true,
            cost,
        }
    }

    #[test]
    fn bucket_allows_bursts_up_to_capacity() {
        let mut bucket = TokenBucket::new(10, 50, Duration::from_secs(1), 1, Instant::now());
        for _ in 0..50 {
            assert!(matches!(
                bucket.try_acquire(Instant::now()),
                RateLimitOutcome::Allowed { .. }
            ));
        }
        assert!(matches!(
            bucket.try_acquire(Instant::now()),
            RateLimitOutcome::Rejected { .. }
        ));
    }

    #[test]
    fn bucket_refills_at_the_sustained_rate() {
        let start = Instant::now();
        let mut bucket = TokenBucket::new(10, 10, Duration::from_secs(1), 1, start);
        for _ in 0..10 {
            bucket.try_acquire(start);
        }
        assert!(matches!(
            bucket.try_acquire(start),
            RateLimitOutcome::Rejected { .. }
        ));
        let later = start + Duration::from_millis(500);
        let outcome = bucket.try_acquire(later);
        assert!(matches!(outcome, RateLimitOutcome::Allowed { remaining } if remaining >= 4));
    }

    #[test]
    fn bucket_consumes_the_configured_cost() {
        let start = Instant::now();
        let mut bucket = TokenBucket::new(10, 10, Duration::from_secs(1), 5, start);
        let outcome = bucket.try_acquire(start);
        assert!(matches!(outcome, RateLimitOutcome::Allowed { remaining } if remaining == 5));
        let outcome = bucket.try_acquire(start);
        assert!(matches!(outcome, RateLimitOutcome::Allowed { remaining } if remaining == 0));
        assert!(matches!(
            bucket.try_acquire(start),
            RateLimitOutcome::Rejected { .. }
        ));
    }

    #[test]
    fn rejection_headers_report_zero_remaining_and_retry_after() {
        let registry = RateLimiterRegistry::new();
        let key = RateLimitKey {
            resource_id: uuid::Uuid::new_v4(),
            scope_key: "tenant:t1".to_owned(),
        };
        let cfg = config(1, Some(1), 1);
        let (first, headers) = registry.check(&key, &cfg, Instant::now());
        assert!(matches!(first, RateLimitOutcome::Allowed { remaining } if remaining == 0));
        assert_eq!(headers.limit, 1);
        let (second, headers) = registry.check(&key, &cfg, Instant::now());
        assert_eq!(
            second,
            RateLimitOutcome::Rejected {
                retry_after_seconds: 1
            }
        );
        assert_eq!(headers.remaining, 0);
        assert!(headers.reset_seconds >= 1);
        let rendered = headers.headers();
        assert_eq!(rendered[0].0, "X-RateLimit-Limit");
        assert_eq!(rendered[1].1, "0");
    }

    #[test]
    fn registry_keys_are_independent() {
        let registry = RateLimiterRegistry::new();
        let cfg = config(5, Some(5), 1);
        let base = RateLimitKey {
            resource_id: uuid::Uuid::new_v4(),
            scope_key: "tenant:a".to_owned(),
        };
        let other = RateLimitKey {
            resource_id: uuid::Uuid::new_v4(),
            scope_key: "tenant:b".to_owned(),
        };
        for _ in 0..5 {
            registry.check(&base, &cfg, Instant::now());
        }
        let (outcome, _) = registry.check(&base, &cfg, Instant::now());
        assert!(matches!(outcome, RateLimitOutcome::Rejected { .. }));
        let (outcome, _) = registry.check(&other, &cfg, Instant::now());
        assert!(matches!(outcome, RateLimitOutcome::Allowed { .. }));
        assert_eq!(registry.len(), 2);
        assert!(!registry.is_empty());
    }

    #[test]
    fn hierarchical_merge_takes_the_minimum() {
        let parent = config(100, Some(500), 1);
        let child = config(50, None, 1);
        let merged = merge_rate_limits(&child, &parent);
        assert_eq!(merged.sustained.rate, 50);
        assert_eq!(merged.capacity(), 50);

        let stricter_parent = config(10, Some(20), 1);
        let merged = merge_rate_limits(&child, &stricter_parent);
        assert_eq!(merged.sustained.rate, 10);
        assert_eq!(merged.capacity(), 20);
    }

    #[test]
    fn windows_scale_the_refill() {
        let start = Instant::now();
        let mut minute = TokenBucket::new(
            60,
            60,
            Duration::from_secs(RateWindow::Minute.seconds()),
            1,
            start,
        );
        assert_eq!(minute.limit(), 60);
        let outcome = minute.try_acquire(start);
        assert!(matches!(outcome, RateLimitOutcome::Allowed { .. }));
    }
}
