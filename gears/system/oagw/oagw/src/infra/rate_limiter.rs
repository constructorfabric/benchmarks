//! In-memory rate limiter (ADR-0003).
//!
//! Two algorithms:
//! * `token_bucket` (default): dual-rate cursor bucket with burst capacity.
//!   Refill is lazy — on each `try_acquire` the bucket is advanced by the
//!   elapsed time, then `cost` tokens are drained if available.
//! * `sliding_window`: fixed-window counter (window boundaries keyed by the
//!   window granularity), rejecting when the counter would exceed `rate`.
//!
//! Limiter keys follow ADR-0003:
//! `oagw:ratelimit:{resource_type}:{resource_id}:{scope}:{scope_id}:{window}`.
//! This is a per-instance limiter; cross-instance coordination is a future
//! distributed-bucket concern (ADR-0003 "budget" section).

use dashmap::DashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::domain::model::{
    RateLimitAlgorithm, RateLimitConfig, RateLimitScope, RateLimitStrategy,
};

/// Result of a rate check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcquireOutcome {
    pub allowed: bool,
    /// Effective limit reported to the caller.
    pub limit: u64,
    /// Tokens/capacity remaining after (or available before) this request.
    pub remaining: u64,
    /// UNIX seconds when the limit resets.
    pub reset_unix_secs: u64,
}

struct Bucket {
    /// Token bucket: current tokens.
    tokens: f64,
    /// Token bucket: last refill timestamp (millis).
    last_refill_ms: u64,
    /// Sliding window: window start (millis).
    window_start_ms: u64,
    /// Sliding window: count in current window.
    window_count: u64,
}

#[derive(Default)]
pub struct RateLimiter {
    buckets: DashMap<String, Bucket>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl RateLimiter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Compute the limiter key for a config+scope, per ADR-0003.
    #[must_use]
    pub fn key_for(
        &self,
        resource_kind: &str,
        resource_id: &str,
        cfg: &RateLimitConfig,
        scope_id: &str,
    ) -> String {
        format!(
            "oagw:ratelimit:{resource_kind}:{resource_id}:{scope}:{scope_id}:{window}",
            scope = cfg.scope.as_str(),
            window = window_name(cfg.sustained.window),
        )
    }

    /// The scope identifier for the caller (ADR-0003 scope semantics).
    #[must_use]
    pub fn scope_id(
        cfg: &RateLimitConfig,
        tenant_id: uuid::Uuid,
        subject_id: uuid::Uuid,
        client_ip: Option<std::net::IpAddr>,
        route_id: uuid::Uuid,
    ) -> String {
        match cfg.scope {
            RateLimitScope::Global => "global".to_owned(),
            RateLimitScope::Tenant => tenant_id.to_string(),
            RateLimitScope::User => subject_id.to_string(),
            RateLimitScope::Ip => client_ip
                .map(|ip| ip.to_string())
                .unwrap_or_else(|| "unknown".to_owned()),
            RateLimitScope::Route => route_id.to_string(),
        }
    }

    /// Try to acquire `cost` tokens for the (already-scoped) key.
    #[must_use]
    pub fn try_acquire(&self, key: &str, cfg: &RateLimitConfig, cost: u32) -> AcquireOutcome {
        let rate = cfg.sustained.rate.max(1);
        let window_secs = cfg.sustained.window.as_secs().max(1);
        let capacity = match &cfg.burst {
            Some(b) if b.capacity >= 1 => b.capacity as f64,
            _ => rate as f64,
        };

        let mut entry = self.buckets.entry(key.to_owned()).or_insert_with(|| Bucket {
            tokens: capacity,
            last_refill_ms: now_ms(),
            window_start_ms: now_ms(),
            window_count: 0,
        });
        let bucket = entry.value_mut();
        let now = now_ms();

        let (allowed, remaining, reset) = match cfg.algorithm {
            RateLimitAlgorithm::TokenBucket => {
                let rate_per_sec = rate as f64 / window_secs as f64;
                let elapsed = now.saturating_sub(bucket.last_refill_ms);
                bucket.tokens =
                    ((capacity.min(bucket.tokens + elapsed as f64 / 1000.0 * rate_per_sec)))
                        .max(0.0);
                bucket.last_refill_ms = now;

                let cost_f = cost as f64;
                let allowed = bucket.tokens >= cost_f;
                if allowed {
                    bucket.tokens -= cost_f;
                }
                let remaining = bucket.tokens.floor() as u64;
                // Reset: time until the bucket fully refills.
                let need_to_full = capacity - bucket.tokens;
                let reset_secs = if rate_per_sec > 0.0 {
                    (need_to_full / rate_per_sec) as u64 + 1
                } else {
                    1
                };
                (
                    allowed,
                    remaining,
                    (now / 1000) + reset_secs,
                )
            }
            RateLimitAlgorithm::SlidingWindow => {
                let window_ms = (window_secs * 1000) as u64;
                if now >= bucket.window_start_ms + window_ms {
                    bucket.window_start_ms = now;
                    bucket.window_count = 0;
                }
                if bucket.window_count + cost as u64 <= rate {
                    bucket.window_count += cost as u64;
                    (
                        true,
                        rate - bucket.window_count,
                        (bucket.window_start_ms / 1000) + window_secs,
                    )
                } else {
                    (
                        false,
                        bucket.window_count,
                        (bucket.window_start_ms / 1000) + window_secs,
                    )
                }
            }
        };

        AcquireOutcome {
            allowed,
            limit: rate,
            remaining,
            reset_unix_secs: reset,
        }
    }

    /// Whether the configured strategy degrades rather than rejects.
    #[must_use]
    pub fn degrades(strategy: RateLimitStrategy) -> bool {
        matches!(strategy, RateLimitStrategy::Degrade)
    }
}

fn window_name(window: crate::domain::model::RateLimitWindow) -> &'static str {
    match window {
        crate::domain::model::RateLimitWindow::Second => "second",
        crate::domain::model::RateLimitWindow::Minute => "minute",
        crate::domain::model::RateLimitWindow::Hour => "hour",
        crate::domain::model::RateLimitWindow::Day => "day",
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::model::{BurstConfig, RateLimitWindow, SharingMode, SustainedRate};

    fn cfg(rate: u64, window: RateLimitWindow, capacity: Option<u64>) -> RateLimitConfig {
        RateLimitConfig {
            sharing: SharingMode::Private,
            algorithm: RateLimitAlgorithm::TokenBucket,
            sustained: SustainedRate { rate, window },
            burst: capacity.map(|c| BurstConfig { capacity: c }),
            scope: RateLimitScope::Tenant,
            strategy: RateLimitStrategy::Reject,
            cost: 1,
            response_headers: true,
        }
    }

    #[tokio::test]
    async fn token_bucket_allows_burst_then_rejects() {
        let limiter = RateLimiter::new();
        // 5 req/sec with burst capacity 5 => 10 token bucket (5 sustained + 5 burst
        // is not how the model works: burst.capacity is the bucket capacity).
        let cfg = cfg(5, RateLimitWindow::Second, Some(5));
        let key = limiter.key_for("upstream", "u1", &cfg, "t1");

        // Burst: the bucket holds `capacity` = 5 tokens.
        for _ in 0..5 {
            let o = limiter.try_acquire(&key, &cfg, 1);
            assert!(o.allowed, "burst requests must be allowed");
        }
        let sixth = limiter.try_acquire(&key, &cfg, 1);
        assert!(!sixth.allowed, "capacity exhausted");
    }

    #[tokio::test]
    async fn refill_happens_on_access() {
        let limiter = RateLimiter::new();
        let cfg = cfg(100, RateLimitWindow::Second, Some(1));
        let key = limiter.key_for("route", "r1", &cfg, "t1");

        let first = limiter.try_acquire(&key, &cfg, 1);
        assert!(first.allowed);
        // Immediately after draining, the bucket is empty: rejected.
        let second = limiter.try_acquire(&key, &cfg, 1);
        assert!(!second.allowed);

        // After ~50ms the bucket has refilled ~5 tokens at 100/s => allowed.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let third = limiter.try_acquire(&key, &cfg, 1);
        assert!(third.allowed);
    }

    #[tokio::test]
    async fn sliding_window_counts_in_window() {
        let limiter = RateLimiter::new();
        let cfg = RateLimitConfig {
            algorithm: RateLimitAlgorithm::SlidingWindow,
            sustained: SustainedRate {
                rate: 2,
                window: RateLimitWindow::Second,
            },
            ..cfg(2, RateLimitWindow::Second, Some(2))
        };
        let key = limiter.key_for("upstream", "u1", &cfg, "t1");
        assert!(limiter.try_acquire(&key, &cfg, 1).allowed);
        assert!(limiter.try_acquire(&key, &cfg, 1).allowed);
        assert!(!limiter.try_acquire(&key, &cfg, 1).allowed);
    }

    #[test]
    fn key_format_matches_adr() {
        let limiter = RateLimiter::new();
        let cfg = cfg(1, RateLimitWindow::Minute, None);
        let key = limiter.key_for("upstream", "abc", &cfg, "tenant-1");
        assert_eq!(key, "oagw:ratelimit:upstream:abc:tenant:tenant-1:minute");
    }
}
