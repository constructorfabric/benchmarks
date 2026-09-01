//! Token bucket rate limiter (ADR-0003 dual-rate).
//!
//! A shared limiter keeps per-key buckets in memory. Each bucket refills at
//! `rate / window_secs` tokens per second up to `capacity`. Requests consume
//! `cost` tokens. When the bucket cannot cover the cost, the request is
//! rejected with the seconds until enough tokens are available.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::domain::dto::RateLimitConfig;

/// Outcome of a rate-limit check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateOutcome {
    /// The request may proceed. `remaining` is the token balance (floor),
    /// `reset_epoch` the unix seconds at which the bucket is full again.
    Allowed { remaining: u64, reset_epoch: u64 },
    /// The request is rejected. `retry_after_secs` is the wait until enough
    /// tokens are available.
    Rejected { retry_after_secs: u64 },
}

/// Max tracked buckets before opportunistic eviction kicks in (bounds
/// memory for arbitrarily many distinct keys).
const MAX_BUCKETS: usize = 10_000;

/// In-memory token bucket limiter.
#[derive(Debug, Default)]
pub struct RateLimiter {
    buckets: Mutex<HashMap<String, Bucket>>,
}

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    last_refill: Instant,
    capacity: f64,
    refill_per_sec: f64,
}

impl Default for Bucket {
    fn default() -> Self {
        Self {
            tokens: 0.0,
            last_refill: Instant::now(),
            capacity: 0.0,
            refill_per_sec: 0.0,
        }
    }
}

/// Rate-limit header values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimitHeaders {
    /// `X-RateLimit-Limit` (bucket capacity).
    pub limit: u64,
    /// `X-RateLimit-Remaining`.
    pub remaining: u64,
    /// `X-RateLimit-Reset` (unix seconds when the bucket is full).
    pub reset_epoch: u64,
    /// `Retry-After` (seconds), present only on rejection.
    pub retry_after_secs: Option<u64>,
}

impl RateLimiter {
    /// Create an empty limiter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Check and (on allow) consume tokens for `key`.
    ///
    /// Token-bucket arithmetic is intentionally `f64` (fractional refill
    /// rates); the integer conversions below only appear in the externally
    /// visible `u64` header values and are bounds-safe.
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::float_cmp // exact equality re-keys a bucket when its geometry changes
    )]
    pub fn check(&self, key: &str, cfg: &RateLimitConfig) -> RateLimitHeaders {
        let capacity = cfg.burst_capacity() as f64;
        let refill_per_sec = cfg.sustained.rate as f64 / cfg.sustained.window.as_secs() as f64;
        let cost = cfg.cost as f64;
        let now = Instant::now();

        let mut buckets = self.buckets.lock();
        // Opportunistic eviction: once the map is over the cap, drop buckets
        // that are both full and idle for at least one window — on the next
        // check they are indistinguishable from a freshly created bucket, so
        // nothing observable changes.
        if buckets.len() > MAX_BUCKETS {
            let window = Duration::from_secs(cfg.sustained.window.as_secs());
            buckets.retain(|_, b| {
                b.tokens < b.capacity || now.saturating_duration_since(b.last_refill) <= window
            });
        }
        let bucket = buckets.entry(key.to_owned()).or_insert_with(|| Bucket {
            tokens: capacity,
            last_refill: now,
            capacity,
            refill_per_sec,
        });
        // Re-key into the new geometry when the config changes.
        if bucket.capacity != capacity || bucket.refill_per_sec != refill_per_sec {
            *bucket = Bucket {
                tokens: capacity,
                last_refill: now,
                capacity,
                refill_per_sec,
            };
        }

        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * bucket.refill_per_sec).min(bucket.capacity);
        bucket.last_refill = now;

        let remaining = bucket.tokens.floor().max(0.0) as u64;
        let reset_epoch = remaining_refill_epoch(bucket);

        if bucket.tokens >= cost {
            bucket.tokens -= cost;
            RateLimitHeaders {
                limit: capacity as u64,
                remaining,
                reset_epoch,
                retry_after_secs: None,
            }
        } else {
            let deficit = cost - bucket.tokens;
            let retry_after = (deficit / bucket.refill_per_sec).ceil() as u64;
            RateLimitHeaders {
                limit: capacity as u64,
                remaining,
                reset_epoch,
                retry_after_secs: Some(retry_after.max(1)),
            }
        }
    }

    /// Number of tracked buckets (test/observability helper).
    #[must_use]
    pub fn len(&self) -> usize {
        self.buckets.lock().len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Unix epoch seconds at which the bucket will be completely full.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss // seconds are always non-negative
)]
fn remaining_refill_epoch(bucket: &Bucket) -> u64 {
    let needed = bucket.capacity - bucket.tokens;
    let secs = if bucket.refill_per_sec > 0.0 {
        (needed / bucket.refill_per_sec).ceil() as u64
    } else {
        0
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    now.saturating_add(secs)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::dto::{RateSpec, RateWindow};

    #[test]
    fn allows_burst_then_rejects() {
        let limiter = RateLimiter::new();
        let cfg = RateLimitConfig {
            sustained: RateSpec {
                rate: 2,
                window: RateWindow::Second,
            },
            ..Default::default()
        };
        // Capacity defaults to sustained.rate → 2 tokens.
        let first = limiter.check("k", &cfg);
        assert_eq!(first.remaining, 2);
        let second = limiter.check("k", &cfg);
        assert_eq!(second.remaining, 1);
        let third = limiter.check("k", &cfg);
        assert!(third.retry_after_secs.is_some(), "bucket exhausted");
        assert_eq!(third.remaining, 0);
    }

    #[test]
    fn different_keys_are_independent() {
        let limiter = RateLimiter::new();
        let cfg = RateLimitConfig {
            sustained: RateSpec {
                rate: 1,
                window: RateWindow::Second,
            },
            ..Default::default()
        };
        assert_eq!(limiter.check("a", &cfg).remaining, 1);
        assert_eq!(limiter.check("b", &cfg).remaining, 1);
    }
}
