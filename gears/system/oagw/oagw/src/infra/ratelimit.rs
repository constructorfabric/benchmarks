//! Rate limiting (ADR-0003): token buckets with a sliding-window variant, merged as `min` across
//! the ancestor chain and between route and upstream.

use std::time::Duration;

use dashmap::DashMap;

use crate::domain::model::{RateAlgorithm, RateLimitScope};

/// Effective limit after merging, in requests per window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveLimit {
    /// Requests allowed per [`Self::window`].
    pub rate: u32,
    /// Length of the window.
    pub window: Duration,
    /// Bucket capacity, i.e. the burst size.
    pub capacity: u32,
    /// Algorithm in force.
    pub algorithm: RateAlgorithm,
    /// Tokens consumed per request.
    pub cost: u32,
    /// Scope the counters are keyed by.
    pub scope: RateLimitScope,
}

impl EffectiveLimit {
    /// Merge two limits, keeping the stricter (`min`) one on each axis.
    #[must_use]
    pub fn merge(a: Option<&Self>, b: Option<&Self>) -> Option<Self> {
        match (a, b) {
            (None, None) => None,
            (Some(x), None) | (None, Some(x)) => Some(x.clone()),
            (Some(x), Some(y)) => {
                let rate = x.rate.min(y.rate);
                let capacity = x.capacity.min(y.capacity);
                Some(Self {
                    rate,
                    capacity,
                    window: x.window.min(y.window),
                    algorithm: if x.algorithm == RateAlgorithm::SlidingWindow
                        || y.algorithm == RateAlgorithm::SlidingWindow
                    {
                        RateAlgorithm::SlidingWindow
                    } else {
                        RateAlgorithm::TokenBucket
                    },
                    cost: x.cost.max(y.cost),
                    scope: if x.scope == RateLimitScope::Global {
                        x.scope
                    } else {
                        y.scope
                    },
                })
            }
        }
    }

    /// Seconds the caller should wait before retrying.
    #[must_use]
    pub fn retry_after_secs(&self) -> u64 {
        let per_second = f64::from(self.rate) / self.window.as_secs_f64().max(0.001);
        let need = f64::from(self.cost.max(1));
        (need / per_second).ceil().max(1.0) as u64
    }
}

/// Counters of one rate limit scope.
#[derive(Debug)]
struct Bucket {
    tokens: f64,
    capacity: f64,
    refill_per_sec: f64,
    last: std::time::Instant,
    window_hits: Vec<std::time::Instant>,
    algorithm: RateAlgorithm,
}

impl Bucket {
    fn try_take(&mut self, limit: &EffectiveLimit, now: std::time::Instant) -> bool {
        match limit.algorithm {
            RateAlgorithm::SlidingWindow => {
                let window = limit.window;
                self.window_hits.retain(|t| now.duration_since(*t) < window);
                let cost = usize::try_from(limit.cost).unwrap_or(0);
                let rate = usize::try_from(limit.rate).unwrap_or(0);
                if self.window_hits.len() + cost > rate {
                    return false;
                }
                for _ in 0..limit.cost {
                    self.window_hits.push(now);
                }
                true
            }
            RateAlgorithm::TokenBucket => {
                let elapsed = now.duration_since(self.last).as_secs_f64();
                self.last = now;
                self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
                if self.tokens >= f64::from(limit.cost) {
                    self.tokens -= f64::from(limit.cost);
                    true
                } else {
                    false
                }
            }
        }
    }
}

/// Registry of live rate limit counters.
#[derive(Debug, Default)]
pub struct RateLimiter {
    buckets: DashMap<String, Bucket>,
}

impl RateLimiter {
    /// Empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Consume `limit.cost` tokens from the bucket identified by `key`.
    ///
    /// Returns the seconds to wait when the limit is exhausted.
    #[must_use = "the rejection carries the retry delay and must not be dropped"]
    pub fn check(&self, key: &str, limit: &EffectiveLimit) -> Result<(), u64> {
        let now = std::time::Instant::now();
        let mut entry = self.buckets.entry(key.to_string()).or_insert_with(|| Bucket {
            tokens: f64::from(limit.capacity),
            capacity: f64::from(limit.capacity),
            refill_per_sec: f64::from(limit.rate) / limit.window.as_secs_f64().max(0.001),
            last: now,
            window_hits: Vec::new(),
            algorithm: limit.algorithm,
        });
        let bucket = entry.value_mut();
        if bucket.capacity < f64::from(limit.capacity) {
            bucket.capacity = f64::from(limit.capacity);
        }
        if bucket.algorithm != limit.algorithm {
            bucket.algorithm = limit.algorithm;
            bucket.window_hits.clear();
            bucket.tokens = f64::from(limit.capacity);
            bucket.refill_per_sec = f64::from(limit.rate) / limit.window.as_secs_f64().max(0.001);
        }
        if bucket.try_take(limit, now) {
            Ok(())
        } else {
            Err(limit.retry_after_secs())
        }
    }

    /// Scope key for a limit, per its configured scope.
    #[must_use]
    pub fn scope_key(
        limit: &EffectiveLimit,
        tenant_id: uuid::Uuid,
        route_id: uuid::Uuid,
        subject: &str,
        remote_ip: &str,
    ) -> String {
        match limit.scope {
            RateLimitScope::Global => "global".to_string(),
            RateLimitScope::Tenant => format!("tenant:{tenant_id}"),
            RateLimitScope::Route => format!("route:{route_id}"),
            RateLimitScope::User => format!("user:{tenant_id}:{subject}"),
            RateLimitScope::Ip => format!("ip:{remote_ip}"),
        }
    }
}

/// Derive the effective limit from a configuration.
#[must_use]
pub fn effective_limit(config: &crate::domain::model::RateLimitConfig) -> EffectiveLimit {
    let capacity = config
        .burst
        .as_ref()
        .map(|b| b.capacity)
        .unwrap_or(config.sustained.rate);
    EffectiveLimit {
        rate: config.sustained.rate,
        window: Duration::from_secs(config.sustained.window.secs()),
        capacity,
        algorithm: config.algorithm,
        cost: config.cost,
        scope: config.scope,
    }
}

#[cfg(test)]
#[path = "ratelimit_tests.rs"]
mod tests;
