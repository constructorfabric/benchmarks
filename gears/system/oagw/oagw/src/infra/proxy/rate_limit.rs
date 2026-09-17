//! Dual-rate limiter (ADR-0003).
//!
//! A token bucket per scope key: tokens refill at the sustained rate and the
//! bucket capacity is the burst. Non-`Reject` strategies degrade to `Reject`
//! (recorded as a known gap).

use dashmap::DashMap;

use crate::domain::dto::{RateLimitConfig, RateScope};
use crate::domain::error::DomainError;

/// A single token bucket.
#[derive(Debug, Clone, Copy)]
struct Bucket {
    /// Tokens currently available.
    tokens: f64,
    /// Instant of the last refill (epoch seconds, fractional).
    last_refill: f64,
}

/// Rate limiter holding one bucket per scope key.
#[derive(Default)]
pub struct RateLimiter {
    buckets: DashMap<String, Bucket>,
}

impl std::fmt::Debug for RateLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RateLimiter")
            .field("buckets", &self.buckets.len())
            .finish()
    }
}

/// Outcome of a rate-limit check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    /// Whether the request may proceed.
    pub allowed: bool,
    /// Tokens left after the decision.
    pub remaining: u32,
    /// Seconds to wait before retrying (only when rejected).
    pub retry_after_seconds: u64,
    /// Epoch seconds at which the bucket is fully replenished.
    pub reset_epoch_seconds: u64,
    /// Configured burst capacity, surfaced as the limit.
    pub limit: u32,
}

impl RateLimiter {
    /// Build an empty limiter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Derive the bucket key for a scope.
    #[must_use]
    pub fn scope_key(
        config: &RateLimitConfig,
        tenant_id: uuid::Uuid,
        subject: Option<&str>,
        client_ip: Option<std::net::IpAddr>,
        route_id: Option<uuid::Uuid>,
    ) -> String {
        let scope = match config.scope {
            RateScope::Global => "global".to_owned(),
            RateScope::Tenant => format!("tenant:{tenant_id}"),
            RateScope::User => format!("tenant:{tenant_id}:user:{}", subject.unwrap_or("-")),
            RateScope::Ip => format!(
                "tenant:{tenant_id}:ip:{}",
                client_ip.map(|ip| ip.to_string()).unwrap_or_else(|| "-".into())
            ),
            RateScope::Route => format!(
                "tenant:{tenant_id}:route:{}",
                route_id.map(|r| r.to_string()).unwrap_or_else(|| "-".into())
            ),
        };
        format!("{scope}:{}", algorithm_tag(config))
    }

    /// Consume `cost` tokens from the bucket for `key`.
    #[must_use]
    pub fn check(&self, key: &str, config: &RateLimitConfig) -> Decision {
        self.take(key, config, config.cost.max(1))
    }

    /// Consume an explicit number of tokens.
    #[must_use]
    pub fn take(&self, key: &str, config: &RateLimitConfig, cost: u32) -> Decision {
        let now = now_epoch_f64();
        let capacity = f64::from(config.burst.capacity.max(1));
        let window_secs = config.sustained.window.secs().max(1) as f64;
        let refill_per_second = f64::from(config.sustained.rate.max(1)) / window_secs;
        let cost = f64::from(cost.max(1));

        let mut entry = self.buckets.entry(key.to_owned()).or_insert(Bucket {
            tokens: capacity,
            last_refill: now,
        });
        let bucket = entry.value_mut();
        let elapsed = (now - bucket.last_refill).max(0.0);
        bucket.tokens = (bucket.tokens + elapsed * refill_per_second).min(capacity);
        bucket.last_refill = now;

        let allowed = bucket.tokens >= cost;
        if allowed {
            bucket.tokens -= cost;
        }
        let remaining = bucket.tokens.floor().max(0.0) as u32;
        let deficit = if allowed { 0.0 } else { cost - bucket.tokens };
        let retry_after = (deficit / refill_per_second).ceil().max(1.0) as u64;
        let reset = now + ((capacity - bucket.tokens) / refill_per_second);
        drop(entry);

        Decision {
            allowed,
            remaining,
            retry_after_seconds: if allowed { 0 } else { retry_after },
            reset_epoch_seconds: reset.ceil() as u64,
            limit: config.burst.capacity.max(1),
        }
    }

    /// Convert a rejected decision into a domain error.
    #[must_use]
    pub fn rejection(&self, decision: Decision) -> DomainError {
        DomainError::RateLimitExceeded {
            retry_after_seconds: decision.retry_after_seconds,
            limit: decision.limit,
            remaining: decision.remaining,
            reset_epoch_seconds: decision.reset_epoch_seconds,
        }
    }

    /// Drop every bucket (used by tests and by config invalidation).
    pub fn clear(&self) {
        self.buckets.clear();
    }

    /// Drop buckets that have been idle for at least `idle_secs` seconds and
    /// report how many were removed.
    ///
    /// Called from the lifecycle tick so that short-lived scope keys (per-user,
    /// per-ip) do not accumulate without bound.
    pub fn prune_idle(&self, idle_secs: u64) -> usize {
        if idle_secs == 0 {
            return 0;
        }
        let cutoff = now_epoch_f64() - idle_secs as f64;
        let before = self.buckets.len();
        self.buckets.retain(|_, bucket| bucket.last_refill > cutoff);
        before.saturating_sub(self.buckets.len())
    }

    /// Number of live buckets (observability).
    #[must_use]
    pub fn len(&self) -> usize {
        self.buckets.len()
    }

    /// Whether no bucket is live.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }
}

fn algorithm_tag(config: &RateLimitConfig) -> &'static str {
    match config.algorithm {
        crate::domain::dto::RateAlgorithm::TokenBucket => "tb",
        crate::domain::dto::RateAlgorithm::SlidingWindow => "sw",
    }
}

fn now_epoch_f64() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::dto::{Burst, RateAlgorithm, SustainedRate, RateWindow};

    fn config() -> RateLimitConfig {
        RateLimitConfig {
            burst: Burst { capacity: 3 },
            sustained: SustainedRate {
                rate: 1,
                window: RateWindow::Second,
            },
            ..RateLimitConfig::default()
        }
    }

    #[test]
    fn burst_is_honoured_then_rejected() {
        let limiter = RateLimiter::new();
        let config = config();
        let decision = limiter.check("k", &config);
        assert!(decision.allowed);
        assert_eq!(decision.remaining, 2);
        assert!(limiter.check("k", &config).allowed);
        assert!(limiter.check("k", &config).allowed);
        let rejected = limiter.check("k", &config);
        assert!(!rejected.allowed);
        assert_eq!(rejected.retry_after_seconds, 1);
        assert_eq!(rejected.limit, 3);
    }

    #[test]
    fn distinct_keys_do_not_share_a_bucket() {
        let limiter = RateLimiter::new();
        let config = config();
        assert!(limiter.check("a", &config).allowed);
        assert!(limiter.check("b", &config).allowed);
    }

    #[test]
    fn scope_key_varies_by_scope() {
        let mut config = config();
        let tenant = uuid::Uuid::nil();
        let ip = std::net::IpAddr::from([10u8, 0, 0, 1]);
        config.scope = RateScope::Tenant;
        let tenant_key = RateLimiter::scope_key(&config, tenant, None, None, None);
        config.scope = RateScope::Ip;
        let ip_key = RateLimiter::scope_key(&config, tenant, None, Some(ip), None);
        config.scope = RateScope::Route;
        let route_key = RateLimiter::scope_key(
            &config,
            tenant,
            None,
            Some(ip),
            Some(uuid::Uuid::now_v7()),
        );
        assert_ne!(tenant_key, ip_key);
        assert_ne!(ip_key, route_key);
        config.scope = RateScope::Global;
        assert!(RateLimiter::scope_key(&config, tenant, None, None, None).starts_with("global"));
        assert_eq!(
            algorithm_tag(&config),
            "tb",
            "token bucket tag by default"
        );
        let _ = RateAlgorithm::SlidingWindow;
    }

    #[test]
    fn rejection_carries_rate_headers() {
        let limiter = RateLimiter::new();
        let config = config();
        for _ in 0..3 {
            let _ = limiter.check("k", &config);
        }
        let decision = limiter.check("k", &config);
        let error = limiter.rejection(decision);
        assert_eq!(error.status(), 429);
        assert_eq!(
            error.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1"
        );
    }
}
