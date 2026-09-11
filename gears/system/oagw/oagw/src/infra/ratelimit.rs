//! Per-instance token-bucket rate limiting, owned by the Data Plane
//! (ADR-0003 §4, ADR-0006).
//!
//! MVP scope: local buckets, no distributed coordination. Keys are prefixed
//! `{resource_type}:{resource_id}` so every counter belonging to one upstream
//! or route shares a prefix and can be dropped in one sweep when the resource
//! is deleted.

use std::time::Instant;

use dashmap::DashMap;

use crate::domain::model::{RateLimitConfig, RateScope, RateStrategy};

/// A single token bucket.
#[derive(Debug)]
struct TokenBucket {
    tokens: f64,
    last_update: Instant,
    capacity: f64,
    refill_rate: f64,
}

impl TokenBucket {
    fn new(capacity: f64, refill_rate: f64) -> Self {
        Self {
            tokens: capacity,
            last_update: Instant::now(),
            capacity,
            refill_rate,
        }
    }

    /// Re-shape an existing bucket when the effective configuration changed,
    /// clamping the balance so a shrunk capacity takes effect immediately.
    fn reshape(&mut self, capacity: f64, refill_rate: f64) {
        if (self.capacity - capacity).abs() > f64::EPSILON
            || (self.refill_rate - refill_rate).abs() > f64::EPSILON
        {
            self.capacity = capacity;
            self.refill_rate = refill_rate;
            self.tokens = self.tokens.min(capacity);
        }
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now
            .saturating_duration_since(self.last_update)
            .as_secs_f64();
        if elapsed > 0.0 {
            self.tokens = (self.tokens + elapsed * self.refill_rate).min(self.capacity);
            self.last_update = now;
        }
    }

    fn try_acquire(&mut self, cost: f64, now: Instant) -> bool {
        self.refill(now);
        if self.tokens >= cost {
            self.tokens -= cost;
            true
        } else {
            false
        }
    }

    /// Seconds until `cost` tokens are available, `0` when they already are.
    fn wait_for(&self, cost: f64) -> f64 {
        if self.tokens >= cost || self.refill_rate <= 0.0 {
            return 0.0;
        }
        (cost - self.tokens) / self.refill_rate
    }
}

/// Outcome of a rate limit check.
#[derive(Debug, Clone, PartialEq)]
pub enum RateLimitOutcome {
    /// The request may proceed.
    Allowed {
        /// Value for `X-RateLimit-Limit`.
        limit: u64,
        /// Value for `X-RateLimit-Remaining`.
        remaining: u64,
        /// Seconds until the bucket is full again.
        reset_after: u64,
    },
    /// The request may proceed, but the limit was exceeded and the configured
    /// strategy is `degrade`.
    Degraded {
        /// Value for `X-RateLimit-Limit`.
        limit: u64,
        /// Seconds until capacity returns.
        retry_after: u64,
    },
    /// The request must be refused with `429`.
    Rejected {
        /// Value for `X-RateLimit-Limit`.
        limit: u64,
        /// Seconds the client should wait.
        retry_after: u64,
    },
}

/// Identity of the resource a bucket belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateResource {
    /// An upstream-level limit.
    Upstream(uuid::Uuid),
    /// A route-level limit.
    Route(uuid::Uuid),
}

impl RateResource {
    fn prefix(self) -> String {
        match self {
            RateResource::Upstream(id) => format!("upstream:{id}"),
            RateResource::Route(id) => format!("route:{id}"),
        }
    }
}

/// The scope-dependent part of a counter key.
#[derive(Debug, Clone)]
pub struct ScopeKey {
    /// Calling tenant.
    pub tenant_id: uuid::Uuid,
    /// Calling subject.
    pub subject_id: uuid::Uuid,
    /// Client address, when known.
    pub client_ip: Option<String>,
    /// Matched route, when one matched.
    pub route_id: Option<uuid::Uuid>,
}

impl ScopeKey {
    fn render(&self, scope: RateScope) -> String {
        match scope {
            RateScope::Global => "global:-".to_owned(),
            RateScope::Tenant => format!("tenant:{}", self.tenant_id),
            RateScope::User => format!("user:{}", self.subject_id),
            RateScope::Ip => format!("ip:{}", self.client_ip.as_deref().unwrap_or("unknown")),
            RateScope::Route => format!(
                "route:{}",
                self.route_id
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "-".to_owned())
            ),
        }
    }
}

/// Registry of per-instance token buckets.
#[derive(Default)]
pub struct RateLimiterRegistry {
    buckets: DashMap<String, TokenBucket>,
}

impl RateLimiterRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Check (and consume from) the bucket for `resource` under `config`.
    #[must_use]
    pub fn check(
        &self,
        resource: RateResource,
        config: &RateLimitConfig,
        scope_key: &ScopeKey,
    ) -> RateLimitOutcome {
        let key = format!(
            "{}:{}:{}",
            resource.prefix(),
            scope_key.render(config.scope),
            window_tag(config)
        );
        let capacity = capacity_f64(config);
        let refill = config.refill_per_second();
        let cost = f64::from(config.cost);
        let now = Instant::now();

        let mut entry = self
            .buckets
            .entry(key)
            .or_insert_with(|| TokenBucket::new(capacity, refill));
        entry.reshape(capacity, refill);
        let allowed = entry.try_acquire(cost, now);
        let limit = config.sustained.rate;
        if allowed {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let remaining = entry.tokens.floor().max(0.0) as u64;
            let reset_after = ceil_secs(entry.wait_for(entry.capacity));
            return RateLimitOutcome::Allowed {
                limit,
                remaining,
                reset_after,
            };
        }
        let retry_after = ceil_secs(entry.wait_for(cost)).max(1);
        match config.strategy {
            RateStrategy::Reject | RateStrategy::Queue => {
                RateLimitOutcome::Rejected { limit, retry_after }
            }
            RateStrategy::Degrade => RateLimitOutcome::Degraded { limit, retry_after },
        }
    }

    /// Seconds a `queue`-strategy caller must wait for `cost` tokens, without
    /// consuming any.
    #[must_use]
    pub fn queue_delay(
        &self,
        resource: RateResource,
        config: &RateLimitConfig,
        scope_key: &ScopeKey,
    ) -> f64 {
        let key = format!(
            "{}:{}:{}",
            resource.prefix(),
            scope_key.render(config.scope),
            window_tag(config)
        );
        self.buckets
            .get(&key)
            .map_or(0.0, |bucket| bucket.wait_for(f64::from(config.cost)))
    }

    /// Drop every bucket belonging to `resource` — called when a resource is
    /// deleted or replaced, so a re-created id starts from a clean balance.
    pub fn forget(&self, resource: RateResource) {
        let prefix = format!("{}:", resource.prefix());
        self.buckets.retain(|key, _| !key.starts_with(&prefix));
    }

    /// Number of live buckets, for tests and diagnostics.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buckets.len()
    }

    /// `true` when no bucket is live.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }
}

#[allow(clippy::cast_precision_loss)]
fn capacity_f64(config: &RateLimitConfig) -> f64 {
    config.capacity() as f64
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn ceil_secs(seconds: f64) -> u64 {
    if seconds <= 0.0 {
        0
    } else {
        seconds.ceil() as u64
    }
}

/// Window discriminator, so switching a limit between `second` and `minute`
/// does not silently reuse the old bucket's balance.
fn window_tag(config: &RateLimitConfig) -> &'static str {
    match config.sustained.window {
        crate::domain::model::RateWindow::Second => "second",
        crate::domain::model::RateWindow::Minute => "minute",
        crate::domain::model::RateWindow::Hour => "hour",
        crate::domain::model::RateWindow::Day => "day",
    }
}

#[cfg(test)]
mod tests {
    use super::{RateLimitOutcome, RateLimiterRegistry, RateResource, ScopeKey};
    use crate::domain::model::{
        BurstRate, RateAlgorithm, RateLimitConfig, RateScope, RateStrategy, RateWindow,
        SharingMode, SustainedRate,
    };
    use uuid::Uuid;

    fn config(rate: u64, window: RateWindow) -> RateLimitConfig {
        RateLimitConfig {
            sharing: SharingMode::Private,
            algorithm: RateAlgorithm::TokenBucket,
            sustained: SustainedRate { rate, window },
            burst: None,
            budget: None,
            scope: RateScope::Tenant,
            strategy: RateStrategy::Reject,
            cost: 1,
            response_headers: true,
        }
    }

    fn scope_key(tenant: Uuid) -> ScopeKey {
        ScopeKey {
            tenant_id: tenant,
            subject_id: Uuid::new_v4(),
            client_ip: Some("127.0.0.1".to_owned()),
            route_id: None,
        }
    }

    #[test]
    fn a_burst_is_allowed_up_to_capacity_then_rejected() {
        let registry = RateLimiterRegistry::new();
        let upstream = RateResource::Upstream(Uuid::new_v4());
        let mut cfg = config(2, RateWindow::Minute);
        cfg.burst = Some(BurstRate { capacity: Some(2) });
        let key = scope_key(Uuid::new_v4());

        for _ in 0..2 {
            assert!(matches!(
                registry.check(upstream, &cfg, &key),
                RateLimitOutcome::Allowed { .. }
            ));
        }
        match registry.check(upstream, &cfg, &key) {
            RateLimitOutcome::Rejected { limit, retry_after } => {
                assert_eq!(limit, 2);
                assert!(retry_after >= 1, "Retry-After must be actionable");
            }
            other => panic!("expected a rejection, got {other:?}"),
        }
    }

    #[test]
    fn remaining_counts_down() {
        let registry = RateLimiterRegistry::new();
        let upstream = RateResource::Upstream(Uuid::new_v4());
        let cfg = config(5, RateWindow::Minute);
        let key = scope_key(Uuid::new_v4());
        let mut seen = Vec::new();
        for _ in 0..3 {
            if let RateLimitOutcome::Allowed { remaining, .. } =
                registry.check(upstream, &cfg, &key)
            {
                seen.push(remaining);
            }
        }
        assert_eq!(seen, vec![4, 3, 2]);
    }

    #[test]
    fn scopes_get_independent_counters() {
        let registry = RateLimiterRegistry::new();
        let upstream = RateResource::Upstream(Uuid::new_v4());
        let cfg = config(1, RateWindow::Minute);
        let tenant_a = scope_key(Uuid::new_v4());
        let tenant_b = scope_key(Uuid::new_v4());

        assert!(matches!(
            registry.check(upstream, &cfg, &tenant_a),
            RateLimitOutcome::Allowed { .. }
        ));
        assert!(matches!(
            registry.check(upstream, &cfg, &tenant_a),
            RateLimitOutcome::Rejected { .. }
        ));
        assert!(
            matches!(
                registry.check(upstream, &cfg, &tenant_b),
                RateLimitOutcome::Allowed { .. }
            ),
            "another tenant's traffic must not consume this tenant's budget"
        );
    }

    #[test]
    fn global_scope_shares_one_counter_across_tenants() {
        let registry = RateLimiterRegistry::new();
        let upstream = RateResource::Upstream(Uuid::new_v4());
        let mut cfg = config(1, RateWindow::Minute);
        cfg.scope = RateScope::Global;
        assert!(matches!(
            registry.check(upstream, &cfg, &scope_key(Uuid::new_v4())),
            RateLimitOutcome::Allowed { .. }
        ));
        assert!(matches!(
            registry.check(upstream, &cfg, &scope_key(Uuid::new_v4())),
            RateLimitOutcome::Rejected { .. }
        ));
    }

    #[test]
    fn cost_consumes_multiple_tokens() {
        let registry = RateLimiterRegistry::new();
        let upstream = RateResource::Upstream(Uuid::new_v4());
        let mut cfg = config(10, RateWindow::Minute);
        cfg.cost = 10;
        let key = scope_key(Uuid::new_v4());
        assert!(matches!(
            registry.check(upstream, &cfg, &key),
            RateLimitOutcome::Allowed { remaining: 0, .. }
        ));
        assert!(matches!(
            registry.check(upstream, &cfg, &key),
            RateLimitOutcome::Rejected { .. }
        ));
    }

    #[test]
    fn degrade_strategy_lets_the_request_through() {
        let registry = RateLimiterRegistry::new();
        let upstream = RateResource::Upstream(Uuid::new_v4());
        let mut cfg = config(1, RateWindow::Minute);
        cfg.strategy = RateStrategy::Degrade;
        let key = scope_key(Uuid::new_v4());
        let _ = registry.check(upstream, &cfg, &key);
        assert!(matches!(
            registry.check(upstream, &cfg, &key),
            RateLimitOutcome::Degraded { .. }
        ));
    }

    #[test]
    fn upstream_and_route_buckets_are_separate() {
        let registry = RateLimiterRegistry::new();
        let id = Uuid::new_v4();
        let cfg = config(1, RateWindow::Minute);
        let key = scope_key(Uuid::new_v4());
        assert!(matches!(
            registry.check(RateResource::Upstream(id), &cfg, &key),
            RateLimitOutcome::Allowed { .. }
        ));
        assert!(matches!(
            registry.check(RateResource::Route(id), &cfg, &key),
            RateLimitOutcome::Allowed { .. }
        ));
    }

    #[test]
    fn forget_drops_only_that_resources_buckets() {
        let registry = RateLimiterRegistry::new();
        let a = RateResource::Upstream(Uuid::new_v4());
        let b = RateResource::Upstream(Uuid::new_v4());
        let cfg = config(5, RateWindow::Minute);
        let key = scope_key(Uuid::new_v4());
        let _ = registry.check(a, &cfg, &key);
        let _ = registry.check(b, &cfg, &key);
        assert_eq!(registry.len(), 2);
        registry.forget(a);
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn refill_restores_capacity_over_time() {
        let registry = RateLimiterRegistry::new();
        let upstream = RateResource::Upstream(Uuid::new_v4());
        // A single-token bucket refilled 100 times a second: one request
        // drains it, and 20ms is more than enough to earn the next token.
        let mut cfg = config(100, RateWindow::Second);
        cfg.burst = Some(BurstRate { capacity: Some(1) });
        let key = scope_key(Uuid::new_v4());

        assert!(matches!(
            registry.check(upstream, &cfg, &key),
            RateLimitOutcome::Allowed { .. }
        ));
        assert!(matches!(
            registry.check(upstream, &cfg, &key),
            RateLimitOutcome::Rejected { .. }
        ));
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(matches!(
            registry.check(upstream, &cfg, &key),
            RateLimitOutcome::Allowed { .. }
        ));
    }

    #[test]
    fn a_shrunk_capacity_takes_effect_immediately() {
        let registry = RateLimiterRegistry::new();
        let upstream = RateResource::Upstream(Uuid::new_v4());
        let key = scope_key(Uuid::new_v4());
        let generous = config(100, RateWindow::Minute);
        let _ = registry.check(upstream, &generous, &key);

        let mut tight = config(100, RateWindow::Minute);
        tight.burst = Some(BurstRate { capacity: Some(1) });
        // Reshaping clamps the carried-over balance to the new capacity, so a
        // single request drains it.
        assert!(matches!(
            registry.check(upstream, &tight, &key),
            RateLimitOutcome::Allowed { .. }
        ));
        assert!(matches!(
            registry.check(upstream, &tight, &key),
            RateLimitOutcome::Rejected { .. }
        ));
    }
}
