//! In-memory token-bucket rate limiting (`docs/ADR/0003`).
//!
//! Each `(upstream, route, scope key)` tuple owns one bucket. A bucket is a
//! classic token bucket: `burst.capacity` tokens are available immediately and
//! `sustained.rate` tokens are replenished every `sustained.window`, so
//! sustained traffic is capped by the rate and bursts by the capacity.
//!
//! Enforcement is local to the process (`docs/ADR/0003` — "MVP: Per-instance
//! rate limiting in Data Plane"); the Redis-backed distributed variant is
//! deferred.
use std::time::Instant;

use dashmap::DashMap;

use crate::domain::model::{RateAlgorithm, RateLimitConfig, RateScope, RateWindow};

/// A rate-limit decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateDecision {
    /// Whether the request may proceed.
    pub allowed: bool,
    /// Whole tokens left in the bucket after the decision.
    pub remaining: u32,
    /// Seconds until the bucket is full again, rounded up.
    pub reset_seconds: u32,
    /// Seconds until one token is available again, when the request was
    /// refused.
    pub retry_after_seconds: u32,
    /// The bucket key the decision was taken against.
    pub scope_key: String,
}

impl RateDecision {
    /// A decision that never limits (no configured limit).
    #[must_use]
    pub fn unlimited() -> Self {
        Self {
            allowed: true,
            remaining: 0,
            reset_seconds: 0,
            retry_after_seconds: 0,
            scope_key: String::new(),
        }
    }
}

/// A bucket's mutable state. Tokens are tracked in thousandths so a slow
/// sustained rate (`1/minute`) still refills between requests.
#[derive(Debug, Clone)]
struct Bucket {
    /// Tokens currently available, in milli-tokens.
    milli_tokens: u64,
    /// Instant of the last refill, used to credit elapsed time.
    last_refill: Instant,
}

impl Bucket {
    fn full(capacity_tokens: u64) -> Self {
        Self {
            milli_tokens: capacity_tokens.saturating_mul(MILLI),
            last_refill: Instant::now(),
        }
    }
}

/// One token in milli-tokens.
const MILLI: u64 = 1_000;

/// Bucket key for a request, per the configured scope.
#[must_use]
pub fn scope_key(
    config: &RateLimitConfig,
    upstream_id: uuid::Uuid,
    route_id: uuid::Uuid,
    tenant_id: uuid::Uuid,
    identity: &str,
) -> String {
    let head = format!("upstream:{upstream_id}:route:{route_id}");
    match config.scope {
        RateScope::Global => format!("{head}:scope:global"),
        RateScope::Route => format!("{head}:scope:route"),
        RateScope::Tenant => format!("{head}:scope:tenant:{tenant_id}:{identity}"),
        RateScope::User => format!("{head}:scope:user:{tenant_id}:{identity}"),
        RateScope::Ip => format!("{head}:scope:ip:{identity}"),
    }
}

/// The limiter: one bucket per scope key.
#[derive(Debug, Clone, Default)]
pub struct RateLimiter {
    buckets: DashMap<String, Bucket>,
}

impl RateLimiter {
    /// An empty limiter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Consume `cost` tokens from the bucket named by `key`.
    #[must_use]
    pub fn check(&self, config: &RateLimitConfig, key: &str, cost: u32) -> RateDecision {
        match config.algorithm {
            RateAlgorithm::TokenBucket => self.bucket(config, key, cost, true),
            RateAlgorithm::SlidingWindow => self.bucket(config, key, cost, false),
        }
    }

    /// Forget every bucket (used by tests and by configuration invalidation).
    pub fn clear(&self) {
        self.buckets.clear();
    }

    /// Number of live buckets.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buckets.len()
    }

    /// Whether the limiter holds no bucket.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }

    fn bucket(
        &self,
        config: &RateLimitConfig,
        key: &str,
        cost: u32,
        continuous_refill: bool,
    ) -> RateDecision {
        let capacity = capacity_of(config);
        let cost_milli = u64::from(cost.max(1)).saturating_mul(MILLI);
        let now = Instant::now();

        let mut entry = self
            .buckets
            .entry(key.to_owned())
            .or_insert_with(|| Bucket::full(capacity));
        let bucket = entry.value_mut();
        let elapsed = now.saturating_duration_since(bucket.last_refill);

        let refill_per_ms = refill_per_millisecond(config.sustained.rate, config.sustained.window);
        if continuous_refill && elapsed.as_millis() > 0 {
            let replenished = elapsed.as_millis() as f64 * refill_per_ms;
            bucket.milli_tokens = (capacity.saturating_mul(MILLI) as f64)
                .min(bucket.milli_tokens as f64 + replenished)
                as u64;
            bucket.last_refill = now;
        } else if !continuous_refill
            && elapsed.as_millis() as u64 >= config.sustained.window.seconds().saturating_mul(1_000)
        {
            // Sliding window: the window's allowance is restored at once.
            bucket.milli_tokens = capacity.saturating_mul(MILLI);
            bucket.last_refill = now;
        }

        let tokens = bucket.milli_tokens;
        if tokens >= cost_milli {
            bucket.milli_tokens = tokens - cost_milli;
            return RateDecision {
                allowed: true,
                remaining: (bucket.milli_tokens / MILLI) as u32,
                reset_seconds: refill_seconds(bucket.milli_tokens, capacity, refill_per_ms),
                retry_after_seconds: 0,
                scope_key: key.to_owned(),
            };
        }

        let window_seconds = config.sustained.window.seconds();
        let deficit_ms = if refill_per_ms > 0.0 {
            (cost_milli as f64 / refill_per_ms) as u64
        } else {
            window_seconds.saturating_mul(1_000)
        };
        let elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        RateDecision {
            allowed: false,
            remaining: 0,
            reset_seconds: refill_seconds(bucket.milli_tokens, capacity, refill_per_ms),
            retry_after_seconds: (deficit_ms.saturating_sub(elapsed_ms) / 1_000)
                .max(1)
                .min(window_seconds.max(1)) as u32,
            scope_key: key.to_owned(),
        }
    }
}

/// Bucket capacity in tokens: `burst.capacity` when set, the sustained rate
/// otherwise, and never zero.
fn capacity_of(config: &RateLimitConfig) -> u64 {
    let sustained = u64::from(config.sustained.rate).max(1);
    config
        .burst
        .map(|burst| u64::from(burst.capacity))
        .unwrap_or(sustained)
        .max(1)
}

/// Milli-tokens replenished per millisecond.
fn refill_per_millisecond(rate: u32, window: RateWindow) -> f64 {
    // `rate` tokens per `window` seconds is `rate * MILLI` milli-tokens per
    // `window * 1000` milliseconds. Reading the window as milliseconds here —
    // the name of the local below used to say so — over-refills by 1000× and
    // turns "2 per minute" into "2 per 60 ms".
    let window_ms = f64::from(window.seconds().max(1) as u32) * 1_000.0;
    f64::from(rate.max(1)) * f64::from(MILLI as u32) / window_ms
}

/// Seconds until the bucket is full again, rounded up.
fn refill_seconds(tokens: u64, capacity: u64, refill_per_ms: f64) -> u32 {
    let deficit = capacity.saturating_mul(MILLI).saturating_sub(tokens) as f64;
    if refill_per_ms <= 0.0 {
        return 0;
    }
    ((deficit / refill_per_ms) / 1_000.0).ceil() as u32
}

#[cfg(test)]
#[path = "rate_limit_tests.rs"]
mod tests;
