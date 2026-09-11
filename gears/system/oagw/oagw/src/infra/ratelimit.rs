//! Token-bucket rate limiting (`ADR/0003`).
//!
//! One bucket per `(scope, upstream, route, subject)`; tokens replenish at the
//! effective sustained rate and the burst capacity is the bucket size.

use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;
use parking_lot::Mutex;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model::{RateLimit, RateScope};

/// A single token bucket.
#[derive(Debug)]
struct Bucket {
    /// Tokens currently available.
    tokens: f64,
    /// Instant of the last refill.
    last_refill: Instant,
}

/// Key identifying one bucket.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BucketKey {
    upstream_id: Uuid,
    route_id: Option<Uuid>,
    scope: RateScope,
    subject: String,
}

/// Rate limiter over per-scope token buckets.
#[derive(Debug, Default)]
pub struct RateLimiter {
    buckets: DashMap<BucketKey, Arc<Mutex<Bucket>>>,
}

/// Outcome of an accepted request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimitDecision {
    /// Effective sustained rate, tokens per window.
    pub limit: u32,
    /// Window of the effective rate, in seconds.
    pub window_secs: u64,
    /// Tokens left in the bucket.
    pub remaining: u32,
    /// Seconds until the bucket is full again.
    pub reset_secs: u64,
}

impl RateLimiter {
    /// Creates an empty limiter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn key(
        &self,
        upstream_id: Uuid,
        route_id: Option<Uuid>,
        scope: RateScope,
        subject: &str,
    ) -> BucketKey {
        BucketKey {
            upstream_id,
            route_id,
            scope,
            // Global and per-route buckets are shared by every caller; the
            // other scopes key on the caller-provided subject.
            subject: match scope {
                RateScope::Global | RateScope::Route => String::new(),
                _ => subject.to_owned(),
            },
        }
    }

    /// Consumes `cost` tokens, refilling first.
    ///
    /// # Errors
    /// [`DomainError::RateLimitExceeded`] when the bucket cannot pay for the
    /// request; the error carries `Retry-After` guidance.
    pub fn check(
        &self,
        limit: &RateLimit,
        upstream_id: Uuid,
        route_id: Option<Uuid>,
        subject: &str,
        now: Instant,
    ) -> Result<RateLimitDecision, DomainError> {
        let key = self.key(upstream_id, route_id, limit.scope, subject);
        let capacity = f64::from(limit.capacity());
        let rate = limit.rate_per_second();
        let cost = f64::from(limit.cost.max(1));

        let bucket = self
            .buckets
            .entry(key)
            .or_insert_with(|| {
                Arc::new(Mutex::new(Bucket {
                    tokens: capacity,
                    last_refill: now,
                }))
            })
            .clone();
        let mut guard = bucket.lock();
        let elapsed = now.duration_since(guard.last_refill).as_secs_f64();
        guard.tokens = (guard.tokens + rate * elapsed).min(capacity);
        guard.last_refill = now;

        if guard.tokens < cost {
            let retry_after_secs: u64 = if rate > 0.0 {
                ((cost - guard.tokens) / rate).ceil() as u64
            } else {
                limit.sustained.window.seconds()
            };
            return Err(DomainError::RateLimitExceeded {
                limit: limit.sustained.rate,
                window_secs: limit.sustained.window.seconds(),
                retry_after_secs: retry_after_secs.max(1),
            });
        }

        guard.tokens -= cost;
        let remaining = guard.tokens;
        Ok(RateLimitDecision {
            limit: limit.sustained.rate,
            window_secs: limit.sustained.window.seconds(),
            remaining: remaining.floor().max(0.0) as u32,
            reset_secs: refill_seconds(rate, remaining, capacity),
        })
    }
}

/// Seconds until the bucket reaches `capacity` again.
fn refill_seconds(rate: f64, remaining: f64, capacity: f64) -> u64 {
    if rate <= 0.0 {
        return 0;
    }
    ((capacity - remaining) / rate).ceil() as u64
}

/// Chooses the effective limit for a request: the route's when it declares
/// one, otherwise the upstream's.
#[must_use]
pub fn effective_limit<'a>(
    upstream: Option<&'a RateLimit>,
    route: Option<&'a RateLimit>,
) -> Option<&'a RateLimit> {
    route.or(upstream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{BurstConfig, RateWindow, SustainedRate};

    fn limit(rate: u32, window: RateWindow, capacity: Option<u32>) -> RateLimit {
        RateLimit {
            sustained: SustainedRate { rate, window },
            burst: BurstConfig { capacity },
            ..RateLimit::default()
        }
    }

    #[test]
    fn the_route_limit_wins_over_the_upstream_limit() {
        let upstream = limit(10, RateWindow::Second, None);
        let route = limit(1, RateWindow::Second, None);
        assert_eq!(effective_limit(Some(&upstream), Some(&route)), Some(&route));
        assert_eq!(effective_limit(Some(&upstream), None), Some(&upstream));
        assert!(effective_limit(None, None).is_none());
    }

    #[test]
    fn the_bucket_rejects_when_it_cannot_pay() {
        let limiter = RateLimiter::new();
        let config = limit(1, RateWindow::Hour, Some(1));
        let upstream = Uuid::new_v4();
        let now = Instant::now();
        assert!(
            limiter
                .check(&config, upstream, None, "subject", now)
                .is_ok()
        );
        let err = limiter
            .check(&config, upstream, None, "subject", now)
            .expect_err("denied");
        assert_eq!(err.status(), 429);
        assert_eq!(
            err,
            DomainError::RateLimitExceeded {
                limit: 1,
                window_secs: 3600,
                retry_after_secs: 3600
            }
        );
    }

    #[test]
    fn buckets_are_isolated_per_subject() {
        let limiter = RateLimiter::new();
        let config = limit(1, RateWindow::Hour, Some(1));
        let upstream = Uuid::new_v4();
        let now = Instant::now();
        assert!(limiter.check(&config, upstream, None, "a", now).is_ok());
        assert!(limiter.check(&config, upstream, None, "b", now).is_ok());
        assert!(limiter.check(&config, upstream, None, "a", now).is_err());
    }

    #[test]
    fn the_global_scope_shares_one_bucket() {
        let limiter = RateLimiter::new();
        let mut config = limit(1, RateWindow::Hour, Some(1));
        config.scope = RateScope::Global;
        let upstream = Uuid::new_v4();
        let now = Instant::now();
        assert!(limiter.check(&config, upstream, None, "a", now).is_ok());
        assert!(limiter.check(&config, upstream, None, "b", now).is_err());
    }

    #[test]
    fn a_full_bucket_reports_its_reset_horizon() {
        let limiter = RateLimiter::new();
        let config = limit(120, RateWindow::Minute, Some(120));
        let decision = limiter
            .check(&config, Uuid::new_v4(), None, "a", Instant::now())
            .expect("allowed");
        assert_eq!(decision.limit, 120);
        assert_eq!(decision.window_secs, 60);
        assert_eq!(decision.remaining, 119);
    }

    #[test]
    fn the_rate_is_normalized_per_second() {
        let config = limit(120, RateWindow::Minute, None);
        assert!((config.rate_per_second() - 2.0).abs() < f64::EPSILON);
        assert_eq!(config.capacity(), 120);
    }

    #[test]
    fn refill_seconds_accounts_for_the_rate() {
        assert_eq!(refill_seconds(2.0, 0.0, 2.0), 1);
        assert_eq!(refill_seconds(0.0, 0.0, 2.0), 0);
        assert_eq!(refill_seconds(1.0, 2.0, 2.0), 0);
    }

    #[test]
    fn limiters_do_not_share_buckets() {
        let a = RateLimiter::new();
        let b = RateLimiter::new();
        let config = limit(1, RateWindow::Hour, Some(1));
        let now = Instant::now();
        assert!(a.check(&config, Uuid::new_v4(), None, "x", now).is_ok());
        assert!(b.check(&config, Uuid::new_v4(), None, "x", now).is_ok());
    }
}
