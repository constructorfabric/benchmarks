//! Token bucket rate limiting and effective-limit merging.

use std::time::{Duration, Instant};

use crate::domain::model::{Algorithm, RateLimit, Scope, Sharing, Strategy, Window};

/// A refill window expressed as a duration.
#[must_use]
pub fn window_duration(window: Window) -> Duration {
    Duration::from_secs(window.seconds())
}

/// Result of trying to consume tokens from a bucket.
#[derive(Debug, Clone, Copy)]
pub struct TakeOutcome {
    /// Whether the request was allowed.
    pub allowed: bool,
    /// Tokens left in the bucket after the call.
    pub remaining: u32,
    /// Instant at which the next token becomes available.
    pub reset_at: Instant,
}

/// A dual-rate token bucket.
///
/// Tokens replenish at `rate / window` and the bucket may burst up to its
/// capacity. Capacity defaults to the sustained rate.
#[derive(Debug, Clone)]
pub struct TokenBucket {
    rate: u32,
    window: Duration,
    capacity: f64,
    tokens: f64,
    last_refill: Instant,
}

impl TokenBucket {
    /// Creates a full bucket.
    #[must_use]
    pub fn new(rate: u32, window: Duration, capacity: u32, now: Instant) -> Self {
        let capacity = f64::from(capacity.max(1));
        Self {
            rate: rate.max(1),
            window,
            capacity,
            tokens: capacity,
            last_refill: now,
        }
    }

    /// Creates a bucket from a rate limit configuration.
    #[must_use]
    pub fn from_config(config: &RateLimit, now: Instant) -> Self {
        Self::new(
            config.sustained.rate,
            window_duration(config.sustained.window),
            config.capacity(),
            now,
        )
    }

    /// Refills tokens for the elapsed time.
    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last_refill);
        if elapsed.is_zero() {
            return;
        }
        self.last_refill = now;
        let replenished = tokens_per_second(self.rate, self.window) * elapsed.as_secs_f64();
        self.tokens = (self.tokens + replenished).min(self.capacity);
    }

    /// Tries to consume `cost` tokens.
    pub fn try_take(&mut self, cost: u32, now: Instant) -> TakeOutcome {
        self.refill(now);
        let cost = f64::from(cost.max(1));
        let next_token_at = now
            + Duration::from_secs_f64(
                self.window.as_secs_f64() / tokens_per_second(self.rate, self.window),
            );
        if self.tokens < cost {
            return TakeOutcome {
                allowed: false,
                remaining: 0,
                reset_at: next_token_at,
            };
        }
        self.tokens -= cost;
        TakeOutcome {
            allowed: true,
            remaining: floor_tokens(self.tokens),
            reset_at: next_token_at,
        }
    }

    /// Tokens currently in the bucket.
    #[must_use]
    pub fn tokens(&self) -> u32 {
        floor_tokens(self.tokens)
    }
}

/// Floors a token count into a `u32`.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn floor_tokens(value: f64) -> u32 {
    let floored = value.floor().max(0.0);
    if floored >= f64::from(u32::MAX) {
        u32::MAX
    } else {
        floored as u32
    }
}

/// Sustained rate expressed in tokens per second.
fn tokens_per_second(rate: u32, window: Duration) -> f64 {
    f64::from(rate.max(1)) / window.as_secs_f64().max(1.0)
}

/// The strictest of a set of limits.
///
/// The effective limit is `min(upstream limit, route limit, all
/// ancestor-enforced limits)`; a route limit tightens an upstream limit but
/// can never exceed it.
#[must_use]
pub fn effective_limit(limits: &[Option<RateLimit>]) -> Option<RateLimit> {
    let mut best: Option<RateLimit> = None;
    for limit in limits.iter().flatten() {
        best = Some(match best {
            None => *limit,
            Some(current) => stricter(current, *limit),
        });
    }
    best
}

/// Returns the stricter of two limits.
#[must_use]
pub fn stricter(a: RateLimit, b: RateLimit) -> RateLimit {
    let rate_of = |limit: &RateLimit| {
        tokens_per_second(limit.sustained.rate, window_duration(limit.sustained.window))
    };
    let (lo, _hi) = if rate_of(&a) <= rate_of(&b) { (a, b) } else { (b, a) };
    lo
}

/// Counter key for a scope.
#[must_use]
pub fn counter_key(scope: Scope, parts: ScopeParts<'_>) -> String {
    match scope {
        Scope::Global => "global".to_owned(),
        Scope::Tenant => format!("tenant:{}", parts.tenant_id),
        Scope::User => format!("user:{}:{}", parts.tenant_id, parts.subject_id),
        Scope::Ip => format!("ip:{}", parts.client_ip),
        Scope::Route => {
            format!("route:{}:{}:{}", parts.tenant_id, parts.upstream_id, parts.route_id)
        }
    }
}

/// Values used to build a scoped counter key.
#[derive(Debug, Clone, Copy)]
pub struct ScopeParts<'a> {
    /// Calling tenant.
    pub tenant_id: &'a str,
    /// Authenticated principal.
    pub subject_id: &'a str,
    /// Client address.
    pub client_ip: &'a str,
    /// Resolved upstream identifier.
    pub upstream_id: &'a str,
    /// Matched route identifier.
    pub route_id: &'a str,
}

/// Builds the merged (effective) rate limit for an upstream and route, taking
/// enforced ancestor constraints into account.
#[must_use]
pub fn merge_limits(
    upstream: Option<RateLimit>,
    route: Option<RateLimit>,
    enforced_ancestors: Vec<RateLimit>,
) -> Option<RateLimit> {
    let mut limits: Vec<Option<RateLimit>> = vec![upstream, route];
    limits.extend(enforced_ancestors.into_iter().map(Some));
    effective_limit(&limits)
}

/// True when the configured algorithm prevents boundary bursts.
#[must_use]
pub fn is_boundary_safe(algorithm: Algorithm) -> bool {
    matches!(algorithm, Algorithm::SlidingWindow)
}

/// True when the strategy queues rather than rejects.
#[must_use]
pub fn queues_on_exhaustion(strategy: Strategy) -> bool {
    matches!(strategy, Strategy::Queue)
}

/// True when a sharing mode propagates to descendants.
#[must_use]
pub fn is_enforced(sharing: Sharing) -> bool {
    matches!(sharing, Sharing::Enforce)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::domain::model::{Burst, Sustained};

    fn limit(rate: u32, window: Window, capacity: Option<u32>) -> RateLimit {
        RateLimit {
            sharing: Sharing::Private,
            algorithm: Algorithm::TokenBucket,
            sustained: Sustained { rate, window },
            burst: capacity.map(|capacity| Burst { capacity }),
            scope: Scope::Tenant,
            strategy: Strategy::Reject,
            cost: 1,
        }
    }

    #[test]
    fn burst_up_to_capacity_is_allowed() {
        let config = limit(3, Window::Second, Some(3));
        let now = Instant::now();
        let mut bucket = TokenBucket::from_config(&config, now);
        assert!(bucket.try_take(1, now).allowed);
        assert!(bucket.try_take(1, now).allowed);
        assert!(bucket.try_take(1, now).allowed);
        assert!(!bucket.try_take(1, now).allowed);
    }

    #[test]
    fn capacity_defaults_to_sustained_rate() {
        let config = limit(5, Window::Second, None);
        let now = Instant::now();
        let mut bucket = TokenBucket::from_config(&config, now);
        for _ in 0..5 {
            assert!(bucket.try_take(1, now).allowed);
        }
        assert!(!bucket.try_take(1, now).allowed);
    }

    #[test]
    fn cost_consumes_multiple_tokens() {
        let config = limit(3, Window::Second, Some(3));
        let now = Instant::now();
        let mut bucket = TokenBucket::from_config(&config, now);
        assert!(bucket.try_take(1, now).allowed);
        assert!(bucket.try_take(1, now).allowed);
        assert!(!bucket.try_take(3, now).allowed);
    }

    #[test]
    fn tokens_replenish_over_time() {
        let config = limit(3, Window::Second, Some(1));
        let now = Instant::now();
        let mut bucket = TokenBucket::from_config(&config, now);
        assert!(bucket.try_take(1, now).allowed);
        assert!(!bucket.try_take(1, now).allowed);
        let later = now + window_duration(Window::Second);
        assert!(bucket.try_take(1, later).allowed);
    }

    #[test]
    fn stricter_limit_wins() {
        let a = limit(5, Window::Second, None);
        let b = limit(1, Window::Second, None);
        assert_eq!(stricter(a, b).sustained.rate, 1);
        assert_eq!(stricter(b, a).sustained.rate, 1);
    }

    #[test]
    fn effective_limit_takes_the_minimum() {
        let limits = vec![
            Some(limit(5, Window::Second, None)),
            Some(limit(2, Window::Second, None)),
        ];
        assert_eq!(effective_limit(&limits).map(|l| l.sustained.rate), Some(2));
        assert!(effective_limit(&[None, None]).is_none());
    }

    #[test]
    fn merge_includes_enforced_ancestors() {
        let merged = merge_limits(
            Some(limit(10, Window::Second, None)),
            Some(limit(8, Window::Second, None)),
            vec![limit(3, Window::Second, None)],
        );
        assert_eq!(merged.map(|l| l.sustained.rate), Some(3));
    }

    #[test]
    fn merge_route_only() {
        let merged = merge_limits(Some(limit(2, Window::Second, None)), None, vec![]);
        assert_eq!(merged.map(|l| l.sustained.rate), Some(2));
    }

    #[test]
    fn window_durations() {
        assert_eq!(window_duration(Window::Minute), Duration::from_mins(1));
        assert_eq!(window_duration(Window::Day), Duration::from_hours(24));
    }

    #[test]
    fn counter_keys_are_scoped() {
        let parts = ScopeParts {
            tenant_id: "t",
            subject_id: "s",
            client_ip: "1.2.3.4",
            upstream_id: "u",
            route_id: "r",
        };
        assert_eq!(counter_key(Scope::Global, parts), "global");
        assert_eq!(counter_key(Scope::Tenant, parts), "tenant:t");
        assert_eq!(counter_key(Scope::User, parts), "user:t:s");
        assert_eq!(counter_key(Scope::Ip, parts), "ip:1.2.3.4");
        assert_eq!(counter_key(Scope::Route, parts), "route:t:u:r");
    }

    #[test]
    fn strategy_and_algorithm_helpers() {
        assert!(is_boundary_safe(Algorithm::SlidingWindow));
        assert!(!is_boundary_safe(Algorithm::TokenBucket));
        assert!(queues_on_exhaustion(Strategy::Queue));
        assert!(!queues_on_exhaustion(Strategy::Reject));
        assert!(is_enforced(Sharing::Enforce));
        assert!(!is_enforced(Sharing::Private));
    }
}
