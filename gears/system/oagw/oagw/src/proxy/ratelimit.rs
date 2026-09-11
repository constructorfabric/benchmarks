//! Rate limiting: token bucket / sliding window with hierarchical merge.
//!
//! See ADR-0003: limits are merged across the config hierarchy with `min()`
//! for enforced ancestors, and counters are scoped per `scope` value.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::domain::model::{RateAlgorithm, RateLimit, RateScope, RateStrategy};

/// `X-RateLimit-*` headers returned on the response.
#[derive(Debug, Clone, Copy)]
pub struct RateLimitHeaders {
    /// Configured limit.
    pub limit: u32,
    /// Tokens remaining.
    pub remaining: u32,
    /// Seconds until the bucket refills.
    pub reset: u64,
}

/// Outcome of a rate-limit check.
#[derive(Debug, Clone)]
pub enum RateLimitVerdict {
    /// The request is allowed.
    Allowed(Option<RateLimitHeaders>),
    /// The request is rejected, with retry guidance.
    Limited {
        /// Headers to attach to the 429.
        headers: Option<RateLimitHeaders>,
        /// `Retry-After` seconds.
        retry_after: u64,
    },
}

/// A single token bucket.
#[derive(Debug, Clone)]
struct Bucket {
    tokens: f64,
    last: Instant,
}

impl Bucket {
    fn new(capacity: u32) -> Self {
        Self {
            tokens: f64::from(capacity),
            last: Instant::now(),
        }
    }
}

/// Rate limiter holding one bucket per scope key.
#[derive(Default)]
pub struct RateLimiter {
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl RateLimiter {
    /// Empty limiter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Consumes `cost` tokens from the named bucket.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn check(
        &self,
        key: &str,
        limit: &RateLimit,
        refill_per_second: f64,
        capacity: u32,
        cost: u32,
        now: Instant,
    ) -> RateLimitVerdict {
        let mut buckets = self.buckets.lock();
        let bucket = buckets
            .entry(key.to_owned())
            .or_insert_with(|| Bucket::new(capacity));
        let elapsed = now.duration_since(bucket.last).as_secs_f64();
        bucket.last = now;
        bucket.tokens = (bucket.tokens + elapsed * refill_per_second).min(f64::from(capacity));

        let cost = f64::from(cost.max(1));
        let headers = if limit.response_headers {
            Some(RateLimitHeaders {
                limit: capacity,
                remaining: bucket.tokens.floor().max(0.0) as u32,
                reset: if refill_per_second > 0.0 {
                    (capacity as f64 - bucket.tokens).max(0.0) / refill_per_second
                } else {
                    0.0
                } as u64,
            })
        } else {
            None
        };

        if bucket.tokens >= cost {
            bucket.tokens -= cost;
            RateLimitVerdict::Allowed(headers)
        } else {
            let deficit = cost - bucket.tokens;
            let retry_after = if refill_per_second > 0.0 {
                (deficit / refill_per_second).ceil() as u64
            } else {
                1
            };
            RateLimitVerdict::Limited {
                headers,
                retry_after: retry_after.max(1),
            }
        }
    }
}

/// Scopes a counter key.
#[must_use]
pub fn scope_key(
    scope: RateScope,
    tenant_id: uuid::Uuid,
    subject_id: uuid::Uuid,
    client_ip: &str,
    route_id: &str,
) -> String {
    match scope {
        RateScope::Global => "global".to_owned(),
        RateScope::Tenant => format!("tenant:{tenant_id}"),
        RateScope::User => format!("user:{tenant_id}:{subject_id}"),
        RateScope::Ip => format!("ip:{client_ip}"),
        RateScope::Route => format!("route:{route_id}"),
    }
}

/// Merged rate-limit configuration across the hierarchy.
#[derive(Debug, Clone)]
pub struct EffectiveRateLimit {
    /// Merged limit.
    pub limit: RateLimit,
    /// Key identifying the counter.
    pub key: String,
}

/// Merges candidate limits with `min()` per component.
#[must_use]
pub fn merge_limits(limits: &[RateLimit], algorithm_default: RateAlgorithm) -> Option<RateLimit> {
    let mut it = limits.iter();
    let first = it.next()?.clone();
    let mut merged = first;
    merged.algorithm = algorithm_default;
    for next in it {
        if next.sustained.rate < merged.sustained.rate {
            merged.sustained.rate = next.sustained.rate;
        }
        let capacity = merged.capacity().min(next.capacity());
        merged.burst = Some(crate::domain::model::RateBurst { capacity });
        if next.cost > merged.cost {
            merged.cost = next.cost;
        }
    }
    Some(merged)
}

/// Whether the request should be queued rather than rejected.
#[must_use]
pub fn strategy_queues(limit: &RateLimit) -> bool {
    limit.strategy == RateStrategy::Queue
}

/// Default client identity when the peer address is unknown.
pub const UNKNOWN_CLIENT_IP: &str = "unknown";

/// Convenience constructor for a limiter shared across the process.
#[must_use]
pub fn shared() -> Arc<RateLimiter> {
    Arc::new(RateLimiter::new())
}

/// Window length of a rate limit in seconds.
#[must_use]
pub fn window_seconds(limit: &RateLimit) -> u64 {
    limit.sustained.window.seconds()
}

/// Whether the limit is a token bucket.
#[must_use]
pub fn is_token_bucket(limit: &RateLimit) -> bool {
    limit.algorithm == RateAlgorithm::TokenBucket
}

/// Formats the `X-RateLimit-*` headers for a verdict.
#[must_use]
pub fn headers_for(verdict: &RateLimitVerdict) -> Vec<(&'static str, String)> {
    match verdict {
        RateLimitVerdict::Allowed(Some(h)) => vec![
            ("x-ratelimit-limit", h.limit.to_string()),
            ("x-ratelimit-remaining", h.remaining.to_string()),
            ("x-ratelimit-reset", h.reset.to_string()),
        ],
        RateLimitVerdict::Limited {
            headers: Some(h), ..
        } => vec![
            ("x-ratelimit-limit", h.limit.to_string()),
            ("x-ratelimit-remaining", "0".to_owned()),
            ("x-ratelimit-reset", h.reset.to_string()),
        ],
        _ => Vec::new(),
    }
}

/// Duration of a rate-limit window as a `Duration`.
#[must_use]
pub fn window_duration(limit: &RateLimit) -> Duration {
    Duration::from_secs(limit.sustained.window.seconds())
}
