//! Token-bucket rate limiting (ADR 0003).
//!
//! Buckets are keyed by `resource:scope:scope_id` and refill continuously.
//! Hierarchical merging uses `min()`: the effective sustained rate and burst
//! capacity are the strictest of the upstream and route limits, so a child can
//! never exceed an ancestor's ceiling.
//!
//! `strategy: queue` / `degrade` are accepted by the configuration surface but
//! fall back to `reject`: unbounded in-process queues would grow without bound
//! and a degraded response would silently change API semantics. The choice is
//! recorded here as the documented behaviour of this implementation.

use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;

use crate::domain::model::RateLimitConfig;

/// Outcome of a rate-limit check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateDecision {
    /// Request allowed; remaining tokens and the reset instant are reported.
    Allowed {
        /// Tokens left in the bucket.
        remaining: u32,
        /// Seconds until the bucket is fully replenished.
        reset_secs: u32,
    },
    /// Bucket exhausted; the retry hint is reported.
    Limited {
        /// Suggested `Retry-After` in seconds.
        retry_after_secs: u32,
    },
}

#[derive(Debug, Clone)]
struct BucketState {
    tokens: f64,
    last_refill: Instant,
}

/// Process-local token bucket store.
#[derive(Default)]
pub struct RateLimiter {
    buckets: DashMap<String, BucketState>,
}

impl RateLimiter {
    /// Creates an empty limiter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Consumes `cost` tokens from the bucket identified by `key`.
    #[must_use]
    pub fn consume(&self, key: &str, limit: &RateLimitConfig, cost: u32) -> RateDecision {
        let capacity = f64::from(limit.capacity());
        let per_second = limit.sustained.per_second();
        let cost = f64::from(cost.max(1));
        let now = Instant::now();

        let mut entry = self
            .buckets
            .entry(key.to_owned())
            .or_insert_with(|| BucketState {
                tokens: capacity,
                last_refill: now,
            });

        let elapsed = now.duration_since(entry.last_refill).as_secs_f64();
        entry.tokens = (entry.tokens + elapsed * per_second).min(capacity);
        entry.last_refill = now;

        if entry.tokens >= cost {
            entry.tokens -= cost;
            let remaining = entry.tokens.floor().max(0.0);
            let deficit = capacity - entry.tokens;
            let reset_secs = if per_second > 0.0 {
                (deficit / per_second).ceil().max(0.0)
            } else {
                0.0
            };
            RateDecision::Allowed {
                remaining: u32::try_from(remaining as u64).unwrap_or(u32::MAX),
                reset_secs: u32::try_from(reset_secs as u64).unwrap_or(u32::MAX),
            }
        } else {
            let wait = (cost - entry.tokens) / per_second.max(f64::MIN_POSITIVE);
            RateDecision::Limited {
                retry_after_secs: u32::try_from(wait.ceil().max(1.0) as u64).unwrap_or(u32::MAX),
            }
        }
    }
}

/// Merges an upstream and a route limit using `min()` per ADR 0003.
///
/// Returns `None` when neither side configures a limit.
#[must_use]
pub fn merge_limits(
    upstream: Option<&RateLimitConfig>,
    route: Option<&RateLimitConfig>,
) -> Option<RateLimitConfig> {
    match (upstream, route) {
        (None, None) => None,
        (Some(only), None) | (None, Some(only)) => Some(only.clone()),
        (Some(up), Some(rt)) => {
            let sustained = if up.sustained.per_second() <= rt.sustained.per_second() {
                up.sustained
            } else {
                rt.sustained
            };
            Some(RateLimitConfig {
                sharing: if up.sharing.is_enforce() {
                    up.sharing
                } else {
                    rt.sharing
                },
                algorithm: up.algorithm,
                sustained,
                burst: Some(up.capacity().min(rt.capacity())),
                scope: rt.scope,
                strategy: rt.strategy,
                cost: rt.cost.max(1),
                response_headers: up.response_headers || rt.response_headers,
                enabled: up.enabled && rt.enabled,
            })
        }
    }
}

/// Builds the counter key for a request.
#[must_use]
pub fn bucket_key(
    resource: &str,
    scope: crate::domain::model::RateScope,
    tenant: &uuid::Uuid,
    subject: &uuid::Uuid,
    ip: &str,
    route_id: &uuid::Uuid,
) -> String {
    let scope_part = match scope {
        crate::domain::model::RateScope::Global => "global".to_owned(),
        crate::domain::model::RateScope::Tenant => format!("tenant:{tenant}"),
        crate::domain::model::RateScope::User => format!("user:{subject}"),
        crate::domain::model::RateScope::Ip => format!("ip:{ip}"),
        crate::domain::model::RateScope::Route => format!("route:{route_id}"),
    };
    format!("oagw:ratelimit:{resource}:{scope_part}")
}

/// Shared handle used by the data plane.
#[derive(Clone, Default)]
pub struct SharedRateLimiter(pub Arc<RateLimiter>);

impl std::fmt::Debug for SharedRateLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SharedRateLimiter")
    }
}

impl SharedRateLimiter {
    /// Wraps a limiter.
    #[must_use]
    pub fn new(limiter: Arc<RateLimiter>) -> Self {
        Self(limiter)
    }

    /// Consumes `cost` tokens from the bucket identified by `key`.
    #[must_use]
    pub fn consume(&self, key: &str, limit: &RateLimitConfig, cost: u32) -> RateDecision {
        self.0.consume(key, limit, cost)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::model::{RateWindow, SustainedRate};

    fn limit(rate: u32, burst: u32) -> RateLimitConfig {
        RateLimitConfig {
            sustained: SustainedRate {
                rate,
                window: RateWindow::Second,
            },
            burst: Some(burst),
            ..RateLimitConfig::default()
        }
    }

    #[test]
    fn burst_up_to_capacity_then_limited() {
        let limiter = RateLimiter::new();
        let cfg = limit(1, 3);
        let key = bucket_key(
            "upstream",
            crate::domain::model::RateScope::Tenant,
            &uuid::Uuid::new_v4(),
            &uuid::Uuid::new_v4(),
            "127.0.0.1",
            &uuid::Uuid::new_v4(),
        );
        for _ in 0..3 {
            assert!(matches!(
                limiter.consume(&key, &cfg, 1),
                RateDecision::Allowed { .. }
            ));
        }
        match limiter.consume(&key, &cfg, 1) {
            RateDecision::Limited { retry_after_secs } => assert!(retry_after_secs >= 1),
            other => panic!("expected limited, got {other:?}"),
        }
    }

    #[test]
    fn hierarchical_merge_uses_strictest_side() {
        let up = limit(100, 500);
        let route = limit(10, 50);
        let merged = merge_limits(Some(&up), Some(&route)).expect("merged");
        assert_eq!(merged.sustained.rate, 10);
        assert_eq!(merged.capacity(), 50);

        let reversed = merge_limits(Some(&route), Some(&up)).expect("merged");
        assert_eq!(reversed.sustained.rate, 10);
        assert_eq!(reversed.capacity(), 50);

        assert!(merge_limits(None, None).is_none());
        let only_up = merge_limits(Some(&up), None).expect("only upstream");
        assert_eq!(only_up.sustained.rate, 100);
    }

    #[test]
    fn buckets_are_isolated_by_scope() {
        let limiter = RateLimiter::new();
        let cfg = limit(1, 1);
        let a = bucket_key(
            "upstream",
            crate::domain::model::RateScope::Tenant,
            &uuid::Uuid::new_v4(),
            &uuid::Uuid::new_v4(),
            "",
            &uuid::Uuid::new_v4(),
        );
        let b = bucket_key(
            "upstream",
            crate::domain::model::RateScope::Tenant,
            &uuid::Uuid::new_v4(),
            &uuid::Uuid::new_v4(),
            "",
            &uuid::Uuid::new_v4(),
        );
        assert!(matches!(
            limiter.consume(&a, &cfg, 1),
            RateDecision::Allowed { .. }
        ));
        assert!(matches!(
            limiter.consume(&a, &cfg, 1),
            RateDecision::Limited { .. }
        ));
        assert!(matches!(
            limiter.consume(&b, &cfg, 1),
            RateDecision::Allowed { .. }
        ));
    }
}
