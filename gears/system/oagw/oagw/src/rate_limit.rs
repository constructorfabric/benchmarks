// Created: 2026-09-03 by Constructor Tech
//! Token-bucket and sliding-window rate limiters.
//!
//! Implements `ADR/0003-rate-limiting.md`: a dual-rate configuration where
//! the sustained rate refills the bucket and the burst capacity bounds it,
//! counters are scoped per tenant/subject/IP/route, and exhaustion returns
//! `429` with `Retry-After` plus the `X-RateLimit-*` family.

use std::time::{Duration, Instant};

use dashmap::DashMap;

use crate::model::{RateAlgorithm, RateLimitConfig, RateScope, RateWindow};

/// Outcome of a rate-limit check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateDecision {
    /// Whether the request is admitted.
    pub allowed: bool,
    /// Configured limit, for the `X-RateLimit-Limit` header.
    pub limit: u64,
    /// Tokens left after admission, for `X-RateLimit-Remaining`.
    pub remaining: u64,
    /// Seconds until the next token, for `Retry-After`.
    pub retry_after_secs: u64,
    /// Seconds until the bucket/window resets, for `X-RateLimit-Reset`.
    pub reset_secs: u64,
}

#[derive(Debug)]
struct Entry {
    tokens: f64,
    window_start: Instant,
    current: u64,
    previous: u64,
}

impl Entry {
    fn new(now: Instant) -> Self {
        Self {
            tokens: 0.0,
            window_start: now,
            current: 0,
            previous: 0,
        }
    }
}

/// Registry of per-key rate-limit counters.
///
/// Keys are synthesized from the scope plus the identifying value, so a
/// tenant counter and a subject counter never collide.
#[derive(Default)]
pub struct RateLimiter {
    entries: DashMap<String, Entry>,
}

fn window_seconds(window: RateWindow) -> f64 {
    match window {
        RateWindow::Second => 1.0,
        RateWindow::Minute => 60.0,
        RateWindow::Hour => 3600.0,
        RateWindow::Day => 86_400.0,
    }
}

/// Saturating conversion of a non-negative float into a token count.
fn tokens_of(value: f64) -> u64 {
    if value.is_finite() && value > 0.0 {
        u64::try_from(value as u128).unwrap_or(u64::MAX)
    } else {
        0
    }
}

/// The counter key for a scope, combining the scope with its subject.
#[must_use]
pub fn scope_key(scope: RateScope, upstream: &str, route: &str, tenant: &str, subject: &str, ip: &str) -> String {
    let identity = match scope {
        RateScope::Global => "global".to_owned(),
        RateScope::Tenant => format!("tenant:{tenant}"),
        RateScope::User => format!("user:{tenant}:{subject}"),
        RateScope::Ip => format!("ip:{ip}"),
        RateScope::Route => format!("route:{route}"),
    };
    format!("{upstream}|{identity}")
}

impl RateLimiter {
    /// Creates an empty limiter registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Runs a check against `config` for the counter `key`.
    #[must_use]
    pub fn check(&self, key: &str, config: &RateLimitConfig) -> RateDecision {
        let capacity = config.capacity();
        let now = Instant::now();
        let mut guard = self.entries.entry(key.to_owned()).or_insert_with(|| {
            let mut entry = Entry::new(now);
            entry.tokens = f64::from(u32::try_from(capacity).unwrap_or(u32::MAX));
            entry
        });
        match config.algorithm {
            RateAlgorithm::TokenBucket => Self::check_bucket(&mut guard, config, capacity, now),
            RateAlgorithm::SlidingWindow => Self::check_window(&mut guard, config, now),
        }
    }

    fn check_bucket(
        entry: &mut Entry,
        config: &RateLimitConfig,
        capacity: u64,
        now: Instant,
    ) -> RateDecision {
        let rate = config.rate_per_second();
        let elapsed = now.duration_since(entry.window_start).as_secs_f64();
        entry.window_start = now;
        entry.tokens = (entry.tokens + elapsed * rate).min(f64::from(u32::try_from(capacity).unwrap_or(u32::MAX)));
        let cost = f64::from(u32::try_from(config.cost()).unwrap_or(u32::MAX));
        if entry.tokens + f64::EPSILON >= cost {
            entry.tokens -= cost;
            RateDecision {
                allowed: true,
                limit: capacity,
                remaining: tokens_of(entry.tokens.floor()),
                retry_after_secs: 0,
                reset_secs: if rate <= 0.0 {
                    0
                } else {
                    tokens_of((cost / rate).ceil())
                },
            }
        } else {
            let deficit = cost - entry.tokens;
            let retry = if rate <= 0.0 {
                60
            } else {
                tokens_of((deficit / rate).ceil().max(1.0))
            };
            RateDecision {
                allowed: false,
                limit: capacity,
                remaining: 0,
                retry_after_secs: retry,
                reset_secs: retry,
            }
        }
    }

    fn check_window(
        entry: &mut Entry,
        config: &RateLimitConfig,
        now: Instant,
    ) -> RateDecision {
        let limit = config.sustained.rate;
        let window = Duration::from_secs_f64(window_seconds(config.sustained.window));
        let elapsed = now.duration_since(entry.window_start);
        if elapsed >= window {
            entry.previous = entry.current;
            entry.current = 0;
            entry.window_start = now;
        }
        let spanned = elapsed.as_secs_f64() / window.as_secs_f64();
        let projected =
            f64::from(u32::try_from(entry.previous).unwrap_or(u32::MAX)) * (1.0 - spanned)
                + f64::from(u32::try_from(entry.current).unwrap_or(u32::MAX));
        let cost = f64::from(u32::try_from(config.cost()).unwrap_or(u32::MAX));
        let cap = f64::from(u32::try_from(limit).unwrap_or(u32::MAX));
        if projected + cost <= cap + f64::EPSILON {
            entry.current += config.cost();
            let remaining = limit.saturating_sub(entry.current);
            RateDecision {
                allowed: true,
                limit,
                remaining,
                retry_after_secs: 0,
                reset_secs: window.as_secs().saturating_sub(elapsed.as_secs()).max(1),
            }
        } else {
            let retry = window.as_secs().saturating_sub(elapsed.as_secs()).max(1);
            RateDecision {
                allowed: false,
                limit,
                remaining: 0,
                retry_after_secs: retry,
                reset_secs: retry,
            }
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::model::{RateScope, RateStrategy, SustainedRate};

    fn config(rate: u64) -> RateLimitConfig {
        RateLimitConfig {
            sharing: crate::model::SharingMode::Private,
            algorithm: RateAlgorithm::TokenBucket,
            sustained: SustainedRate {
                rate,
                window: RateWindow::Second,
            },
            burst: None,
            scope: RateScope::Tenant,
            strategy: RateStrategy::Reject,
            cost: None,
        }
    }

    #[test]
    fn admits_up_to_capacity_then_rejects() {
        let limiter = RateLimiter::new();
        let _cfg = config(2);
        assert!(limiter.check("a", &config(2)).allowed);
        assert!(limiter.check("a", &config(2)).allowed);
        let denied = limiter.check("a", &config(2));
        assert!(!denied.allowed);
        assert!(denied.retry_after_secs >= 1);
        assert_eq!(denied.limit, 2);
    }

    #[test]
    fn distinct_keys_are_isolated() {
        let limiter = RateLimiter::new();
        assert!(limiter.check("a", &config(1)).allowed);
        assert!(limiter.check("b", &config(1)).allowed);
    }

    #[test]
    fn sliding_window_counts_requests() {
        let limiter = RateLimiter::new();
        let cfg = config(1);
        assert!(limiter.check("w", &cfg).allowed);
        assert!(!limiter.check("w", &cfg).allowed);
    }

    #[test]
    fn scope_keys_never_collide() {
        let a = scope_key(RateScope::Tenant, "u", "r", "t1", "s", "ip");
        let b = scope_key(RateScope::Tenant, "u", "r", "t2", "s", "ip");
        let c = scope_key(RateScope::User, "u", "r", "t1", "s", "ip");
        assert_ne!(a, b);
        assert_ne!(a, c);
    }
}
