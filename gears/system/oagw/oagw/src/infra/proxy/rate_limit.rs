//! Rate limiting (ADR-0003): a token bucket and a sliding-window log.
//!
//! One state entry per resolved `(scope, key)` per configuration. The route's
//! configuration, when it declares one, tightens the upstream's; an ancestor's
//! enforced limit is never loosened by a descendant because the effective
//! configuration has already been merged to the tighter of the two.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use uuid::Uuid;

use crate::domain::dto::{RateAlgorithm, RateLimitConfig, RateScope};

/// A single token bucket.
#[derive(Debug)]
struct Bucket {
    tokens: f64,
    last_refill: Instant,
}

impl Bucket {
    // The bucket is a float accumulator by design, so this widening is the
    // boundary the algorithm needs, not a precision hazard to work around.
    #[allow(clippy::cast_precision_loss)]
    fn full(capacity: u64, now: Instant) -> Self {
        Self {
            tokens: capacity as f64,
            last_refill: now,
        }
    }
}

/// Verdict of a `try_consume` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateDecision {
    /// Whether the request may proceed.
    pub allowed: bool,
    /// Tokens still available.
    pub remaining: u64,
    /// Seconds until the bucket can satisfy the request again.
    pub retry_after: u64,
    /// Bucket capacity.
    pub limit: u64,
    /// Seconds until the bucket is full again.
    pub reset_seconds: u64,
}

/// Scope key identifying one bucket.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RateKey {
    /// One bucket for the whole gateway.
    Global,
    /// One bucket per tenant.
    Tenant(Uuid),
    /// One bucket per authenticated subject.
    User(Uuid),
    /// One bucket per client address.
    Ip(std::net::IpAddr),
    /// One bucket per route.
    Route(Uuid),
}

impl RateKey {
    /// Derives the bucket key for a configured scope.
    #[must_use]
    pub fn for_scope(
        scope: RateScope,
        tenant_id: Uuid,
        subject_id: Uuid,
        route_id: Option<Uuid>,
        client_ip: Option<std::net::IpAddr>,
    ) -> Self {
        match scope {
            RateScope::Global => Self::Global,
            RateScope::Tenant => Self::Tenant(tenant_id),
            RateScope::User => Self::User(subject_id),
            RateScope::Route => Self::Route(route_id.unwrap_or(tenant_id)),
            RateScope::Ip => Self::Ip(client_ip.unwrap_or(std::net::IpAddr::from([0, 0, 0, 0]))),
        }
    }
}

/// A sliding-window log: the instants at which the budget was spent.
#[derive(Debug, Default)]
struct WindowLog {
    /// One entry per unit of budget, so a cost of two occupies two slots.
    hits: VecDeque<Instant>,
}

impl WindowLog {
}

/// Rate limiter: a token bucket by default, a sliding-window log when the
/// configuration asks for one.
#[derive(Debug, Default)]
pub struct TokenBucketLimiter {
    buckets: DashMap<String, Bucket>,
    windows: DashMap<String, WindowLog>,
}

impl TokenBucketLimiter {
    /// A fresh limiter.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Consumes `cost` tokens from the bucket identified by `key`.
    ///
    // The token bucket is a float accumulator, so these casts are the
    // deliberate integer↔float boundary of the algorithm. Every `f64 → u64`
    // cast below is already clamped by a preceding `.floor()`/`.ceil()` plus a
    // `.max(...)`, so no value can be truncated or negative; rewriting the
    // arithmetic through `try_from` would add a branch to a hot path without
    // changing a single result.
    #[must_use]
    pub fn consume(
        &self,
        key: &RateKey,
        config: &RateLimitConfig,
        cost: u64,
        now: Instant,
    ) -> RateDecision {
        match config.algorithm {
            RateAlgorithm::TokenBucket => self.consume_tokens(key, config, cost, now),
            RateAlgorithm::SlidingWindow => self.consume_window(key, config, cost, now),
        }
    }

    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn consume_tokens(
        &self,
        key: &RateKey,
        config: &RateLimitConfig,
        cost: u64,
        now: Instant,
    ) -> RateDecision {
        let capacity = config.capacity().max(1) as f64;
        let refill = config.refill_per_second().max(f64::MIN_POSITIVE);
        let id = bucket_id(key, config);
        let mut entry = self
            .buckets
            .entry(id)
            .or_insert_with(|| Bucket::full(config.capacity().max(1), now));

        let elapsed = now.saturating_duration_since(entry.last_refill).as_secs_f64();
        entry.tokens = (entry.tokens + elapsed * refill).min(capacity);
        entry.last_refill = now;

        let needed = cost.max(1) as f64;
        let allowed = entry.tokens >= needed;
        if allowed {
            entry.tokens -= needed;
        }
        let deficit = (needed - entry.tokens).max(0.0);
        let retry_after = if allowed {
            0
        } else {
            (deficit / refill).ceil().max(1.0) as u64
        };
        let reset_seconds = ((capacity - entry.tokens) / refill).ceil().max(0.0) as u64;
        RateDecision {
            allowed,
            remaining: entry.tokens.floor().max(0.0) as u64,
            retry_after,
            limit: config.capacity().max(1),
            reset_seconds,
        }
    }

    /// Spends `cost` units of a sliding-window budget.
    ///
    /// The log is trimmed to the window first, so a request is admitted only
    /// while the window still has room for it: this is what prevents the
    /// boundary burst a fixed window would allow.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn consume_window(
        &self,
        key: &RateKey,
        config: &RateLimitConfig,
        cost: u64,
        now: Instant,
    ) -> RateDecision {
        let limit = config.sustained.rate.max(1);
        let window = Duration::from_secs(config.sustained.window.seconds());
        let id = bucket_id(key, config);
        let mut entry = self.windows.entry(id).or_default();

        while entry
            .hits
            .front()
            .is_some_and(|at| now.saturating_duration_since(*at) > window)
        {
            entry.hits.pop_front();
        }
        let used = entry.hits.len().min(usize::try_from(limit).unwrap_or(usize::MAX)) as u64;
        let allowed = used + cost <= limit;
        if allowed {
            for _ in 0..cost {
                entry.hits.push_back(now);
            }
        }
        // A refused caller waits for the oldest hit to leave the window, since
        // that is the first slot that frees up.
        let waits = entry
            .hits
            .front()
            .map_or(Duration::ZERO, |at| {
                window.saturating_sub(now.saturating_duration_since(*at))
            });
        let retry_after = if allowed { 0 } else { wait_seconds(waits) };
        let remaining = limit.saturating_sub(used + u64::from(allowed) * cost);
        RateDecision {
            allowed,
            remaining,
            retry_after,
            limit,
            reset_seconds: wait_seconds(waits),
        }
    }
}

/// Rounds a wait up to a whole second, and never reports zero for a refusal.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn wait_seconds(wait: Duration) -> u64 {
    #[allow(clippy::cast_sign_loss)]
    let secs = wait.as_secs_f64().ceil().max(0.0) as u64;
    secs.max(1)
}

fn bucket_id(key: &RateKey, config: &RateLimitConfig) -> String {
    let scope = match key {
        RateKey::Global => "global".to_owned(),
        RateKey::Tenant(id) => format!("tenant:{id}"),
        RateKey::User(id) => format!("user:{id}"),
        RateKey::Ip(ip) => format!("ip:{ip}"),
        RateKey::Route(id) => format!("route:{id}"),
    };
    format!(
        "{scope}|{}|{}|{}",
        config.sustained.rate,
        config.sustained.window.seconds(),
        config.capacity()
    )
}

/// Evaluates a rate-limit configuration for a request.
///
/// `None` means no limit applies. The route's configuration, when present,
/// replaces the upstream's.
#[must_use]
pub fn effective_rate_limit(
    upstream: Option<&RateLimitConfig>,
    route: Option<&RateLimitConfig>,
) -> Option<RateLimitConfig> {
    // Merge, not override: the route may tighten its upstream's budget but
    // never loosen it, which is what `min` gives on both capacity and refill.
    crate::domain::services::resolution::merge_rate_limit(upstream, route)
}

/// Builds the `429` problem detail from a decision.
#[must_use]
pub fn rejection_detail(decision: &RateDecision, config: &RateLimitConfig) -> String {
    format!(
        "rate limit of {} requests per {} exceeded under the {} algorithm; retry in {}s",
        decision.limit,
        config.sustained.window.seconds(),
        algorithm_name(config),
        decision.retry_after
    )
}

/// Extra headers a `429` response must carry.
#[must_use]
pub fn rate_limit_headers(decision: &RateDecision) -> Vec<(String, String)> {
    let mut headers = vec![
        ("retry-after".to_owned(), decision.retry_after.to_string()),
        ("x-ratelimit-limit".to_owned(), decision.limit.to_string()),
        ("x-ratelimit-remaining".to_owned(), decision.remaining.to_string()),
        (
            "x-ratelimit-reset".to_owned(),
            decision.reset_seconds.to_string(),
        ),
    ];
    headers.dedup();
    headers
}

/// The algorithm a configuration selects.
#[must_use]
pub fn algorithm_name(config: &RateLimitConfig) -> &'static str {
    match config.algorithm {
        RateAlgorithm::TokenBucket => "token_bucket",
        RateAlgorithm::SlidingWindow => "sliding_window",
    }
}

/// Convenience for building a sorted map from the rate-limit headers.
#[must_use]
pub fn header_map(decision: &RateDecision) -> BTreeMap<String, String> {
    rate_limit_headers(decision).into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use crate::domain::dto::{Burst, RateStrategy, RateWindow, SustainedRate};

    fn config(rate: u64, capacity: u64) -> RateLimitConfig {
        RateLimitConfig {
            sharing: crate::domain::dto::Sharing::Private,
            algorithm: RateAlgorithm::TokenBucket,
            sustained: SustainedRate {
                rate,
                window: RateWindow::Second,
            },
            burst: Some(Burst { capacity }),
            scope: RateScope::Tenant,
            strategy: RateStrategy::Reject,
            cost: 1,
        }
    }

    fn now() -> Instant {
        Instant::now()
    }

    #[test]
    fn bursts_up_to_capacity_then_rejects() {
        let limiter = TokenBucketLimiter::new();
        let config = config(1, 3);
        let key = RateKey::Tenant(Uuid::nil());
        assert!(limiter.consume(&key, &config, 1, now()).allowed);
        assert!(limiter.consume(&key, &config, 1, now()).allowed);
        assert!(limiter.consume(&key, &config, 1, now()).allowed);
        let decision = limiter.consume(&key, &config, 1, now());
        assert!(!decision.allowed);
        assert!(decision.retry_after >= 1);
    }

    #[test]
    fn cost_is_honoured() {
        let limiter = TokenBucketLimiter::new();
        let mut config = config(1, 4);
        config.cost = 3;
        let key = RateKey::Tenant(Uuid::nil());
        assert!(limiter.consume(&key, &config, 3, now()).allowed);
        assert!(!limiter.consume(&key, &config, 3, now()).allowed);
    }

    #[test]
    fn separate_scopes_have_separate_buckets() {
        let limiter = TokenBucketLimiter::new();
        let config = config(1, 1);
        let tenant = RateKey::Tenant(Uuid::new_v4());
        assert!(limiter.consume(&tenant, &config, 1, now()).allowed);
        assert!(!limiter.consume(&tenant, &config, 1, now()).allowed);
        assert!(limiter.consume(&RateKey::Global, &config, 1, now()).allowed);
    }

    #[test]
    fn headers_carry_the_budget() {
        let limiter = TokenBucketLimiter::new();
        let config = config(10, 10);
        let decision = limiter.consume(&RateKey::Tenant(Uuid::nil()), &config, 1, now());
        let headers = header_map(&decision);
        assert_eq!(headers.get("x-ratelimit-limit").map(String::as_str), Some("10"));
        assert_eq!(headers.get("retry-after").map(String::as_str), Some("0"));
    }

    #[test]
    fn refill_recovers_tokens_over_time() {
        let limiter = TokenBucketLimiter::new();
        let config = config(1000, 1);
        let key = RateKey::Tenant(Uuid::nil());
        assert!(limiter.consume(&key, &config, 1, now()).allowed);
        let drained = limiter.consume(&key, &config, 1, now());
        assert!(!drained.allowed);
        let decision = limiter.consume(&key, &config, 1, now() + Duration::from_millis(20));
        assert!(decision.allowed, "1000/s refills a token in 1ms");
    }

    #[test]
    fn route_config_overrides_the_upstream() {
        let upstream = config(10, 10);
        let route = config(1, 1);
        let effective = effective_rate_limit(Some(&upstream), Some(&route)).expect("route wins");
        assert_eq!(effective.capacity(), 1);
        assert!(effective_rate_limit(Some(&upstream), None).is_some());
        assert!(effective_rate_limit(None, None).is_none());
    }

    #[test]
    fn a_looser_route_cannot_loosen_its_upstream() {
        let upstream = config(2, 2);
        let route = config(1000, 1000);
        let effective = effective_rate_limit(Some(&upstream), Some(&route)).expect("merged");
        assert_eq!(
            effective.capacity(),
            2,
            "the upstream's tighter budget is what applies"
        );
    }

    fn window(rate: u64, window: RateWindow) -> RateLimitConfig {
        RateLimitConfig {
            sharing: crate::domain::dto::Sharing::Private,
            algorithm: RateAlgorithm::SlidingWindow,
            sustained: SustainedRate { rate, window },
            burst: None,
            scope: crate::domain::dto::RateScope::Tenant,
            strategy: RateStrategy::Reject,
            cost: 1,
        }
    }

    #[test]
    fn a_sliding_window_admits_its_rate_then_refuses() {
        let limiter = TokenBucketLimiter::new();
        let config = window(3, RateWindow::Second);
        let key = RateKey::Tenant(Uuid::nil());
        for _ in 0..3 {
            assert!(
                limiter.consume(&key, &config, 1, now()).allowed,
                "three hits fit a three-per-second window"
            );
        }
        let refused = limiter.consume(&key, &config, 1, now());
        assert!(!refused.allowed);
        assert_eq!(refused.limit, 3, "the budget is the sustained rate");
        assert_eq!(refused.remaining, 0);
        assert_eq!(refused.retry_after, 1, "the oldest hit leaves in one second");
    }

    #[test]
    fn a_sliding_window_frees_up_as_its_hits_age_out() {
        let limiter = TokenBucketLimiter::new();
        let config = window(2, RateWindow::Second);
        let key = RateKey::Tenant(Uuid::nil());
        assert!(limiter.consume(&key, &config, 1, now()).allowed);
        assert!(limiter.consume(&key, &config, 1, now()).allowed);
        assert!(!limiter.consume(&key, &config, 1, now()).allowed);
        // Half a second in, the first hit is still inside the window.
        assert!(
            !limiter.consume(&key, &config, 1, now() + Duration::from_millis(500)).allowed,
            "the window slides, it does not reset at a boundary"
        );
        let decision = limiter.consume(&key, &config, 1, now() + Duration::from_millis(1100));
        assert!(decision.allowed, "the first hit has aged out");
    }

    #[test]
    fn a_sliding_window_charges_the_full_cost() {
        let limiter = TokenBucketLimiter::new();
        let config = RateLimitConfig { cost: 2, ..window(4, RateWindow::Second) };
        let key = RateKey::Tenant(Uuid::nil());
        assert!(limiter.consume(&key, &config, 2, now()).allowed);
        assert!(limiter.consume(&key, &config, 2, now()).allowed);
        assert!(
            !limiter.consume(&key, &config, 1, now()).allowed,
            "two requests of cost two exactly fill a four-unit window"
        );
    }

    #[test]
    fn a_window_wider_than_a_second_is_honoured() {
        let limiter = TokenBucketLimiter::new();
        let config = window(1, RateWindow::Minute);
        let key = RateKey::Tenant(Uuid::nil());
        assert!(limiter.consume(&key, &config, 1, now()).allowed);
        let refused = limiter.consume(&key, &config, 1, now() + Duration::from_secs(5));
        assert!(!refused.allowed, "five seconds is nothing against a minute");
        assert_eq!(refused.retry_after, 55);
        assert!(
            limiter
                .consume(&key, &config, 1, now() + Duration::from_secs(61))
                .allowed,
            "past the window the slot is free"
        );
    }
}
