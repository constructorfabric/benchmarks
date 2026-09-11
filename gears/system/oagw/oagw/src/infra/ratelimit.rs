// @cpt-begin:cpt-cf-oagw-dod-policy-rate-limiting:p2:inst-ratelimit
//! Per-instance token-bucket rate limiting.
//!
//! Limiters are local to the process. Distributed synchronisation through a
//! shared cache is out of scope for this build, so a limit is enforced per
//! instance rather than globally.

use crate::domain::model::{RateLimitConfig, RateLimitScope, RateLimitStrategy};
use dashmap::DashMap;
use std::time::{Duration, Instant};

/// Outcome of an admission check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    /// The request may proceed.
    Allowed {
        /// Configured limit for the window.
        limit: u32,
        /// Tokens left after this request.
        remaining: u32,
        /// Seconds until the bucket is full again.
        reset_after: u64,
    },
    /// The request must be rejected.
    Rejected {
        /// Configured limit for the window.
        limit: u32,
        /// Seconds the caller should wait.
        retry_after: u64,
    },
    /// The request proceeds but is marked degraded.
    Degraded {
        /// Configured limit for the window.
        limit: u32,
    },
}

/// Round a non-negative value up to a whole number of units.
///
/// The value is clamped into range first, so the conversion below cannot
/// truncate meaningfully, lose a sign, or wrap.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the value is clamped to a non-negative in-range float before conversion"
)]
fn whole_units(value: f64) -> u64 {
    let rounded = value.ceil();
    if !rounded.is_finite() || rounded <= 0.0 {
        return 0;
    }
    // 2^53 is the largest integer an f64 represents exactly.
    rounded.min(9_007_199_254_740_992.0) as u64
}

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    last_refill: Instant,
}

/// A collection of token buckets keyed by scope.
#[derive(Debug, Default)]
pub struct RateLimiter {
    buckets: DashMap<String, Bucket>,
}

impl RateLimiter {
    /// Create an empty limiter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Build the bucket key for a scope.
    #[must_use]
    pub fn scope_key(
        scope: RateLimitScope,
        resource_id: &str,
        tenant_id: &str,
        subject_id: &str,
        client_ip: &str,
        route_id: &str,
    ) -> String {
        let discriminator = match scope {
            RateLimitScope::Global => "global",
            RateLimitScope::Tenant => tenant_id,
            RateLimitScope::User => subject_id,
            RateLimitScope::Ip => client_ip,
            RateLimitScope::Route => route_id,
        };
        format!("{resource_id}:{discriminator}")
    }

    /// Check and consume capacity for one request.
    #[must_use]
    pub fn check(&self, key: &str, config: &RateLimitConfig) -> Admission {
        let capacity = f64::from(config.burst_capacity());
        let window = config.sustained.window.seconds();
        // Window lengths are small constants, so the conversion is exact.
        let window_secs = f64::from(u32::try_from(window).unwrap_or(u32::MAX));
        let refill_per_second = f64::from(config.sustained.rate) / window_secs;
        let cost = f64::from(config.cost);
        let now = Instant::now();

        let mut entry = self
            .buckets
            .entry(key.to_owned())
            .or_insert_with(|| Bucket {
                tokens: capacity,
                last_refill: now,
            });

        let elapsed = now.saturating_duration_since(entry.last_refill);
        entry.tokens = (entry.tokens + elapsed.as_secs_f64() * refill_per_second).min(capacity);
        entry.last_refill = now;

        if entry.tokens >= cost {
            entry.tokens -= cost;
            let remaining = entry.tokens.floor().max(0.0);
            let deficit = capacity - entry.tokens;
            let reset_after = if refill_per_second > 0.0 {
                whole_units(deficit / refill_per_second)
            } else {
                window
            };
            return Admission::Allowed {
                limit: config.sustained.rate,
                remaining: u32::try_from(whole_units(remaining)).unwrap_or(u32::MAX),
                reset_after,
            };
        }

        let missing = cost - entry.tokens;
        let retry_after = if refill_per_second > 0.0 {
            whole_units(missing / refill_per_second).max(1)
        } else {
            window
        };
        match config.strategy {
            RateLimitStrategy::Degrade => Admission::Degraded {
                limit: config.sustained.rate,
            },
            RateLimitStrategy::Reject | RateLimitStrategy::Queue => Admission::Rejected {
                limit: config.sustained.rate,
                retry_after,
            },
        }
    }

    /// Drop buckets that have been idle for longer than the given period.
    pub fn evict_idle(&self, idle_for: Duration) {
        let now = Instant::now();
        self.buckets
            .retain(|_, bucket| now.saturating_duration_since(bucket.last_refill) < idle_for);
    }
}
// @cpt-end:cpt-cf-oagw-dod-policy-rate-limiting:p2:inst-ratelimit

#[cfg(test)]
mod tests {
    use super::{Admission, RateLimiter};
    use crate::domain::model::{
        BurstRate, RateLimitAlgorithm, RateLimitConfig, RateLimitScope, RateLimitStrategy,
        RateLimitWindow, SharingMode, SustainedRate,
    };

    fn config(rate: u32, burst: u32, strategy: RateLimitStrategy) -> RateLimitConfig {
        RateLimitConfig {
            sharing: SharingMode::Private,
            algorithm: RateLimitAlgorithm::TokenBucket,
            sustained: SustainedRate {
                rate,
                window: RateLimitWindow::Minute,
            },
            burst: Some(BurstRate { capacity: burst }),
            scope: RateLimitScope::Tenant,
            strategy,
            cost: 1,
        }
    }

    #[test]
    fn requests_within_the_burst_are_admitted() {
        let limiter = RateLimiter::new();
        let cfg = config(60, 2, RateLimitStrategy::Reject);
        assert!(matches!(
            limiter.check("k", &cfg),
            Admission::Allowed { .. }
        ));
        assert!(matches!(
            limiter.check("k", &cfg),
            Admission::Allowed { .. }
        ));
    }

    #[test]
    fn exceeding_the_burst_is_rejected_with_a_retry_hint() {
        let limiter = RateLimiter::new();
        let cfg = config(60, 1, RateLimitStrategy::Reject);
        assert!(matches!(
            limiter.check("k", &cfg),
            Admission::Allowed { .. }
        ));
        match limiter.check("k", &cfg) {
            Admission::Rejected { limit, retry_after } => {
                assert_eq!(limit, 60);
                assert!(retry_after >= 1);
            }
            other => panic!("expected rejection, got {other:?}"),
        }
    }

    #[test]
    fn the_degrade_strategy_admits_instead_of_rejecting() {
        let limiter = RateLimiter::new();
        let cfg = config(60, 1, RateLimitStrategy::Degrade);
        assert!(matches!(
            limiter.check("k", &cfg),
            Admission::Allowed { .. }
        ));
        assert!(matches!(
            limiter.check("k", &cfg),
            Admission::Degraded { .. }
        ));
    }

    #[test]
    fn separate_keys_have_separate_buckets() {
        let limiter = RateLimiter::new();
        let cfg = config(60, 1, RateLimitStrategy::Reject);
        assert!(matches!(
            limiter.check("a", &cfg),
            Admission::Allowed { .. }
        ));
        assert!(matches!(
            limiter.check("b", &cfg),
            Admission::Allowed { .. }
        ));
    }

    #[test]
    fn scope_key_discriminates_by_scope() {
        let tenant = RateLimiter::scope_key(RateLimitScope::Tenant, "u1", "t1", "s1", "ip", "r1");
        let user = RateLimiter::scope_key(RateLimitScope::User, "u1", "t1", "s1", "ip", "r1");
        assert_ne!(tenant, user);
        assert!(tenant.starts_with("u1:"));
    }
}
