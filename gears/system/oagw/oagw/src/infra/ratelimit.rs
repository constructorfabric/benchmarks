//! Per-instance token-bucket rate limiting (ADR-0003).
//!
//! MVP posture: local buckets owned by the Data Plane, no distributed
//! coordination. The key layout keeps the `{resource_type}:{resource_id}`
//! prefix so a deleted upstream or route can be swept in one pass.

use std::time::Instant;

use dashmap::DashMap;
use parking_lot::Mutex;

use crate::domain::model::{RateLimitConfig, RateLimitScope, RateLimitStrategy};

/// Outcome of one rate-limit check.
#[derive(Debug, Clone, PartialEq)]
pub struct RateLimitOutcome {
    pub allowed: bool,
    /// `X-RateLimit-Limit` — the sustained rate in its configured window.
    pub limit: u32,
    /// `X-RateLimit-Remaining` — whole tokens left in the bucket.
    pub remaining: u32,
    /// `X-RateLimit-Reset` — Unix time at which the bucket is full again.
    pub reset_epoch: u64,
    /// `Retry-After`, in seconds; only meaningful when `allowed` is false.
    pub retry_after_seconds: u64,
    /// Fraction of the bucket consumed, for `oagw_rate_limit_usage_ratio`.
    pub usage_ratio: f64,
}

/// Classic token bucket: `tokens` refills at `refill_rate` up to `capacity`.
#[derive(Debug)]
pub struct TokenBucket {
    tokens: f64,
    last_update: Instant,
    capacity: f64,
    refill_rate: f64,
}

impl TokenBucket {
    #[must_use]
    pub fn new(capacity: f64, refill_rate: f64) -> Self {
        Self::new_at(capacity, refill_rate, Instant::now())
    }

    /// Deterministic constructor: the bucket starts full as of `now`.
    #[must_use]
    pub fn new_at(capacity: f64, refill_rate: f64, now: Instant) -> Self {
        Self {
            tokens: capacity,
            last_update: now,
            capacity,
            refill_rate,
        }
    }

    /// Re-shape an existing bucket when the effective configuration changes,
    /// clamping the balance rather than refilling it.
    fn reshape(&mut self, capacity: f64, refill_rate: f64) {
        if (self.capacity - capacity).abs() > f64::EPSILON
            || (self.refill_rate - refill_rate).abs() > f64::EPSILON
        {
            self.capacity = capacity;
            self.refill_rate = refill_rate;
            self.tokens = self.tokens.min(capacity);
        }
    }

    fn refill_at(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last_update).as_secs_f64();
        if elapsed > 0.0 {
            self.tokens = (self.tokens + elapsed * self.refill_rate).min(self.capacity);
            self.last_update = now;
        }
    }

    /// Try to consume `cost` tokens.
    pub fn try_acquire_at(&mut self, cost: u32, now: Instant) -> bool {
        self.refill_at(now);
        let cost = f64::from(cost);
        if self.tokens >= cost {
            self.tokens -= cost;
            true
        } else {
            false
        }
    }

    /// Seconds until `cost` tokens are available (0 when already available).
    #[must_use]
    pub fn seconds_until(&self, cost: u32) -> f64 {
        let deficit = f64::from(cost) - self.tokens;
        if deficit <= 0.0 || self.refill_rate <= 0.0 {
            0.0
        } else {
            deficit / self.refill_rate
        }
    }

    /// Seconds until the bucket is completely refilled.
    #[must_use]
    pub fn seconds_until_full(&self) -> f64 {
        let deficit = self.capacity - self.tokens;
        if deficit <= 0.0 || self.refill_rate <= 0.0 {
            0.0
        } else {
            deficit / self.refill_rate
        }
    }

    #[must_use]
    pub fn tokens(&self) -> f64 {
        self.tokens
    }
}

/// Registry of per-key buckets owned by the Data Plane (ADR-0006).
#[derive(Default)]
pub struct RateLimiterRegistry {
    buckets: DashMap<String, Mutex<TokenBucket>>,
}

impl RateLimiterRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Key layout: `oagw:ratelimit:{resource_type}:{resource_id}:{scope}:{scope_id}`.
    #[must_use]
    pub fn build_key(
        resource_type: &str,
        resource_id: &str,
        scope: RateLimitScope,
        scope_id: &str,
    ) -> String {
        let scope_name = match scope {
            RateLimitScope::Global => "global",
            RateLimitScope::Tenant => "tenant",
            RateLimitScope::User => "user",
            RateLimitScope::Ip => "ip",
            RateLimitScope::Route => "route",
        };
        format!("oagw:ratelimit:{resource_type}:{resource_id}:{scope_name}:{scope_id}")
    }

    /// Evaluate `config` against the bucket at `key`.
    #[must_use]
    pub fn check(&self, key: &str, config: &RateLimitConfig) -> RateLimitOutcome {
        self.check_at(key, config, Instant::now())
    }

    /// Deterministic variant used by tests.
    #[must_use]
    pub fn check_at(
        &self,
        key: &str,
        config: &RateLimitConfig,
        now: Instant,
    ) -> RateLimitOutcome {
        let capacity = f64::from(config.capacity());
        let refill = config.refill_per_second();
        let entry = self
            .buckets
            .entry(key.to_owned())
            .or_insert_with(|| Mutex::new(TokenBucket::new_at(capacity, refill, now)));
        let mut bucket = entry.lock();
        bucket.reshape(capacity, refill);

        let allowed = bucket.try_acquire_at(config.cost, now);
        let retry_after = if allowed {
            0
        } else {
            bucket.seconds_until(config.cost).ceil().max(1.0) as u64
        };
        let reset_in = bucket.seconds_until_full().ceil().max(0.0) as u64;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let remaining = bucket.tokens().floor().max(0.0) as u32;
        let usage_ratio = if capacity > 0.0 {
            ((capacity - bucket.tokens()) / capacity).clamp(0.0, 1.0)
        } else {
            0.0
        };

        RateLimitOutcome {
            allowed,
            limit: config.sustained.rate,
            remaining,
            reset_epoch: crate::util::unix_now().saturating_add(reset_in),
            retry_after_seconds: retry_after,
            usage_ratio,
        }
    }

    /// Drop every bucket whose key starts with `prefix` — used when the owning
    /// upstream or route is deleted.
    pub fn purge_prefix(&self, prefix: &str) {
        self.buckets.retain(|key, _| !key.starts_with(prefix));
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.buckets.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }
}

/// Whether the configured strategy short-circuits the request with a 429.
#[must_use]
pub fn rejects(strategy: RateLimitStrategy) -> bool {
    // `queue` and `degrade` are documented but degrade to pass-through in the
    // MVP: neither has a backpressure queue behind it yet (DESIGN §4.7).
    matches!(strategy, RateLimitStrategy::Reject)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{
        BurstConfig, RateLimitAlgorithm, RateLimitWindow, SharingMode, SustainedRate,
    };
    use std::time::Duration;

    fn config(rate: u32, capacity: Option<u32>, cost: u32) -> RateLimitConfig {
        RateLimitConfig {
            sharing: SharingMode::Private,
            algorithm: RateLimitAlgorithm::TokenBucket,
            sustained: SustainedRate {
                rate,
                window: RateLimitWindow::Second,
            },
            burst: BurstConfig { capacity },
            budget: None,
            scope: RateLimitScope::Tenant,
            strategy: RateLimitStrategy::Reject,
            cost,
            response_headers: true,
        }
    }

    #[test]
    fn bursts_are_allowed_up_to_capacity_then_rejected() {
        let reg = RateLimiterRegistry::new();
        let cfg = config(1, Some(3), 1);
        let now = Instant::now();
        for i in 0..3 {
            assert!(reg.check_at("k", &cfg, now).allowed, "token {i}");
        }
        let denied = reg.check_at("k", &cfg, now);
        assert!(!denied.allowed);
        assert_eq!(denied.limit, 1);
        assert_eq!(denied.remaining, 0);
        assert!(denied.retry_after_seconds >= 1);
    }

    #[test]
    fn tokens_refill_over_time() {
        let reg = RateLimiterRegistry::new();
        let cfg = config(10, Some(1), 1);
        let t0 = Instant::now();
        assert!(reg.check_at("k", &cfg, t0).allowed);
        assert!(!reg.check_at("k", &cfg, t0).allowed);
        // 10 tokens/second: 200ms is enough for two tokens, capacity caps at 1.
        let t1 = t0 + Duration::from_millis(200);
        assert!(reg.check_at("k", &cfg, t1).allowed);
    }

    #[test]
    fn cost_consumes_multiple_tokens() {
        let reg = RateLimiterRegistry::new();
        let cfg = config(100, Some(10), 10);
        let now = Instant::now();
        assert!(reg.check_at("k", &cfg, now).allowed);
        assert!(!reg.check_at("k", &cfg, now).allowed);
    }

    #[test]
    fn buckets_are_isolated_per_key() {
        let reg = RateLimiterRegistry::new();
        let cfg = config(1, Some(1), 1);
        let now = Instant::now();
        assert!(reg.check_at("a", &cfg, now).allowed);
        assert!(reg.check_at("b", &cfg, now).allowed);
        assert!(!reg.check_at("a", &cfg, now).allowed);
    }

    #[test]
    fn key_layout_supports_prefix_purge() {
        let key = RateLimiterRegistry::build_key(
            "upstream",
            "u-1",
            RateLimitScope::Tenant,
            "t-1",
        );
        assert_eq!(key, "oagw:ratelimit:upstream:u-1:tenant:t-1");

        let reg = RateLimiterRegistry::new();
        let cfg = config(5, None, 1);
        let _ = reg.check(&key, &cfg);
        let _ = reg.check("oagw:ratelimit:upstream:u-2:tenant:t-1", &cfg);
        assert_eq!(reg.len(), 2);
        reg.purge_prefix("oagw:ratelimit:upstream:u-1:");
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn only_reject_short_circuits() {
        assert!(rejects(RateLimitStrategy::Reject));
        assert!(!rejects(RateLimitStrategy::Queue));
        assert!(!rejects(RateLimitStrategy::Degrade));
    }

    #[test]
    fn a_minute_window_refills_proportionally() {
        let reg = RateLimiterRegistry::new();
        let mut cfg = config(60, Some(1), 1);
        cfg.sustained.window = RateLimitWindow::Minute;
        let t0 = Instant::now();
        assert!(reg.check_at("k", &cfg, t0).allowed);
        assert!(!reg.check_at("k", &cfg, t0).allowed);
        // 60/minute == 1/second.
        assert!(reg.check_at("k", &cfg, t0 + Duration::from_secs(1)).allowed);
    }
}
