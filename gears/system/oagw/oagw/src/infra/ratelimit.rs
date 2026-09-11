// Created: 2026-09-02 by Constructor Tech
//! Rate limiting (`ADR-0003`).
//!
//! Token buckets are kept in a shared map keyed by
//! `(config fingerprint, scope key)`. A *fingerprint* identifies the effective
//! limit configuration, so reconfiguring an upstream starts a fresh bucket
//! rather than inheriting a stale one, and two upstreams with different limits
//! never share a bucket.
//!
//! Hierarchical limits compose with `min()`: the effective limit is the
//! smallest of the selected upstream's rate, the route's rate and every
//! ancestor-enforced rate on the chain.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use uuid::Uuid;

use crate::domain::model::{RateLimitAlgorithm, RateLimitConfig};
use crate::error::GatewayError;

/// Which bucket a request is accounted against.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ScopeKey {
    /// Algorithm + rates, so a config change starts a fresh bucket.
    pub fingerprint: String,
    /// Scope discriminator (`global` / `tenant:{uuid}` / `user:{id}` / …).
    pub subject: String,
}

/// A single token bucket.
#[derive(Debug)]
struct Bucket {
    tokens: f64,
    capacity: f64,
    refill_per_sec: f64,
    updated: Instant,
}

impl Bucket {
    fn new(capacity: u64, refill_per_sec: f64, now: Instant) -> Self {
        Self {
            tokens: capacity as f64,
            capacity: capacity as f64,
            refill_per_sec,
            updated: now,
        }
    }

    /// Takes `cost` tokens, returning `(tokens left, wait if short)`.
    fn try_take(&mut self, cost: u64, now: Instant) -> (u64, Option<Duration>) {
        let elapsed = now.duration_since(self.updated).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        self.updated = now;
        let cost = (cost.max(1)) as f64;
        if self.tokens >= cost {
            self.tokens -= cost;
            (self.tokens.floor().max(0.0) as u64, None)
        } else {
            let missing = cost - self.tokens;
            let wait = Duration::from_secs_f64(
                (missing / self.refill_per_sec.max(f64::MIN_POSITIVE)).min(3600.0),
            );
            (0, Some(wait))
        }
    }
}

/// The counters.
#[derive(Debug, Default)]
pub struct RateLimiter {
    buckets: DashMap<ScopeKey, Bucket>,
}

impl RateLimiter {
    /// Creates an empty limiter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds the scope key for a request.
    #[must_use]
    pub fn scope_key(config: &RateLimitConfig, tenant_id: Uuid, subject_id: &str, ip: &str, route_id: &str) -> ScopeKey {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        serde_json::to_string(config).unwrap_or_default().hash(&mut hasher);
        ScopeKey {
            fingerprint: format!("{:016x}", hasher.finish()),
            subject: match config.scope {
                crate::domain::model::RateScope::Global => "global".to_owned(),
                crate::domain::model::RateScope::Tenant => format!("tenant:{tenant_id}"),
                crate::domain::model::RateScope::User => format!("user:{subject_id}"),
                crate::domain::model::RateScope::Ip => format!("ip:{ip}"),
                crate::domain::model::RateScope::Route => format!("route:{route_id}"),
            },
        }
    }

    /// Consumes `cost` tokens, or returns the `429` problem to return.
    ///
    /// # Errors
    ///
    /// [`GatewayError::RateLimited`] when the bucket has no tokens left.
    pub fn check(
        &self,
        config: &RateLimitConfig,
        key: &ScopeKey,
        cost: u64,
    ) -> Result<RateDecision, GatewayError> {
        if config.algorithm == RateLimitAlgorithm::SlidingWindow {
            return self.check_window(config, key, cost);
        }
        let capacity = config.capacity().max(1);
        let refill = 1.0 / config.refill_period().as_secs_f64();
        let mut entry = self
            .buckets
            .entry(key.clone())
            .or_insert_with(|| Bucket::new(capacity, refill, Instant::now()));
        // A bucket created for a different capacity is reset, so an operator
        // raising the burst limit takes effect immediately.
        if (entry.capacity - capacity as f64).abs() > f64::EPSILON {
            *entry = Bucket::new(capacity, refill, Instant::now());
        }
        let (remaining, wait) = entry.try_take(cost, Instant::now());
        match wait {
            None => Ok(RateDecision::Allowed { limit: capacity, remaining }),
            Some(wait) => {
                let reset = wait.as_secs().max(1);
                Err(GatewayError::RateLimited {
                    detail: "rate limit exceeded for this upstream".to_owned(),
                    limit: capacity,
                    remaining: 0,
                    reset_secs: reset,
                    retry_after_secs: reset,
                })
            }
        }
    }

    /// Sliding-window accounting: `rate` requests per `window`.
    fn check_window(
        &self,
        config: &RateLimitConfig,
        key: &ScopeKey,
        cost: u64,
    ) -> Result<RateDecision, GatewayError> {
        let window = config.sustained.window.duration();
        let now = Instant::now();
        let limit = config.capacity().max(1);
        let mut entry = self
            .buckets
            .entry(key.clone())
            .or_insert_with(|| Bucket {
                tokens: limit as f64,
                capacity: limit as f64,
                refill_per_sec: 0.0,
                updated: now,
            });
        // The sliding window reuses the bucket's token field as a counter that
        // drains linearly over the window.
        let elapsed = now.duration_since(entry.updated).as_secs_f64();
        let drain = elapsed * (limit.max(1)) as f64 / window.as_secs_f64();
        entry.tokens = (entry.tokens + drain).min(limit as f64);
        entry.updated = now;
        let cost = (cost.max(1)) as f64;
        if entry.tokens >= cost {
            entry.tokens -= cost;
            let remaining = entry.tokens.floor().max(0.0) as u64;
            Ok(RateDecision::Allowed { limit, remaining })
        } else {
            let needed = (cost - entry.tokens) / ((limit.max(1)) as f64 / window.as_secs_f64());
            let wait = Duration::from_secs_f64(needed.min(window.as_secs_f64()));
            let reset = wait.as_secs().max(1);
            Err(GatewayError::RateLimited {
                detail: "rate limit exceeded for this upstream".to_owned(),
                limit,
                remaining: 0,
                reset_secs: reset,
                retry_after_secs: reset,
            })
        }
    }
}

/// Outcome of a rate-limit check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateDecision {
    /// The request may proceed; the remaining quota is reported.
    Allowed {
        /// Configured bucket capacity.
        limit: u64,
        /// Tokens left after this request.
        remaining: u64,
    },
}

/// The effective rate limit for a request: `min()` over the chain (`DESIGN.md`).
///
/// `candidates` are `(tenant_depth, config)` pairs; `None` when no candidate
/// applies a limit.
#[must_use]
pub fn effective_limit(candidates: &[RateLimitConfig]) -> Option<RateLimitConfig> {
    let mut best: Option<RateLimitConfig> = None;
    for candidate in candidates {
        best = Some(match best {
            None => candidate.clone(),
            Some(current) => {
                if candidate_permits_fewer(&current, candidate) {
                    candidate.clone()
                } else {
                    current
                }
            }
        });
    }
    best
}

/// Whether `candidate` is stricter than `current`.
fn candidate_permits_fewer(current: &RateLimitConfig, candidate: &RateLimitConfig) -> bool {
    let current_rate = per_second(current);
    let candidate_rate = per_second(candidate);
    if (candidate_rate - current_rate).abs() < f64::EPSILON {
        candidate.capacity() < current.capacity()
    } else {
        candidate_rate < current_rate
    }
}

fn per_second(config: &RateLimitConfig) -> f64 {
    let window = config.sustained.window.duration().as_secs_f64();
    (config.sustained.rate.max(1)) as f64 / window.max(1.0)
}

/// A limiter shared by the whole gear.
pub type SharedRateLimiter = Arc<RateLimiter>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{BurstConfig, RateScope, RateWindow, SustainedRate};

    fn config(rate: u64, window: RateWindow, capacity: Option<u64>) -> RateLimitConfig {
        RateLimitConfig {
            sharing: crate::domain::model::Sharing::Private,
            algorithm: RateLimitAlgorithm::TokenBucket,
            sustained: SustainedRate { rate, window },
            burst: capacity.map(|capacity| BurstConfig { capacity }),
            scope: RateScope::Tenant,
            strategy: crate::domain::model::RateStrategy::Reject,
            cost: 1,
        }
    }

    #[test]
    fn a_bucket_admits_bursts_up_to_capacity() {
        let limiter = RateLimiter::new();
        let cfg = config(1, RateWindow::Second, Some(3));
        let key = RateLimiter::scope_key(&cfg, Uuid::nil(), "u", "127.0.0.1", "r");
        assert!(limiter.check(&cfg, &key, 1).is_ok());
        assert!(limiter.check(&cfg, &key, 1).is_ok());
        assert!(limiter.check(&cfg, &key, 1).is_ok());
        let err = limiter.check(&cfg, &key, 1).unwrap_err();
        match err {
            GatewayError::RateLimited { limit, retry_after_secs, .. } => {
                assert_eq!(limit, 3);
                assert!(retry_after_secs >= 1);
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn buckets_are_isolated_by_scope() {
        let limiter = RateLimiter::new();
        let cfg = config(1, RateWindow::Second, Some(1));
        let a = RateLimiter::scope_key(&cfg, Uuid::from_u128(1), "a", "1.1.1.1", "r");
        let b = RateLimiter::scope_key(&cfg, Uuid::from_u128(2), "b", "2.2.2.2", "r");
        assert!(limiter.check(&cfg, &a, 1).is_ok());
        assert!(limiter.check(&cfg, &b, 1).is_ok(), "another subject has its own bucket");
        assert!(limiter.check(&cfg, &a, 1).is_err());
    }

    #[test]
    fn buckets_are_isolated_by_config_fingerprint() {
        let limiter = RateLimiter::new();
        let first = config(1, RateWindow::Second, Some(1));
        let second = config(2, RateWindow::Second, Some(1));
        let other = key_of(&second, "u");
        assert!(limiter.check(&first, &key_of(&first, "u"), 1).is_ok());
        assert!(limiter.check(&second, &other, 1).is_ok());
    }

    fn key_of(cfg: &RateLimitConfig, subject: &str) -> ScopeKey {
        RateLimiter::scope_key(cfg, Uuid::nil(), subject, "ip", "r")
    }

    #[test]
    fn tokens_replenish_over_time() {
        let limiter = RateLimiter::new();
        let cfg = config(1000, RateWindow::Second, Some(1));
        let key = RateLimiter::scope_key(&cfg, Uuid::nil(), "u", "ip", "r");
        assert!(limiter.check(&cfg, &key, 1).is_ok());
        assert!(limiter.check(&cfg, &key, 1).is_err());
        std::thread::sleep(Duration::from_millis(5));
        assert!(limiter.check(&cfg, &key, 1).is_ok(), "the bucket refills at 1000 tokens/s");
    }

    #[test]
    fn cost_weighted_requests_consume_more() {
        let limiter = RateLimiter::new();
        let mut cfg = config(1, RateWindow::Second, Some(6));
        cfg.cost = 3;
        let key = RateLimiter::scope_key(&cfg, Uuid::nil(), "u", "ip", "r");
        assert!(limiter.check(&cfg, &key, cfg.cost).is_ok());
        assert!(limiter.check(&cfg, &key, cfg.cost).is_ok());
        assert!(limiter.check(&cfg, &key, cfg.cost).is_err());
    }

    #[test]
    fn sliding_window_rejects_when_the_window_is_saturated() {
        let limiter = RateLimiter::new();
        let mut cfg = config(2, RateWindow::Second, None);
        cfg.algorithm = RateLimitAlgorithm::SlidingWindow;
        let key = RateLimiter::scope_key(&cfg, Uuid::nil(), "u", "ip", "r");
        assert!(limiter.check(&cfg, &key, 1).is_ok());
        assert!(limiter.check(&cfg, &key, 1).is_ok());
        assert!(limiter.check(&cfg, &key, 1).is_err());
    }

    #[test]
    fn the_strictest_candidate_wins() {
        let loose = config(100, RateWindow::Minute, Some(100));
        let strict = config(1, RateWindow::Second, Some(2));
        let effective = effective_limit(&[loose.clone(), strict.clone()]).unwrap();
        assert_eq!(effective.sustained.rate, 1);
        assert_eq!(effective.capacity(), 2);
        assert_eq!(effective_limit(&[loose]).unwrap().sustained.rate, 100);
        assert!(effective_limit(&[]).is_none());
    }

    #[test]
    fn scope_keys_name_their_subject() {
        let cfg = config(1, RateWindow::Second, None);
        let tenant = Uuid::from_u128(7);
        assert_eq!(
            RateLimiter::scope_key(&cfg, tenant, "u", "ip", "r").subject,
            format!("tenant:{tenant}")
        );
        let mut cfg = cfg.clone();
        cfg.scope = RateScope::Ip;
        assert_eq!(RateLimiter::scope_key(&cfg, tenant, "u", "10.0.0.1", "r").subject, "ip:10.0.0.1");
        cfg.scope = RateScope::Route;
        assert!(RateLimiter::scope_key(&cfg, tenant, "u", "ip", "route-1").subject.starts_with("route:"));
        cfg.scope = RateScope::Global;
        assert_eq!(RateLimiter::scope_key(&cfg, tenant, "u", "ip", "r").subject, "global");
    }
}
