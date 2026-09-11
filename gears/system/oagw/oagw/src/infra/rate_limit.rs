//! Per-instance rate limiting, owned by the Data Plane.
//!
//! `cpt-cf-oagw-adr-rate-limiting` chose a token bucket with dual-rate
//! configuration (sustained rate + burst capacity) and an optional sliding
//! window, held locally with no distributed coordination for the MVP. Keys are
//! shaped `…:{resource_type}:{resource_id}:{scope}:{scope_id}:{window}` so a
//! deleted resource's counters can be dropped by prefix.

use std::time::{Duration, Instant};

use dashmap::DashMap;
use parking_lot::Mutex;

use crate::domain::model::{RateAlgorithm, RateLimitConfig, RateScope, RateStrategy};

/// Prefix shared by every counter key.
const KEY_PREFIX: &str = "oagw:ratelimit";

/// Outcome of a limit check, including the numbers the `X-RateLimit-*`
/// response headers report.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateDecision {
    /// Whether the request may proceed.
    pub allowed: bool,
    /// Configured bucket capacity.
    pub limit: u64,
    /// Tokens left after this check.
    pub remaining: u64,
    /// Seconds until the bucket is full again.
    pub reset_after_secs: u64,
    /// Seconds a rejected caller should wait, at least one.
    pub retry_after_secs: u64,
    /// Fraction of the bucket consumed, in `[0.0, 1.0]`.
    pub usage_ratio: f64,
}

/// A token bucket, or the two-window approximation used for
/// `sliding_window`.
#[derive(Debug)]
struct Bucket {
    tokens: f64,
    last_update: Instant,
    /// Hits in the current window and the previous one, used by
    /// `sliding_window` to smooth the boundary burst a fixed window allows.
    current_window: u64,
    previous_window: u64,
    window_started: Instant,
}

impl Bucket {
    fn new(capacity: f64, now: Instant) -> Self {
        Self {
            tokens: capacity,
            last_update: now,
            current_window: 0,
            previous_window: 0,
            window_started: now,
        }
    }

    /// Refill according to elapsed time, capped at capacity.
    fn refill(&mut self, capacity: f64, per_second: f64, now: Instant) {
        let elapsed = now
            .saturating_duration_since(self.last_update)
            .as_secs_f64();
        if elapsed > 0.0 {
            self.tokens = (self.tokens + elapsed * per_second).min(capacity);
            self.last_update = now;
        }
    }

    /// Roll the sliding window forward, carrying the previous count.
    fn roll_window(&mut self, window: Duration, now: Instant) {
        let elapsed = now.saturating_duration_since(self.window_started);
        if elapsed >= window.saturating_mul(2) {
            self.previous_window = 0;
            self.current_window = 0;
            self.window_started = now;
        } else if elapsed >= window {
            self.previous_window = self.current_window;
            self.current_window = 0;
            self.window_started = now;
        }
    }
}

/// Counter registry. One entry per `(resource, scope)` pair.
#[derive(Debug, Default)]
pub struct RateLimiterRegistry {
    buckets: DashMap<String, Mutex<Bucket>>,
}

impl RateLimiterRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of live counters (diagnostics and tests).
    #[must_use]
    pub fn len(&self) -> usize {
        self.buckets.len()
    }

    /// Whether no counters exist yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }

    /// Drop every counter for a resource — used when an upstream or route is
    /// deleted, which is what the shared key prefix exists for.
    pub fn forget_resource(&self, resource_type: &str, resource_id: &str) {
        let prefix = format!("{KEY_PREFIX}:{resource_type}:{resource_id}:");
        self.buckets.retain(|key, _| !key.starts_with(&prefix));
    }

    /// Check and consume, using the wall clock.
    pub fn check(&self, key: &str, config: &RateLimitConfig) -> RateDecision {
        self.check_at(key, config, Instant::now())
    }

    /// Check and consume at an explicit instant.
    pub fn check_at(&self, key: &str, config: &RateLimitConfig, now: Instant) -> RateDecision {
        #[allow(clippy::cast_precision_loss)]
        let capacity = config.capacity() as f64;
        let per_second = config.tokens_per_second();
        let cost = f64::from(config.cost);

        let entry = self
            .buckets
            .entry(key.to_owned())
            .or_insert_with(|| Mutex::new(Bucket::new(capacity, now)));
        let mut bucket = entry.lock();

        let window = Duration::from_secs(config.sustained.window.seconds());
        bucket.roll_window(window, now);
        bucket.refill(capacity, per_second, now);

        let allowed = match config.algorithm {
            RateAlgorithm::TokenBucket => bucket.tokens >= cost,
            RateAlgorithm::SlidingWindow => {
                // Weight the previous window by the fraction of it that is
                // still inside the sliding view.
                let elapsed = now
                    .saturating_duration_since(bucket.window_started)
                    .as_secs_f64();
                let window_secs = window.as_secs_f64().max(f64::EPSILON);
                let carry = 1.0 - (elapsed / window_secs).clamp(0.0, 1.0);
                #[allow(clippy::cast_precision_loss)]
                let observed = bucket.current_window as f64 + bucket.previous_window as f64 * carry;
                #[allow(clippy::cast_precision_loss)]
                let ceiling = config.sustained.rate as f64;
                observed + cost <= ceiling
            }
        };

        if allowed {
            bucket.tokens = (bucket.tokens - cost).max(0.0);
            bucket.current_window = bucket.current_window.saturating_add(u64::from(config.cost));
        }

        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let remaining = bucket.tokens.max(0.0) as u64;
        let deficit = (cost - bucket.tokens).max(0.0);
        let retry_after_secs = if allowed {
            0
        } else if per_second <= 0.0 {
            config.sustained.window.seconds()
        } else {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let secs = (deficit / per_second).ceil() as u64;
            secs.max(1)
        };
        let missing = (capacity - bucket.tokens).max(0.0);
        let reset_after_secs = if per_second <= 0.0 {
            config.sustained.window.seconds()
        } else {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let secs = (missing / per_second).ceil() as u64;
            secs
        };
        let usage_ratio = if capacity > 0.0 {
            (1.0 - bucket.tokens / capacity).clamp(0.0, 1.0)
        } else {
            0.0
        };

        RateDecision {
            allowed,
            limit: config.capacity(),
            remaining,
            reset_after_secs,
            retry_after_secs,
            usage_ratio,
        }
    }
}

/// Build the counter key for a request.
///
/// `scope` decides which identity component participates, so a `tenant`-scoped
/// limit on one route cannot be consumed by traffic on another.
#[must_use]
#[allow(
    clippy::too_many_arguments,
    reason = "the key is the product of the documented scope dimensions; grouping them into a struct would only move the same list"
)]
pub fn counter_key(
    resource_type: &str,
    resource_id: &str,
    scope: RateScope,
    tenant_id: &str,
    subject_id: &str,
    client_ip: Option<&str>,
    route_id: &str,
    window: &str,
) -> String {
    let (scope_name, scope_id) = match scope {
        RateScope::Global => ("global", "all".to_owned()),
        RateScope::Tenant => ("tenant", tenant_id.to_owned()),
        RateScope::User => ("user", subject_id.to_owned()),
        RateScope::Ip => ("ip", client_ip.unwrap_or("unknown").to_owned()),
        RateScope::Route => ("route", route_id.to_owned()),
    };
    format!("{KEY_PREFIX}:{resource_type}:{resource_id}:{scope_name}:{scope_id}:{window}")
}

/// Whether the configured strategy answers an over-limit request with `429`.
#[must_use]
pub fn rejects_on_limit(strategy: RateStrategy) -> bool {
    matches!(strategy, RateStrategy::Reject)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{Burst, RateWindow, SharingMode, Sustained};

    fn config(rate: u64, capacity: Option<u64>, algorithm: RateAlgorithm) -> RateLimitConfig {
        RateLimitConfig {
            sharing: SharingMode::Private,
            algorithm,
            sustained: Sustained {
                rate,
                window: RateWindow::Second,
            },
            burst: capacity.map(|capacity| Burst {
                capacity: Some(capacity),
            }),
            budget: None,
            scope: RateScope::Tenant,
            strategy: RateStrategy::Reject,
            cost: 1,
            response_headers: true,
        }
    }

    #[test]
    fn token_bucket_allows_a_burst_then_rejects() {
        let registry = RateLimiterRegistry::new();
        let cfg = config(1, Some(3), RateAlgorithm::TokenBucket);
        let now = Instant::now();

        for expected_remaining in [2, 1, 0] {
            let decision = registry.check_at("k", &cfg, now);
            assert!(decision.allowed);
            assert_eq!(decision.remaining, expected_remaining);
            assert_eq!(decision.limit, 3);
        }
        let denied = registry.check_at("k", &cfg, now);
        assert!(!denied.allowed);
        assert!(denied.retry_after_secs >= 1);
        assert!((denied.usage_ratio - 1.0).abs() < 1e-9);
    }

    #[test]
    fn tokens_refill_over_time() {
        let registry = RateLimiterRegistry::new();
        let cfg = config(2, Some(2), RateAlgorithm::TokenBucket);
        let start = Instant::now();
        assert!(registry.check_at("k", &cfg, start).allowed);
        assert!(registry.check_at("k", &cfg, start).allowed);
        assert!(!registry.check_at("k", &cfg, start).allowed);

        // 2 tokens/second → half a second buys one token back.
        let later = start + Duration::from_millis(500);
        assert!(registry.check_at("k", &cfg, later).allowed);
    }

    #[test]
    fn cost_consumes_multiple_tokens() {
        let registry = RateLimiterRegistry::new();
        let mut cfg = config(10, Some(10), RateAlgorithm::TokenBucket);
        cfg.cost = 10;
        let now = Instant::now();
        assert!(registry.check_at("k", &cfg, now).allowed);
        assert!(!registry.check_at("k", &cfg, now).allowed, "bucket drained");
    }

    #[test]
    fn keys_are_independent() {
        let registry = RateLimiterRegistry::new();
        let cfg = config(1, Some(1), RateAlgorithm::TokenBucket);
        let now = Instant::now();
        assert!(registry.check_at("a", &cfg, now).allowed);
        assert!(!registry.check_at("a", &cfg, now).allowed);
        assert!(
            registry.check_at("b", &cfg, now).allowed,
            "a separate scope has its own bucket"
        );
    }

    #[test]
    fn sliding_window_caps_at_the_sustained_rate() {
        let registry = RateLimiterRegistry::new();
        // Capacity 100 would let a token bucket burst; the sliding window
        // holds the line at the sustained rate of 2/second.
        let cfg = config(2, Some(100), RateAlgorithm::SlidingWindow);
        let now = Instant::now();
        assert!(registry.check_at("k", &cfg, now).allowed);
        assert!(registry.check_at("k", &cfg, now).allowed);
        assert!(!registry.check_at("k", &cfg, now).allowed);

        // A full window later the carry has decayed to zero.
        let later = now + Duration::from_secs(2);
        assert!(registry.check_at("k", &cfg, later).allowed);
    }

    #[test]
    fn minute_window_scales_the_refill_rate() {
        let registry = RateLimiterRegistry::new();
        let cfg = RateLimitConfig {
            sustained: Sustained {
                rate: 60,
                window: RateWindow::Minute,
            },
            ..config(60, Some(1), RateAlgorithm::TokenBucket)
        };
        let now = Instant::now();
        assert!(registry.check_at("k", &cfg, now).allowed);
        assert!(!registry.check_at("k", &cfg, now).allowed);
        // 60/minute == 1/second.
        assert!(
            registry
                .check_at("k", &cfg, now + Duration::from_secs(1))
                .allowed
        );
    }

    #[test]
    fn counter_keys_reflect_the_scope() {
        let tenant = counter_key(
            "upstream",
            "u1",
            RateScope::Tenant,
            "t1",
            "s1",
            None,
            "r1",
            "second",
        );
        assert_eq!(tenant, "oagw:ratelimit:upstream:u1:tenant:t1:second");
        let user = counter_key(
            "upstream",
            "u1",
            RateScope::User,
            "t1",
            "s1",
            None,
            "r1",
            "second",
        );
        assert_eq!(user, "oagw:ratelimit:upstream:u1:user:s1:second");
        let ip = counter_key(
            "upstream",
            "u1",
            RateScope::Ip,
            "t1",
            "s1",
            Some("10.0.0.1"),
            "r1",
            "second",
        );
        assert_eq!(ip, "oagw:ratelimit:upstream:u1:ip:10.0.0.1:second");
        let global = counter_key(
            "upstream",
            "u1",
            RateScope::Global,
            "t1",
            "s1",
            None,
            "r1",
            "second",
        );
        assert_eq!(global, "oagw:ratelimit:upstream:u1:global:all:second");
        let route = counter_key(
            "route",
            "r1",
            RateScope::Route,
            "t1",
            "s1",
            None,
            "r1",
            "second",
        );
        assert_eq!(route, "oagw:ratelimit:route:r1:route:r1:second");
    }

    #[test]
    fn deleting_a_resource_drops_its_counters_by_prefix() {
        let registry = RateLimiterRegistry::new();
        let cfg = config(1, Some(1), RateAlgorithm::TokenBucket);
        let now = Instant::now();
        registry.check_at(
            &counter_key(
                "upstream",
                "u1",
                RateScope::Tenant,
                "t1",
                "s",
                None,
                "r",
                "second",
            ),
            &cfg,
            now,
        );
        registry.check_at(
            &counter_key(
                "upstream",
                "u2",
                RateScope::Tenant,
                "t1",
                "s",
                None,
                "r",
                "second",
            ),
            &cfg,
            now,
        );
        assert_eq!(registry.len(), 2);
        registry.forget_resource("upstream", "u1");
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn strategy_reject_is_the_only_429_path() {
        assert!(rejects_on_limit(RateStrategy::Reject));
        assert!(!rejects_on_limit(RateStrategy::Queue));
        assert!(!rejects_on_limit(RateStrategy::Degrade));
    }
}
