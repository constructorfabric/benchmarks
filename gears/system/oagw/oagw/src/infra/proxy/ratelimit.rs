//! Token-bucket rate limiter registry.
//!
//! Implements `docs/ADR/0003-rate-limiting.md`.

use std::collections::HashMap;
use std::time::Instant;

use parking_lot::Mutex;
use uuid::Uuid;

use crate::domain::model::{RateLimitConfig, RateScope};

/// Outcome of a token-bucket attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateOutcome {
    /// Whether the request was allowed.
    pub allowed: bool,
    /// Effective bucket capacity.
    pub limit: u64,
    /// Tokens left after the attempt.
    pub remaining: u64,
    /// Seconds until the bucket is fully replenished.
    pub reset_secs: u64,
    /// Seconds until the next token becomes available.
    pub retry_after_secs: u64,
}

/// One token bucket.
#[derive(Debug)]
struct Bucket {
    tokens: f64,
    capacity: f64,
    refill_per_sec: f64,
    last_refill: Instant,
}

impl Bucket {
    #[allow(
        clippy::cast_precision_loss,
        reason = "bucket capacities are small integers; f64 represents them exactly"
    )]
    fn new(capacity: u64, refill_per_sec: f64, now: Instant) -> Self {
        Self {
            tokens: capacity as f64,
            capacity: capacity as f64,
            refill_per_sec,
            last_refill: now,
        }
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now
            .saturating_duration_since(self.last_refill)
            .as_secs_f64();
        if elapsed > 0.0 {
            self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
            self.last_refill = now;
        }
    }

    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "token arithmetic is f64 by construction; the reported fields are integer projections of it"
    )]
    fn try_take(&mut self, cost: u64, now: Instant) -> RateOutcome {
        self.refill(now);
        let need = cost as f64;
        let full_secs = if self.tokens >= self.capacity {
            0
        } else {
            ((self.capacity - self.tokens) / self.refill_per_sec).ceil() as u64
        };
        if self.tokens >= need {
            self.tokens -= need;
            RateOutcome {
                allowed: true,
                limit: self.capacity as u64,
                remaining: self.tokens.floor().max(0.0) as u64,
                reset_secs: full_secs,
                retry_after_secs: 0,
            }
        } else {
            let deficit = need - self.tokens;
            RateOutcome {
                allowed: false,
                limit: self.capacity as u64,
                remaining: 0,
                reset_secs: full_secs,
                retry_after_secs: (deficit / self.refill_per_sec).ceil().max(1.0) as u64,
            }
        }
    }
}

/// Counter key of a rate limiter.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CounterKey {
    upstream_id: Uuid,
    route_id: Option<Uuid>,
    scope: RateScope,
    subject: String,
}

/// Concurrent registry of rate-limit buckets.
#[derive(Debug, Default)]
pub struct RateLimitRegistry {
    buckets: Mutex<HashMap<CounterKey, Bucket>>,
}

impl RateLimitRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Attempt to consume `cost` tokens for the effective configuration.
    #[must_use]
    pub fn check(
        &self,
        upstream_id: Uuid,
        route_id: Option<Uuid>,
        scope: RateScope,
        subject: &str,
        effective: &EffectiveRate,
    ) -> RateOutcome {
        let key = CounterKey {
            upstream_id,
            route_id,
            scope,
            subject: subject.to_owned(),
        };
        let now = Instant::now();
        let mut buckets = self.buckets.lock();
        let bucket = buckets
            .entry(key)
            .or_insert_with(|| Bucket::new(effective.capacity, effective.refill_per_sec, now));
        bucket.try_take(effective.cost, now)
    }
}

/// Effective rate after the hierarchical `min()` merge.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EffectiveRate {
    /// Bucket capacity (burst, or the sustained rate).
    pub capacity: u64,
    /// Tokens replenished per second.
    pub refill_per_sec: f64,
    /// Tokens consumed per request.
    pub cost: u64,
    /// Counter scope of the strictest contributing configuration.
    pub scope: RateScope,
    /// Whether `X-RateLimit-*` headers should be emitted.
    pub response_headers: bool,
}

impl EffectiveRate {
    /// Build the effective rate from every contributing configuration,
    /// ordered from the loosest (ancestor upstream) to the strictest (route).
    ///
    /// Rates merge with `min()`; later entries win for cost and scope.
    #[must_use]
    pub fn merge(configs: &[&RateLimitConfig]) -> Option<Self> {
        // Configurations without a sustained rate contribute nothing.
        let active: Vec<&RateLimitConfig> = configs
            .iter()
            .copied()
            .filter(|c| c.sustained.is_some())
            .collect();
        let first = *active.first()?;

        let capacity = active
            .iter()
            .map(|c| c.effective_capacity())
            .min()
            .unwrap_or_else(|| first.effective_capacity());
        let refill_per_sec = active
            .iter()
            .map(|c| c.sustained_per_sec())
            .fold(f64::INFINITY, f64::min);
        let cost = configs
            .iter()
            .rev()
            .find(|c| c.cost > 0)
            .map_or(1, |c| c.cost);
        let scope = active.last().map_or(first.scope, |c| c.scope);
        let response_headers = active.iter().all(|c| c.response_headers);

        Some(Self {
            capacity: capacity.max(1),
            refill_per_sec: refill_per_sec.max(f64::MIN_POSITIVE),
            cost: cost.max(1),
            scope,
            response_headers,
        })
    }
}

impl RateLimitConfig {
    /// Burst capacity, or the sustained rate when no burst is configured.
    #[must_use]
    pub fn effective_capacity(&self) -> u64 {
        self.burst
            .map_or(1, |b| b.capacity)
            .max(self.sustained.map_or(1, |s| s.rate))
    }

    /// Sustained replenishment rate in tokens per second.
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        reason = "rates are small integers; the quotient is deliberately approximate"
    )]
    pub fn sustained_per_sec(&self) -> f64 {
        let Some(sustained) = self.sustained else {
            return 1.0;
        };
        let window = sustained.window.seconds().max(1);
        sustained.rate as f64 / window as f64
    }
}

/// Resolve the subject that identifies the counter.
#[must_use]
pub fn counter_subject(
    scope: RateScope,
    tenant_id: Uuid,
    subject_id: &str,
    remote_ip: &str,
    route_id: Option<Uuid>,
) -> String {
    match scope {
        RateScope::Global => "global".to_owned(),
        RateScope::Tenant => tenant_id.to_string(),
        RateScope::User => format!("{tenant_id}:{subject_id}"),
        RateScope::Ip => remote_ip.to_owned(),
        RateScope::Route => format!(
            "{}:{}",
            tenant_id,
            route_id.map_or_else(|| "none".to_owned(), |id| id.to_string())
        ),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::domain::model::BurstConfig;
    use crate::domain::model::{RateWindow, SustainedRate};

    fn config(rate: u64, capacity: u64) -> RateLimitConfig {
        RateLimitConfig {
            sustained: Some(SustainedRate {
                rate,
                window: RateWindow::Second,
            }),
            burst: Some(BurstConfig { capacity }),
            ..RateLimitConfig::default()
        }
    }

    #[test]
    fn third_request_is_rejected_at_capacity_two() {
        let registry = RateLimitRegistry::new();
        let effective = EffectiveRate::merge(&[&config(2, 2)]).unwrap();
        let upstream = Uuid::new_v4();
        assert!(
            registry
                .check(upstream, None, RateScope::Tenant, "t", &effective)
                .allowed
        );
        assert!(
            registry
                .check(upstream, None, RateScope::Tenant, "t", &effective)
                .allowed
        );
        let third = registry.check(upstream, None, RateScope::Tenant, "t", &effective);
        assert!(!third.allowed);
        assert_eq!(third.retry_after_secs, 1);
        assert_eq!(third.limit, 2);
        assert_eq!(third.remaining, 0);
    }

    #[test]
    fn different_subjects_get_separate_buckets() {
        let registry = RateLimitRegistry::new();
        let effective = EffectiveRate::merge(&[&config(1, 1)]).unwrap();
        let upstream = Uuid::new_v4();
        assert!(
            registry
                .check(upstream, None, RateScope::User, "a", &effective)
                .allowed
        );
        assert!(
            registry
                .check(upstream, None, RateScope::User, "b", &effective)
                .allowed
        );
        assert!(
            !registry
                .check(upstream, None, RateScope::User, "a", &effective)
                .allowed
        );
    }

    #[test]
    fn route_limit_merges_with_min() {
        let upstream = config(100, 100);
        let route = config(1, 1);
        let merged = EffectiveRate::merge(&[&upstream, &route]).unwrap();
        assert_eq!(merged.capacity, 1);
        assert!((merged.refill_per_sec - 1.0).abs() < f64::EPSILON);
        assert_eq!(merged.scope, RateScope::Tenant);
    }

    #[test]
    fn upstream_limit_alone_is_used_without_a_route() {
        let merged = EffectiveRate::merge(&[&config(5, 5)]).unwrap();
        assert_eq!(merged.capacity, 5);
        assert!((merged.refill_per_sec - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn window_scales_the_refill_rate() {
        let cfg = RateLimitConfig {
            sustained: Some(SustainedRate {
                rate: 60,
                window: RateWindow::Minute,
            }),
            burst: Some(BurstConfig { capacity: 60 }),
            ..RateLimitConfig::default()
        };
        let merged = EffectiveRate::merge(&[&cfg]).unwrap();
        assert!((merged.refill_per_sec - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn configs_without_a_sustained_rate_are_ignored() {
        let empty = RateLimitConfig::default();
        assert!(EffectiveRate::merge(&[&empty]).is_none());
    }

    #[test]
    fn counter_subject_scopes_are_distinct() {
        let tenant = Uuid::new_v4();
        assert_eq!(
            counter_subject(RateScope::Global, tenant, "s", "ip", None),
            "global"
        );
        assert_eq!(
            counter_subject(RateScope::Tenant, tenant, "s", "ip", None),
            tenant.to_string()
        );
        assert_eq!(
            counter_subject(RateScope::Ip, tenant, "s", "1.2.3.4", None),
            "1.2.3.4"
        );
        assert_eq!(
            counter_subject(RateScope::User, tenant, "s", "ip", None),
            format!("{tenant}:s")
        );
    }

    #[test]
    fn response_headers_require_every_layer_to_opt_in() {
        let mut quiet = config(2, 2);
        quiet.response_headers = false;
        assert!(
            !EffectiveRate::merge(&[&quiet, &config(2, 2)])
                .unwrap()
                .response_headers
        );
        assert!(
            EffectiveRate::merge(&[&config(2, 2), &config(2, 2)])
                .unwrap()
                .response_headers
        );
    }
}
