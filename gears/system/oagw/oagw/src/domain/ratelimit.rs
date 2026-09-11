//! Token-bucket rate limiting.
//!
//! Buckets are keyed by `(scope, resource, principal)` and refill continuously
//! from the effective sustained rate; the burst capacity is the bucket size.

use crate::domain::error::OagwError;
use crate::domain::model::RateLimit;
use dashmap::DashMap;
use std::time::{Duration, Instant};

/// One token bucket.
#[derive(Debug, Clone)]
pub struct TokenBucket {
    /// Tokens currently available.
    pub tokens: f64,
    /// Bucket capacity.
    pub capacity: f64,
    /// Tokens replenished per second.
    pub per_second: f64,
    /// Last refill instant.
    pub updated: Instant,
}

impl TokenBucket {
    /// Creates a bucket from an effective rate limit.
    #[must_use]
    pub fn from_config(config: &RateLimit) -> Self {
        let capacity = f64::from(config.capacity());
        let window_seconds = config
            .sustained
            .window
            .duration()
            .as_secs_f64()
            .max(1.0 / 1_000.0);
        Self {
            tokens: capacity,
            capacity,
            per_second: f64::from(config.sustained.rate) / window_seconds,
            updated: Instant::now(),
        }
    }

    /// Refills the bucket up to its capacity for the elapsed time.
    pub fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.updated).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.per_second).min(self.capacity);
        self.updated = now;
    }

    /// Seconds until `cost` tokens are available, zero when they already are.
    #[must_use]
    pub fn seconds_until_available(&self, cost: f64) -> f64 {
        if self.tokens >= cost || self.per_second <= 0.0 {
            return 0.0;
        }
        (cost - self.tokens) / self.per_second
    }
}

/// The outcome of a rate-limit check.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateDecision {
    /// Whether the request is allowed.
    pub allowed: bool,
    /// Configured limit, in tokens.
    pub limit: u32,
    /// Tokens left after the check.
    pub remaining: u32,
    /// Seconds until the bucket is full again.
    pub reset_seconds: u64,
}

/// Key under which a counter is kept.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BucketKey {
    /// Configured scope name.
    pub scope: String,
    /// Resource the counter is attached to (upstream or route id).
    pub resource: String,
    /// Principal the counter is attached to.
    pub principal: String,
}

impl BucketKey {
    /// Builds a key from its parts.
    #[must_use]
    pub fn new(
        scope: impl Into<String>,
        resource: impl Into<String>,
        principal: impl Into<String>,
    ) -> Self {
        Self {
            scope: scope.into(),
            resource: resource.into(),
            principal: principal.into(),
        }
    }
}

/// A process-wide rate-limit registry.
#[derive(Debug, Default)]
pub struct RateLimiter {
    buckets: DashMap<BucketKey, TokenBucket>,
}

impl RateLimiter {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Rounds a fractional token or seconds count down to its integer form.
    ///
    /// `as` from `f64` to an integer saturates, so a non-finite or negative
    /// count cannot wrap; the lints that guard the cast are quiet because the
    /// conversion is deliberately saturating.
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    fn saturate(value: f64) -> u64 {
        value.floor().max(0.0) as u64
    }

    /// Consumes `cost` tokens for the key, refilling first.
    #[must_use]
    pub fn check(
        &self,
        key: &BucketKey,
        config: &RateLimit,
        cost: u32,
        now: Instant,
    ) -> RateDecision {
        let cost = f64::from(cost.max(1));
        let mut entry = self
            .buckets
            .entry(key.clone())
            .or_insert_with(|| TokenBucket::from_config(config));
        entry.refill(now);
        if entry.tokens >= cost {
            entry.tokens -= cost;
            let remaining = entry.tokens.max(0.0);
            RateDecision {
                allowed: true,
                limit: config.capacity(),
                remaining: u32::try_from(Self::saturate(remaining)).unwrap_or(u32::MAX),
                reset_seconds: Self::saturate(entry.seconds_until_available(entry.capacity).ceil()),
            }
        } else {
            let wait = entry.seconds_until_available(cost);
            RateDecision {
                allowed: false,
                limit: config.capacity(),
                remaining: 0,
                reset_seconds: Self::saturate(wait.ceil().max(1.0)),
            }
        }
    }

    /// The [`OagwError`] raised when a check fails.
    #[must_use]
    pub fn exceeded(decision: &RateDecision) -> OagwError {
        OagwError::RateLimitExceeded {
            detail: format!(
                "rate limit of {} exceeded; retry in {}s",
                decision.limit, decision.reset_seconds
            ),
            retry_after: Duration::from_secs(decision.reset_seconds.max(1)),
        }
    }
}

/// Formats the `X-RateLimit-*` header triple for a decision.
#[must_use]
pub fn rate_limit_headers(decision: &RateDecision) -> [(String, String); 3] {
    [
        ("X-RateLimit-Limit".to_owned(), decision.limit.to_string()),
        (
            "X-RateLimit-Remaining".to_owned(),
            decision.remaining.to_string(),
        ),
        (
            "X-RateLimit-Reset".to_owned(),
            decision.reset_seconds.to_string(),
        ),
    ]
}

#[cfg(test)]
#[path = "ratelimit_tests.rs"]
mod tests;
