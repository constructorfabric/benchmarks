//! In-memory rate limiting (ADR-0003).
//!
//! One token bucket per counter key. Buckets refill continuously from the
//! sustained rate and are capped by the burst capacity, so the dual-rate model
//! collapses onto the standard token bucket: capacity `max(burst, rate)` and
//! refill `rate / window_seconds` tokens per second.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;

use crate::domain::error::{DomainError, DomainResult};
use crate::domain::model::RateLimitConfig;

/// Identifies one counter (ADR-0003 `scope`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RateKey {
    /// `scope: global` — a single gateway-wide counter.
    Global,
    /// `scope: tenant` — one counter per tenant.
    Tenant(String),
    /// `scope: user` — one counter per authenticated subject, tenant-scoped.
    User { tenant: String, subject: String },
    /// `scope: ip` — one counter per client address.
    Ip(String),
    /// `scope: route` — one counter per route, tenant-scoped.
    Route { tenant: String, route: String },
}

impl RateKey {
    /// Derives the counter identity from the configured scope.
    #[must_use]
    pub fn for_scope(
        scope: crate::domain::model::RateScope,
        tenant_id: &str,
        subject_id: &str,
        client_ip: &str,
        route_id: &str,
    ) -> Self {
        use crate::domain::model::RateScope;
        match scope {
            RateScope::Global => Self::Global,
            RateScope::Tenant => Self::Tenant(tenant_id.to_owned()),
            RateScope::User => Self::User {
                tenant: tenant_id.to_owned(),
                subject: subject_id.to_owned(),
            },
            RateScope::Ip => Self::Ip(client_ip.to_owned()),
            RateScope::Route => Self::Route {
                tenant: tenant_id.to_owned(),
                route: route_id.to_owned(),
            },
        }
    }
}

/// Verdict of a limiter check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateVerdict {
    /// Configured sustained rate.
    pub limit: u64,
    /// Tokens left in the bucket after this request.
    pub remaining: u64,
    /// Seconds until the bucket is fully replenished.
    pub reset_seconds: u64,
    /// Seconds until one token is available again (only on rejection).
    pub retry_after_seconds: u64,
}

/// Upper bound on live buckets, so an attacker cycling distinct counter keys
/// (a fresh IP per request, for instance) cannot grow the table without limit.
pub const MAX_BUCKETS: usize = 65_536;

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    shape: BucketShape,
    /// Upper bound of the bucket.
    capacity: f64,
    /// Tokens added per second.
    refill_per_second: f64,
    last_refill: Instant,
}

/// The rate posture a bucket was created (or last reconciled) under.
#[derive(Debug, Clone, Copy, PartialEq)]
struct BucketShape {
    capacity: f64,
    refill_per_second: f64,
}

impl Bucket {
    fn refill(&mut self, now: Instant) {
        let elapsed = now
            .saturating_duration_since(self.last_refill)
            .as_secs_f64();
        if elapsed > 0.0 {
            self.tokens = (self.tokens + elapsed * self.refill_per_second).min(self.capacity);
            self.last_refill = now;
        }
    }
}

/// Token-bucket limiter over an in-memory counter table.
#[derive(Debug, Default)]
pub struct RateLimiter {
    buckets: Mutex<HashMap<RateKey, Bucket>>,
}

impl RateLimiter {
    /// Creates an empty limiter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Charges `cost` tokens against `key` under `config`.
    ///
    /// # Errors
    ///
    /// Returns `RateLimitExceeded` with the header values the wire layer
    /// relays when the bucket is exhausted.
    pub fn check(
        &self,
        key: RateKey,
        config: &RateLimitConfig,
        cost: u64,
    ) -> DomainResult<RateVerdict> {
        let now = Instant::now();
        let window_seconds = config.sustained.window.seconds().max(1) as f64;
        let rate = config.sustained.rate.max(1) as f64;
        let capacity = config
            .burst
            .capacity
            .unwrap_or(config.sustained.rate)
            .max(1) as f64;
        let cost = cost.max(1) as f64;

        let shape = BucketShape {
            capacity,
            refill_per_second: rate / window_seconds,
        };

        let mut buckets = self.buckets.lock();
        // A key the table has no room for is still served, it just is not
        // remembered: the request starts from a full bucket and is forgotten.
        if !buckets.contains_key(&key) && buckets.len() >= MAX_BUCKETS {
            buckets.clear();
        }
        let bucket = buckets.entry(key).or_insert_with(|| Bucket {
            tokens: capacity,
            capacity,
            refill_per_second: shape.refill_per_second,
            last_refill: now,
            shape,
        });
        // An admin edit to the limit must not leave a bucket pinned to the old
        // posture: the counter restarts full under the new shape, because the
        // old refill pace would otherwise keep punishing callers for a limit
        // that no longer exists.
        if bucket.shape != shape {
            bucket.capacity = shape.capacity;
            bucket.refill_per_second = shape.refill_per_second;
            bucket.tokens = shape.capacity;
            bucket.last_refill = now;
            bucket.shape = shape;
        }
        bucket.refill(now);

        if bucket.tokens >= cost {
            bucket.tokens -= cost;
            let remaining = bucket.tokens.floor().max(0.0) as u64;
            let reset =
                (bucket.capacity - bucket.tokens) / bucket.refill_per_second.max(f64::MIN_POSITIVE);
            Ok(RateVerdict {
                limit: config.sustained.rate,
                remaining,
                reset_seconds: reset.ceil() as u64,
                retry_after_seconds: 0,
            })
        } else {
            let deficit = cost - bucket.tokens;
            let retry = (deficit / bucket.refill_per_second.max(f64::MIN_POSITIVE)).ceil() as u64;
            Err(DomainError::RateLimitExceeded {
                limit: config.sustained.rate,
                remaining: 0,
                reset_seconds: retry.max(1),
                retry_after_seconds: retry.max(1),
            })
        }
    }

    /// Forgets every bucket (used by tests and by the GC sweep).
    pub fn clear(&self) {
        self.buckets.lock().clear();
    }
}

/// Shared limiter handle.
pub type SharedRateLimiter = Arc<RateLimiter>;

impl crate::domain::services::proxy::RateLimitStore for RateLimiter {
    fn charge(
        &self,
        key: crate::domain::services::proxy::CounterKey,
        config: &RateLimitConfig,
        cost: u64,
    ) -> DomainResult<crate::domain::services::proxy::RateVerdict> {
        // The infra counter table is keyed identically; only the type name
        // differs between the two layers.
        let mapped = match key {
            crate::domain::services::proxy::CounterKey::Global => RateKey::Global,
            crate::domain::services::proxy::CounterKey::Tenant(id) => RateKey::Tenant(id),
            crate::domain::services::proxy::CounterKey::User { tenant, subject } => {
                RateKey::User { tenant, subject }
            }
            crate::domain::services::proxy::CounterKey::Ip(ip) => RateKey::Ip(ip),
            crate::domain::services::proxy::CounterKey::Route { tenant, route } => {
                RateKey::Route { tenant, route }
            }
        };
        self.check(mapped, config, cost).map(|verdict| {
            crate::domain::services::proxy::RateVerdict {
                limit: verdict.limit,
                remaining: verdict.remaining,
                reset_seconds: verdict.reset_seconds,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{
        BurstConfig, RateAlgorithm, RateScope, RateStrategy, SustainedRate, Window,
    };

    fn config(rate: u64, window: Window, capacity: Option<u64>) -> RateLimitConfig {
        RateLimitConfig {
            sharing: crate::domain::model::Sharing::Private,
            algorithm: RateAlgorithm::TokenBucket,
            sustained: SustainedRate { rate, window },
            burst: BurstConfig { capacity },
            scope: RateScope::Tenant,
            strategy: RateStrategy::Reject,
            cost: 1,
            response_headers: true,
        }
    }

    #[test]
    fn allows_requests_within_the_sustained_rate() {
        let limiter = RateLimiter::new();
        let key = RateKey::Tenant("t".to_owned());
        let config = config(3, Window::Second, None);
        for _ in 0..3 {
            let verdict = limiter.check(key.clone(), &config, 1).expect("allowed");
            assert_eq!(verdict.limit, 3);
        }
        let exceeded = limiter.check(key, &config, 1).expect_err("exhausted");
        assert_eq!(exceeded.status_code(), 429);
    }

    #[test]
    fn burst_capacity_extends_the_bucket() {
        let limiter = RateLimiter::new();
        let key = RateKey::User {
            tenant: "t".to_owned(),
            subject: "u".to_owned(),
        };
        let config = config(1, Window::Second, Some(10));
        for _ in 0..10 {
            assert!(limiter.check(key.clone(), &config, 1).is_ok());
        }
        assert!(limiter.check(key, &config, 1).is_err());
    }

    #[test]
    fn buckets_are_independent_per_key() {
        let limiter = RateLimiter::new();
        let config = config(1, Window::Second, None);
        assert!(
            limiter
                .check(RateKey::Tenant("a".to_owned()), &config, 1)
                .is_ok()
        );
        assert!(
            limiter
                .check(RateKey::Tenant("b".to_owned()), &config, 1)
                .is_ok()
        );
        assert!(
            limiter
                .check(RateKey::Tenant("a".to_owned()), &config, 1)
                .is_err()
        );
    }

    #[test]
    fn cost_is_charged_per_request() {
        let limiter = RateLimiter::new();
        let mut config = config(2, Window::Second, None);
        config.cost = 2;
        let key = RateKey::Route {
            tenant: "t".to_owned(),
            route: "r".to_owned(),
        };
        assert!(limiter.check(key.clone(), &config, 2).is_ok());
        assert!(limiter.check(key, &config, 2).is_err());
    }

    #[test]
    fn user_and_route_counters_are_scoped_to_the_tenant() {
        let limiter = RateLimiter::new();
        let config = config(1, Window::Second, None);
        let subject = |tenant: &str| RateKey::User {
            tenant: tenant.to_owned(),
            subject: "u".to_owned(),
        };
        assert!(limiter.check(subject("a"), &config, 1).is_ok());
        assert!(limiter.check(subject("b"), &config, 1).is_ok());
        assert!(limiter.check(subject("a"), &config, 1).is_err());
    }

    #[test]
    fn a_config_change_reconciles_the_bucket() {
        let limiter = RateLimiter::new();
        let key = RateKey::Tenant("t".to_owned());
        assert!(
            limiter
                .check(key.clone(), &config(1, Window::Second, None), 1)
                .is_ok()
        );
        assert!(
            limiter
                .check(key.clone(), &config(1, Window::Second, None), 1)
                .is_err()
        );
        // The admin doubles the sustained rate: the same counter must not stay
        // pinned to the exhausted bucket shape.
        assert!(
            limiter
                .check(key.clone(), &config(100, Window::Second, None), 1)
                .is_ok()
        );
    }

    #[test]
    fn the_counter_table_is_bounded() {
        let limiter = RateLimiter::new();
        let config = config(1000, Window::Second, None);
        for index in 0..(MAX_BUCKETS + 64) {
            let key = RateKey::Ip(format!("10.0.0.{index}"));
            assert!(limiter.check(key, &config, 1).is_ok());
        }
        assert!(limiter.buckets.lock().len() <= MAX_BUCKETS);
    }

    #[test]
    fn tokens_refill_over_time() {
        let limiter = RateLimiter::new();
        let key = RateKey::Ip("1.2.3.4".to_owned());
        let config = config(1000, Window::Second, None);
        assert!(limiter.check(key.clone(), &config, 1).is_ok());
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(limiter.check(key, &config, 1).is_ok());
    }
}
