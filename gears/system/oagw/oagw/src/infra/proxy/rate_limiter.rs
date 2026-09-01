// Created: 2026-08-29 by Constructor Tech
//! Rate-limit registry over [`TokenBucket`].
//!
//! Bucket keys follow the contract: `(scope, scope_key, upstream_or_route_id)`.
//! `token_bucket` uses the refill maths from `domain/rate_limit`; the
//! `sliding_window` algorithm is approximated with a fixed-window counter
//! (recorded as an MVP deviation — no ring-buffer history is kept).

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;

use crate::domain::model::{RateAlgorithm, RateLimitConfig};
use crate::domain::rate_limit::TokenBucket;

/// A counter key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RateKey {
    /// Scope name (`global`, `tenant`, `user`, `ip`, `route`).
    pub scope: String,
    /// Scope discriminator (tenant id, subject id, client ip, route id).
    pub scope_key: String,
    /// Owning route or upstream id.
    pub owner: String,
}

impl std::fmt::Display for RateKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}|{}|{}", self.scope, self.scope_key, self.owner)
    }
}

/// Outcome of a rate-limit check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateDecision {
    /// `false` when the request must be rejected with `429`.
    pub allowed: bool,
    /// Configured capacity (for `X-RateLimit-Limit`).
    pub limit: u64,
    /// Tokens left in the bucket.
    pub remaining: u64,
    /// Unix epoch seconds at which the bucket is full again.
    pub reset_epoch: u64,
    /// Seconds until `cost` tokens are available again.
    pub retry_after: u64,
}

/// Bucket store keyed by [`RateKey`].
#[derive(Debug, Default)]
pub struct RateLimiterRegistry {
    buckets: Mutex<HashMap<String, RateBucket>>,
}

#[derive(Debug)]
enum RateBucket {
    Token(TokenBucket),
    Window {
        window_started: std::time::Instant,
        window_seconds: u64,
        count: u64,
        capacity: u64,
    },
}

impl Default for RateBucket {
    fn default() -> Self {
        Self::Token(TokenBucket::new(1.0, 1.0))
    }
}

impl RateLimiterRegistry {
    /// Empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply the effective rate-limit configuration for one key.
    ///
    /// Returns `None` when no rate limit is configured (the request proceeds
    /// with no `X-RateLimit-*` headers).
    #[must_use]
    pub fn check(&self, key: &RateKey, config: &RateLimitConfig) -> Option<RateDecision> {
        let capacity = config.capacity().max(1) as u64;
        // Merging may pair a large per-request cost with an ancestor's smaller
        // burst; clamping keeps every request affordable for the bucket.
        let cost = u64::from(config.cost()).min(capacity);
        let window = config.sustained.window.seconds();
        let rate = u64::from(config.sustained.rate);

        let mut buckets = self.buckets.lock();
        let bucket = buckets
            .entry(key.to_string())
            .or_insert_with(|| match config.algorithm {
                RateAlgorithm::TokenBucket => RateBucket::Token(TokenBucket::new(
                    capacity as f64,
                    crate::domain::rate_limit::refill_rate(config.sustained.rate, window),
                )),
                RateAlgorithm::SlidingWindow => RateBucket::Window {
                    window_started: std::time::Instant::now(),
                    window_seconds: window,
                    count: 0,
                    capacity: rate.max(1),
                },
            });

        Some(match bucket {
            RateBucket::Token(token_bucket) => {
                let allowed =
                    token_bucket.try_acquire(f64::from(u32::try_from(cost).unwrap_or(u32::MAX)));
                RateDecision {
                    allowed,
                    limit: capacity,
                    remaining: token_bucket.current_tokens().max(0.0) as u64,
                    reset_epoch: token_bucket.epoch_seconds_until_full(),
                    retry_after: token_bucket
                        .seconds_until_tokens(f64::from(u32::try_from(cost).unwrap_or(u32::MAX))),
                }
            }
            RateBucket::Window {
                window_started,
                window_seconds,
                count,
                capacity: limit,
            } => {
                let window_secs = (*window_seconds).max(1);
                if window_started.elapsed().as_secs() >= window_secs {
                    *window_started = std::time::Instant::now();
                    *count = 0;
                }
                let allowed = *count < *limit;
                if allowed {
                    *count = count.saturating_add(cost.max(1));
                }
                let elapsed = window_started.elapsed().as_secs();
                RateDecision {
                    allowed,
                    limit: capacity,
                    remaining: (*limit).saturating_sub(*count),
                    reset_epoch: epoch_now() + window_secs.saturating_sub(elapsed),
                    retry_after: if allowed {
                        0
                    } else {
                        window_secs.saturating_sub(elapsed).max(1)
                    },
                }
            }
        })
    }

    /// Drop every bucket (test helper).
    pub fn reset(&self) {
        self.buckets.lock().clear();
    }
}

/// Current unix epoch in seconds.
fn epoch_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |value| value.as_secs())
}

/// Per-subject and per-IP scopes need an identity to key on; without one the
/// request falls back to its tenant so it cannot share a bucket with the world.
#[must_use]
pub fn keyed_scope(value: &str, tenant_id: &str) -> String {
    if value.is_empty() {
        format!("tenant:{tenant_id}")
    } else {
        value.to_owned()
    }
}

/// Build the registry key for a resolved route / upstream pair.
#[must_use]
pub fn rate_key(config: &RateLimitConfig, scope_key: &str, owner: &str) -> RateKey {
    RateKey {
        scope: match config.scope {
            crate::domain::model::RateScope::Global => "global".to_owned(),
            crate::domain::model::RateScope::Tenant => "tenant".to_owned(),
            crate::domain::model::RateScope::User => "user".to_owned(),
            crate::domain::model::RateScope::Ip => "ip".to_owned(),
            crate::domain::model::RateScope::Route => "route".to_owned(),
        },
        scope_key: match config.scope {
            crate::domain::model::RateScope::Global => "global".to_owned(),
            _ => scope_key.to_owned(),
        },
        owner: match config.scope {
            crate::domain::model::RateScope::Global => "global".to_owned(),
            _ => owner.to_owned(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{Burst, Sustained};

    fn config() -> RateLimitConfig {
        RateLimitConfig {
            sharing: crate::domain::model::Sharing::Private,
            algorithm: RateAlgorithm::TokenBucket,
            sustained: Sustained {
                rate: 2,
                window: crate::domain::model::RateWindow::Second,
            },
            burst: Some(Burst { capacity: 2 }),
            budget: None,
            scope: crate::domain::model::RateScope::Tenant,
            strategy: crate::domain::model::RateStrategy::Reject,
            cost: None,
            response_headers: None,
        }
    }

    #[test]
    fn key_includes_scope_and_owner() {
        let cfg = config();
        let key = rate_key(&cfg, "tenant-1", "route-1");
        assert_eq!(key.scope, "tenant");
        assert_eq!(key.scope_key, "tenant-1");
        assert_eq!(key.owner, "route-1");
    }

    #[test]
    fn exhausts_then_refills() {
        let registry = RateLimiterRegistry::new();
        let cfg = config();
        let key = rate_key(&cfg, "tenant-1", "route-1");
        let first = registry.check(&key, &cfg).expect("decision");
        assert!(first.allowed);
        let second = registry.check(&key, &cfg).expect("decision");
        assert!(second.allowed);
        let third = registry.check(&key, &cfg).expect("decision");
        assert!(!third.allowed);
        assert!(third.retry_after > 0 || third.reset_epoch >= epoch_now());
    }

    #[test]
    fn sliding_window_counts_requests() {
        let registry = RateLimiterRegistry::new();
        let mut cfg = config();
        cfg.algorithm = RateAlgorithm::SlidingWindow;
        cfg.sustained = Sustained {
            rate: 1,
            window: crate::domain::model::RateWindow::Minute,
        };
        let key = rate_key(&cfg, "t", "r");
        assert!(registry.check(&key, &cfg).expect("d").allowed);
        assert!(!registry.check(&key, &cfg).expect("d").allowed);
    }

    #[test]
    fn cost_is_clamped_to_the_bucket_capacity() {
        let registry = RateLimiterRegistry::new();
        let mut cfg = config();
        cfg.cost = Some(10); // burst capacity is 2
        let key = rate_key(&cfg, "tenant-1", "route-1");
        let first = registry.check(&key, &cfg).expect("decision");
        assert!(
            first.allowed,
            "a clamped cost of 2 still fits a capacity of 2"
        );
        assert!(!registry.check(&key, &cfg).expect("decision").allowed);
    }

    #[test]
    fn missing_identity_falls_back_to_the_tenant() {
        assert_eq!(keyed_scope("", "tenant-7"), "tenant:tenant-7");
        assert_eq!(keyed_scope("203.0.113.9", "tenant-7"), "203.0.113.9");
    }

    #[test]
    fn global_scope_is_one_bucket() {
        let mut cfg = config();
        cfg.scope = crate::domain::model::RateScope::Global;
        let key = rate_key(&cfg, "tenant-1", "route-1");
        assert_eq!(key.scope_key, "global");
        assert_eq!(key.owner, "global");
    }
}
