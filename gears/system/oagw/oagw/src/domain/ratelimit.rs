//! Token-bucket rate limiting (ADR 0003).

use std::time::Instant;

use crate::domain::dto::{RateLimit, RateScope, SharingMode};

/// A token bucket: `capacity` tokens, refilled at `refill_rate` tokens/second.
///
/// The state (`tokens`, `last_update`) is exactly the triple ADR 0003
/// specifies; `capacity` and `refill_rate` are the immutable limits.
#[derive(Debug, Clone)]
pub struct TokenBucket {
    /// Tokens currently available.
    pub tokens: f64,
    /// Instant of the last refill.
    pub last_update: Instant,
    /// Maximum number of tokens the bucket holds.
    pub capacity: u64,
    /// Tokens replenished per second.
    pub refill_rate: f64,
}

impl TokenBucket {
    /// Builds a full bucket.
    pub fn new(capacity: u64, refill_rate: f64) -> Self {
        Self {
            tokens: capacity as f64,
            last_update: Instant::now(),
            capacity: capacity.max(1),
            refill_rate: refill_rate.max(0.0),
        }
    }

    /// Refills the bucket from the elapsed time since the last refill.
    pub fn refill(&mut self) {
        let elapsed = self.last_update.elapsed().as_secs_f64();
        self.last_update = Instant::now();
        if elapsed <= 0.0 {
            return;
        }
        if self.refill_rate <= 0.0 {
            return;
        }
        self.tokens = (self.tokens + elapsed * self.refill_rate).min(self.capacity as f64);
    }

    /// Takes `cost` tokens, returning the number left when the bucket could
    /// afford the request and `None` when it could not.
    pub fn try_acquire(&mut self, cost: u64) -> Option<u64> {
        let cost = cost.max(1) as f64;
        if self.tokens + f64::EPSILON < cost {
            return None;
        }
        self.tokens -= cost;
        Some((self.tokens.floor().max(0.0)) as u64)
    }

    /// The fraction of the bucket already consumed.
    pub fn usage_ratio(&self) -> f64 {
        if self.capacity == 0 {
            return 1.0;
        }
        1.0 - (self.tokens / self.capacity as f64)
    }

    /// Seconds until `cost` tokens are available again.
    pub fn retry_after(&self, cost: u64) -> u64 {
        if self.refill_rate <= 0.0 {
            return 1;
        }
        let deficit = cost.max(1) as f64 - self.tokens;
        if deficit <= 0.0 {
            return 0;
        }
        (deficit / self.refill_rate).ceil().max(1.0) as u64
    }
}

/// The effective rate limit after layering upstream < route < tenant.
///
/// ADR 0003: the most restrictive configuration wins, and an ancestor
/// `enforce` limit can never be relaxed by a descendant.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveRateLimit {
    /// Bucket capacity.
    pub capacity: u64,
    /// Tokens replenished per second.
    pub refill_rate: f64,
    /// Tokens consumed per request.
    pub cost: u64,
    /// Counter scope.
    pub scope: RateScope,
    /// Whether the effective limit came from an ancestor `enforce` layer.
    pub ancestor_enforced: bool,
    /// Whether responses may carry `X-RateLimit-*` headers (ADR 0003).
    pub response_headers: bool,
}

impl EffectiveRateLimit {
    /// Builds the effective limit of a single configured layer.
    pub fn from_config(config: &RateLimit) -> Self {
        Self {
            capacity: config.capacity(),
            refill_rate: config.refill_rate(),
            cost: config.cost.max(1),
            scope: config.scope,
            ancestor_enforced: false,
            response_headers: config.response_headers,
        }
    }

    /// Merges another layer in, keeping the most restrictive value.
    ///
    /// `enforce`d ancestor layers are never relaxed: the merged layer keeps
    /// the `enforce` marker so descendants cannot raise it again.
    pub fn merge(&mut self, other: &Self) {
        if other.refill_rate < self.refill_rate {
            self.refill_rate = other.refill_rate;
        }
        if other.capacity < self.capacity {
            self.capacity = other.capacity;
        }
        if other.cost > self.cost {
            self.cost = other.cost;
        }
        self.ancestor_enforced = self.ancestor_enforced || other.ancestor_enforced;
        // Advertising the counters is a disclosure, so a layer that withholds
        // them wins over one that publishes them.
        self.response_headers = self.response_headers && other.response_headers;
    }

    /// The most restrictive of two layers.
    pub fn min(a: &Self, b: &Self) -> Self {
        let mut merged = a.clone();
        merged.merge(b);
        merged
    }
}

/// Scope value of a request for the configured [`RateScope`].
pub fn scope_value(scope: RateScope, tenant_id: &str, user_id: &str, ip: &str, route: &str) -> String {
    match scope {
        RateScope::Global => "global".to_string(),
        RateScope::Tenant => tenant_id.to_string(),
        RateScope::User => user_id.to_string(),
        RateScope::Ip => ip.to_string(),
        RateScope::Route => route.to_string(),
    }
}

/// Whether a rate-limit layer is inherited by descendants.
pub fn sharing_allows_override(sharing: Option<SharingMode>) -> bool {
    !matches!(sharing, Some(SharingMode::Enforce))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::dto::{Burst, RateWindow, Sustained};

    fn bucket(capacity: u64, rate_per_sec: f64) -> TokenBucket {
        TokenBucket::new(capacity, rate_per_sec)
    }

    #[test]
    fn burst_is_limited_by_capacity() {
        let mut b = bucket(3, 0.0);
        assert_eq!(b.try_acquire(1), Some(2));
        assert_eq!(b.try_acquire(1), Some(1));
        assert_eq!(b.try_acquire(1), Some(0));
        assert_eq!(b.try_acquire(1), None, "capacity is exhausted");
    }

    #[test]
    fn capacity_defaults_to_the_sustained_rate() {
        let cfg = RateLimit {
            sustained: Sustained { rate: 10, window: RateWindow::Second },
            ..RateLimit::default()
        };
        assert_eq!(cfg.capacity(), 10);
        assert_eq!(cfg.refill_rate(), 10.0);
    }

    #[test]
    fn burst_capacity_overrides_the_sustained_rate() {
        let cfg = RateLimit {
            sustained: Sustained { rate: 10, window: RateWindow::Second },
            burst: Some(Burst { capacity: 25 }),
            ..RateLimit::default()
        };
        assert_eq!(cfg.capacity(), 25);
        assert_eq!(cfg.refill_rate(), 10.0);
    }

    #[test]
    fn cost_multiplies_consumption() {
        let mut b = bucket(5, 0.0);
        assert_eq!(b.try_acquire(3), Some(2));
        assert_eq!(b.try_acquire(3), None);
        assert_eq!(b.try_acquire(2), Some(0));
    }

    #[test]
    fn refill_restores_tokens_over_time() {
        let mut b = bucket(2, 10.0);
        assert_eq!(b.try_acquire(2), Some(0));
        assert_eq!(b.try_acquire(1), None);
        std::thread::sleep(std::time::Duration::from_millis(120));
        b.refill();
        assert!(b.tokens > 0.5, "tokens refilled by ~1.2, got {}", b.tokens);
    }

    #[test]
    fn usage_ratio_reports_consumption() {
        let mut b = bucket(4, 0.0);
        assert!(b.usage_ratio() < f64::EPSILON);
        let _ = b.try_acquire(4);
        assert!((b.usage_ratio() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn retry_after_reflects_the_refill_rate() {
        let mut b = bucket(1, 2.0);
        let _ = b.try_acquire(1);
        assert_eq!(b.retry_after(1), 1, "two tokens a second recover one in 1s");
        let mut slow = bucket(1, 0.5);
        let _ = slow.try_acquire(1);
        assert_eq!(slow.retry_after(1), 2, "half a token a second needs 2s");
    }

    #[test]
    fn min_keeps_the_most_restrictive_layer() {
        let a = EffectiveRateLimit {
            capacity: 100,
            refill_rate: 10.0,
            cost: 1,
            scope: RateScope::Tenant,
            ancestor_enforced: false,
            response_headers: true,
        };
        let b = EffectiveRateLimit {
            capacity: 5,
            refill_rate: 2.0,
            cost: 3,
            scope: RateScope::User,
            ancestor_enforced: true,
            response_headers: true,
        };
        let merged = EffectiveRateLimit::min(&a, &b);
        assert_eq!(merged.capacity, 5);
        assert_eq!(merged.refill_rate, 2.0);
        assert_eq!(merged.cost, 3);
        assert!(merged.ancestor_enforced);
    }

    #[test]
    fn window_lengths_are_seconds() {
        assert_eq!(RateWindow::Second.seconds(), 1);
        assert_eq!(RateWindow::Minute.seconds(), 60);
        assert_eq!(RateWindow::Hour.seconds(), 3_600);
        assert_eq!(RateWindow::Day.seconds(), 86_400);
    }

    #[test]
    fn scope_value_selects_the_request_identity() {
        assert_eq!(scope_value(RateScope::Global, "t", "u", "ip", "r"), "global");
        assert_eq!(scope_value(RateScope::Tenant, "t", "u", "ip", "r"), "t");
        assert_eq!(scope_value(RateScope::User, "t", "u", "ip", "r"), "u");
        assert_eq!(scope_value(RateScope::Ip, "t", "u", "ip", "r"), "ip");
        assert_eq!(scope_value(RateScope::Route, "t", "u", "ip", "r"), "r");
    }

    #[test]
    fn enforce_blocks_override() {
        assert!(!sharing_allows_override(Some(SharingMode::Enforce)));
        assert!(sharing_allows_override(Some(SharingMode::Inherit)));
        assert!(sharing_allows_override(None));
    }
}
