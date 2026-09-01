//! In-memory token-bucket rate limiting (ADR 0003, ADR 0006).
//!
//! The data plane owns per-instance token buckets keyed per the ADR 0003 key
//! shape `oagw:ratelimit:{resource_type}:{resource_id}:{scope}:{scope_id}:{window}`
//! so a common prefix enables prefix-based cleanup when an upstream is
//! deleted. Dual-rate configuration (sustained rate + burst capacity) and the
//! effective-limit merge (`min(ancestor, descendant)` — stricter always wins,
//! DESIGN "Hierarchical Configuration") are honored.
//!
//! # DESIGN-led deviations
//!
//! - The MVP is local-only (`strategy: queue|degrade` are honored as `reject`;
//!   the models document this). Distributed coordination (Hybrid Local +
//!   Periodic Sync into Redis/Valkey) is out of scope.
//! - Buckets are continuous (refilled lazily on access) rather than
//!   fixed-window counters, so the ADR's fixed-window `YYYYMMDDHHMM` bucket id
//!   is dropped and `{window}` is the window granularity (`second`/`minute`/
//!   `hour`/`day`). Stale buckets are swept amortized (once per `SWEEP_EVERY`
//!   acquires) so the table stays bounded without a background task.
//! - Hierarchical budget allocation (`budget.mode: allocated|shared`) and
//!   ancestor-enforced sharing beyond the upstream→route `min` merge are out
//!   of scope for the MVP.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use http::HeaderMap;
use http::header::HeaderName;
use toolkit_security::SecurityContext;
use tracing::debug;
use uuid::Uuid;

/// `X-Forwarded-For` (not shipped by `http` ≥ 1.0; defined locally for the
/// IP counter-scope's source-IP heuristic).
const X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");

use crate::domain::models::{RateLimitConfig, RateLimitScope, RateWindow, Route, Upstream};

/// Sweep the bucket table once every N acquires (amortized cleanup; bounded
/// memory without a background task).
const SWEEP_EVERY: u64 = 256;
/// Buckets idle longer than this are dropped by the amortized sweep.
const MAX_IDLE: Duration = Duration::from_mins(5);

/// Effective rate-limit plan for one proxied request (merged upstream+route).
#[derive(Debug, Clone)]
pub struct RateLimitPlan {
    /// Full bucket-map key (`oagw:ratelimit:...`).
    key: String,
    /// Tokens replenished per second.
    tps: f64,
    /// Burst capacity (max bucket size).
    capacity: f64,
    /// Tokens consumed per request (`cost`).
    cost: u64,
    /// Sustained rate (tokens per window) for `X-RateLimit-Limit`.
    limit: u64,
}

/// Outcome of a rate check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimitDecision {
    /// Whether the request consumed tokens.
    pub allowed: bool,
    /// Effective limit (tokens per window) — `X-RateLimit-Limit`.
    pub limit: u64,
    /// Tokens left in the bucket (floored) — `X-RateLimit-Remaining`.
    pub remaining: u64,
    /// Seconds until the bucket refills to capacity — `Retry-After` and
    /// `X-RateLimit-Reset` (epoch = now + this).
    pub reset_after_secs: u64,
}

/// DP-owned per-instance rate-limiter state (ADR 0006). Not `Clone`; share via
/// `Arc` so every proxy handler sees the same buckets.
#[derive(Debug, Default)]
pub struct RateLimiter {
    buckets: DashMap<String, TokenBucket>,
    acquire_count: AtomicU64,
}

/// A lazily-refilled token bucket (ADR 0003 "Implementation Notes").
#[derive(Debug)]
struct TokenBucket {
    tokens: f64,
    capacity: f64,
    tps: f64,
    last_update: Instant,
    last_seen: Instant,
}

impl TokenBucket {
    fn new(capacity: f64, tps: f64) -> Self {
        let now = Instant::now();
        Self {
            tokens: capacity,
            capacity,
            tps,
            last_update: now,
            last_seen: now,
        }
    }

    /// Refill tokens proportionally to elapsed time, capped at capacity.
    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_update).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.tps).min(self.capacity);
        self.last_update = now;
    }

    /// Try to take `cost` tokens.
    fn try_acquire(&mut self, cost: f64) -> bool {
        self.refill();
        if self.tokens >= cost {
            self.tokens -= cost;
            true
        } else {
            false
        }
    }
}

impl RateLimiter {
    /// Create an empty rate limiter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Derive the effective rate-limit plan for one request from the upstream
    /// (ancestor) and matched route (descendant) configs, resolving the
    /// counter scope to a concrete scope id.
    ///
    /// Returns `None` when neither resource configures a rate limit (fail-open).
    #[must_use]
    pub fn plan_for(
        upstream: &Upstream,
        route: &Route,
        security_ctx: Option<&SecurityContext>,
        inbound_headers: &HeaderMap,
    ) -> Option<RateLimitPlan> {
        let effective = effective_config(upstream, route)?;
        let scope_id = resolve_scope_id(effective.scope, route, security_ctx, inbound_headers);
        let window_label = window_label(effective.window);
        let key = format!(
            "oagw:ratelimit:upstream:{}:{}:{}:{}",
            upstream.id,
            effective.scope_label(),
            scope_id,
            window_label
        );
        Some(RateLimitPlan {
            key,
            tps: effective.tps,
            capacity: effective.capacity,
            cost: effective.cost,
            limit: effective.limit,
        })
    }

    /// Attempt to consume `cost` tokens from the plan's bucket.
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "token-bucket accounting is f64 internally (ADR 0003); small u64 token counts are exactly representable in f64 and the 'as u64' truncations floor them intentionally"
    )]
    pub fn try_acquire(&self, plan: &RateLimitPlan) -> RateLimitDecision {
        // Amortized stale-bucket sweep.
        let count = self.acquire_count.fetch_add(1, Ordering::Relaxed);
        if count.is_multiple_of(SWEEP_EVERY) {
            self.sweep_stale(MAX_IDLE);
        }

        let mut bucket = self
            .buckets
            .entry(plan.key.clone())
            .or_insert_with(|| TokenBucket::new(plan.capacity, plan.tps));
        // Detect config drift: when the plan (tps/capacity) changed since the
        // bucket was created, the `or_insert_with` snapshot is stale — reset to
        // the current plan so a rate-limit reconfiguration takes effect
        // immediately (and a deleted+recreated aliased upstream never inherits
        // a predecessor's counters).
        if !f64_approx_eq(bucket.tps, plan.tps) || !f64_approx_eq(bucket.capacity, plan.capacity) {
            *bucket = TokenBucket::new(plan.capacity, plan.tps);
        }
        bucket.last_seen = Instant::now();

        if bucket.try_acquire(plan.cost as f64) {
            RateLimitDecision {
                allowed: true,
                limit: plan.limit,
                remaining: bucket.tokens.floor() as u64,
                reset_after_secs: 0,
            }
        } else {
            // Seconds until the bucket refills to capacity (how long the
            // caller should back off before retrying the full budget).
            // Defensive math: a non-positive/non-finite plan `tps` would make
            // the division diverge — treat such a bucket as never refilling.
            if !plan.tps.is_finite() || plan.tps <= 0.0 {
                return RateLimitDecision {
                    allowed: false,
                    limit: plan.limit,
                    remaining: bucket.tokens.floor() as u64,
                    reset_after_secs: u64::MAX,
                };
            }
            let refill_gap = (plan.capacity - bucket.tokens).max(0.0);
            let reset = ((refill_gap / plan.tps).ceil().max(1.0).min(u64::MAX as f64)) as u64;
            RateLimitDecision {
                allowed: false,
                limit: plan.limit,
                remaining: bucket.tokens.floor() as u64,
                reset_after_secs: reset,
            }
        }
    }

    /// Drop buckets idle longer than `max_idle`.
    pub fn sweep_stale(&self, max_idle: Duration) {
        self.buckets
            .retain(|_, b| b.last_seen.elapsed() <= max_idle);
    }

    /// Drop every bucket (used by tests; also a reset surface on config wipe).
    pub fn clear(&self) {
        self.buckets.clear();
    }

    /// Number of live buckets (observability/tests).
    #[must_use]
    pub fn len(&self) -> usize {
        self.buckets.len()
    }

    /// Whether the limiter has no buckets (infallible).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }

    /// Drop all buckets for one upstream (prefix-based cleanup, ADR 0003) so a
    /// deleted/reconfigured upstream's counters never leak into its successor.
    pub fn clear_for_upstream(&self, upstream_id: Uuid) {
        let prefix = format!("oagw:ratelimit:upstream:{upstream_id}:");
        self.buckets.retain(|key, _| !key.starts_with(&prefix));
    }
}

/// Approximate float equality for token-bucket params (small non-negative
/// rates/capacities, so an absolute epsilon is appropriate).
fn f64_approx_eq(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

/// The merged effective rate configuration.
struct EffectiveConfig {
    /// Tokens per second (min over configured sustained rates).
    tps: f64,
    /// Capacity (min over configured burst capacities).
    capacity: f64,
    /// Integer rate of the primary config for `X-RateLimit-Limit`.
    limit: u64,
    /// Window granularity of the primary config.
    window: RateWindow,
    /// Cost per request (route overrides upstream).
    cost: u64,
    /// Counter scope (route overrides upstream).
    scope: RateLimitScope,
}

impl EffectiveConfig {
    fn scope_label(&self) -> &'static str {
        match self.scope {
            RateLimitScope::Global => "global",
            RateLimitScope::Tenant => "tenant",
            RateLimitScope::User => "user",
            RateLimitScope::Ip => "ip",
            RateLimitScope::Route => "route",
        }
    }
}

/// Merge `upstream.rate_limit` (ancestor) and `route.rate_limit`
/// (descendant): stricter always wins for the limit; the descendant overrides
/// `cost` and `scope`. `None` when neither configures a limit.
fn effective_config(upstream: &Upstream, route: &Route) -> Option<EffectiveConfig> {
    let u = upstream.rate_limit.as_ref();
    let r = route.rate_limit.as_ref();
    if u.is_none() && r.is_none() {
        return None;
    }

    #[allow(
        clippy::cast_precision_loss,
        reason = "sustained rates are small u64 counters; f64 division is the intended tokens-per-second semantics"
    )]
    let tps = |c: &RateLimitConfig| c.sustained.rate as f64 / c.sustained.window.seconds() as f64;
    #[allow(
        clippy::cast_precision_loss,
        reason = "burst capacities are small u64 counters exactly representable in f64"
    )]
    let cap = |c: &RateLimitConfig| {
        c.burst
            .as_ref()
            .map_or(c.sustained.rate as f64, |b| b.capacity as f64)
    };

    let mut tps_min = f64::INFINITY;
    let mut cap_min = f64::INFINITY;
    for c in [u, r].into_iter().flatten() {
        tps_min = tps_min.min(tps(c));
        cap_min = cap_min.min(cap(c));
    }

    // Primary (for window/label): the stricter config; the descendant wins a
    // tie so its window is the basis of `X-RateLimit-Limit`/`Reset`.
    let primary = match (u, r) {
        (Some(u), Some(r)) if tps(r) <= tps(u) => r,
        (Some(u), _) => u,
        (None, Some(r)) => r,
        (None, None) => unreachable!("checked above"),
    };

    // `cost`/`scope`: the descendant (route) wins when it configures a rate
    // limit; otherwise the ancestor's (upstream) values apply — never dropped
    // to defaults because the route itself has no limit block.
    let merged_cost = r.as_ref().or(u.as_ref()).map_or(1, |c| c.cost);
    // Clamp the merged cost to the effective (ancestor-min'd) burst capacity
    // so a cross-resource pair (e.g. upstream `10/10/1` + route `100/20/cost
    // 15`) can never yield a plan that is permanently unsatisfiable — with
    // `cost > capacity` the very first request would 429 forever. When the
    // effective cost exceeds the effective capacity, the request consumes the
    // whole bucket, which is the existing per-bucket semantics for
    // cost == capacity (belt-and-braces for pairs that per-config validation
    // in validation.rs — which validates each config alone — cannot see).
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "small non-negative token counters: the effective capacity is an exact-integer f64 (derived from u64 burst/sustained), and the truncating cast only ever lowers the cost toward that capacity so the merged plan stays satisfiable"
    )]
    let cost = (merged_cost as f64).min(cap_min.max(1.0)) as u64;
    if cost < merged_cost {
        debug!(
            upstream_id = %upstream.id,
            route_id = %route.id,
            merged_cost,
            effective_capacity = cap_min,
            clamped_cost = cost,
            "merged rate-limit cost exceeds the effective burst capacity; clamped so the plan stays satisfiable"
        );
    }
    Some(EffectiveConfig {
        tps: tps_min,
        capacity: cap_min,
        limit: primary.sustained.rate,
        window: primary.sustained.window,
        cost,
        scope: r
            .as_ref()
            .or(u.as_ref())
            .map(|c| c.scope)
            .unwrap_or_default(),
    })
}

fn window_label(window: RateWindow) -> &'static str {
    match window {
        RateWindow::Second => "second",
        RateWindow::Minute => "minute",
        RateWindow::Hour => "hour",
        RateWindow::Day => "day",
    }
}

/// Resolve a configured counter scope to a concrete scope id.
///
/// - `tenant`/`user` come from the security context ("anonymous" when the
///   proxy runs without one);
/// - `ip` uses the first `X-Forwarded-For` hop ("0.0.0.0" when absent);
/// - `global`/`route` need no caller identity.
fn resolve_scope_id(
    scope: RateLimitScope,
    route: &Route,
    security_ctx: Option<&SecurityContext>,
    inbound_headers: &HeaderMap,
) -> String {
    match scope {
        RateLimitScope::Global => "global".to_owned(),
        RateLimitScope::Route => route.id.to_string(),
        RateLimitScope::Tenant => security_ctx.map_or_else(
            || "anonymous".to_owned(),
            |c| c.subject_tenant_id().to_string(),
        ),
        RateLimitScope::User => {
            security_ctx.map_or_else(|| "anonymous".to_owned(), |c| c.subject_id().to_string())
        }
        RateLimitScope::Ip => inbound_headers
            .get(X_FORWARDED_FOR)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.split(',').next())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("0.0.0.0")
            .to_owned(),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::models::{
        BurstConfig, Endpoint, HeadersConfig, MatchRule, PROTOCOL_HTTP_V1, PluginsConfig,
        RateLimitAlgorithm, RateLimitStrategy, Scheme, ServerConfig, SharingMode, SustainedRate,
    };
    use http::HeaderValue;

    fn upstream_with(rate: Option<RateLimitConfig>) -> Upstream {
        Upstream {
            id: Uuid::new_v4(),
            enabled: true,
            alias: "api".to_owned(),
            tags: Vec::new(),
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: Scheme::Https,
                    host: "api.example.com".to_owned(),
                    port: 443,
                }],
            },
            protocol: PROTOCOL_HTTP_V1.to_owned(),
            auth: None,
            headers: HeadersConfig::default(),
            plugins: PluginsConfig::default(),
            rate_limit: rate,
            cors: None,
        }
    }

    fn route_for(upstream_id: Uuid, rate: Option<RateLimitConfig>) -> Route {
        Route {
            id: Uuid::new_v4(),
            enabled: true,
            tags: Vec::new(),
            upstream_id,
            r#match: Some(MatchRule {
                http: None,
                grpc: None,
            }),
            plugins: PluginsConfig::default(),
            rate_limit: rate,
            cors: None,
        }
    }

    fn ctx(tenant: u128) -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::from_u128(1))
            .subject_tenant_id(Uuid::from_u128(tenant))
            .build()
            .unwrap()
    }

    fn second(rate: u64) -> RateLimitConfig {
        RateLimitConfig {
            sharing: SharingMode::Private,
            algorithm: RateLimitAlgorithm::default(),
            sustained: SustainedRate {
                rate,
                window: RateWindow::Second,
            },
            burst: None,
            scope: RateLimitScope::Tenant,
            strategy: RateLimitStrategy::default(),
            cost: 1,
        }
    }

    #[test]
    fn effective_config_is_none_without_limits() {
        let u = upstream_with(None);
        let r = route_for(u.id, None);
        assert!(effective_config(&u, &r).is_none());
    }

    #[test]
    fn effective_config_takes_the_stricter_limit() {
        // Upstream: 100/s; route: 10/s → effective 10/s.
        let u = upstream_with(Some(second(100)));
        let r = route_for(u.id, Some(second(10)));
        let eff = effective_config(&u, &r).expect("configured");
        assert!((eff.tps - 10.0).abs() < f64::EPSILON);
        assert_eq!(eff.limit, 10);
        // Reverse: route less strict than upstream → upstream wins.
        let u = upstream_with(Some(second(10)));
        let r = route_for(u.id, Some(second(100)));
        let eff = effective_config(&u, &r).expect("configured");
        assert!((eff.tps - 10.0).abs() < f64::EPSILON);
        // Route cost + scope override the ancestor's.
        let u = upstream_with(Some(second(10)));
        let mut r = route_for(u.id, Some(second(100)));
        r.rate_limit = Some(RateLimitConfig {
            scope: RateLimitScope::User,
            cost: 5,
            ..second(100)
        });
        let eff = effective_config(&u, &r).expect("configured");
        assert_eq!(eff.cost, 5);
        assert_eq!(eff.scope, RateLimitScope::User);
    }

    #[test]
    fn burst_capacity_min_wins_and_defaults_to_sustained() {
        let u = upstream_with(Some(RateLimitConfig {
            burst: Some(BurstConfig { capacity: 50 }),
            ..second(100)
        }));
        let r = route_for(u.id, Some(second(10))); // no burst → default 10
        let eff = effective_config(&u, &r).expect("configured");
        assert!((eff.capacity - 10.0).abs() < f64::EPSILON);
    }

    #[test]
    fn merged_cost_above_effective_capacity_is_clamped_and_first_request_allowed() {
        // Upstream: 10/s with burst 10, cost 1; route: 100/s with burst 20 but
        // cost 15. Effective plan: tps 10, capacity 10, cost 15 — without the
        // clamp the first request (cost 15 > 10 tokens) would 429 forever, a
        // permanently unsatisfiable merged plan (e.g. upstream `10/10/1` +
        // route `100/20/cost 15`). The clamp drops cost to the capacity (10),
        // and the first request consumes the whole bucket and is allowed.
        let u = upstream_with(Some(RateLimitConfig {
            burst: Some(BurstConfig { capacity: 10 }),
            ..second(10)
        }));
        let r = route_for(
            u.id,
            Some(RateLimitConfig {
                burst: Some(BurstConfig { capacity: 20 }),
                cost: 15,
                ..second(100)
            }),
        );
        let eff = effective_config(&u, &r).expect("configured");
        assert!((eff.capacity - 10.0).abs() < f64::EPSILON);
        assert_eq!(eff.cost, 10, "merged cost must be clamped to the capacity");

        let limiter = RateLimiter::new();
        let plan = RateLimiter::plan_for(&u, &r, Some(&ctx(1)), &HeaderMap::new()).unwrap();
        assert!(
            limiter.try_acquire(&plan).allowed,
            "first request must be allowed, not an immediate 429"
        );
        // The bucket is now drained; a second immediate request is rejected
        // (the clamped plan is satisfiable recharge-wise, not a lie).
        assert!(!limiter.try_acquire(&plan).allowed);
    }

    #[test]
    fn scope_id_resolution() {
        let hdrs = HeaderMap::new();
        let ctx = ctx(42);
        // Tenant scope uses the security context.
        assert_eq!(
            resolve_scope_id(
                RateLimitScope::Tenant,
                &route_for(Uuid::new_v4(), None),
                Some(&ctx),
                &hdrs
            ),
            ctx.subject_tenant_id().to_string()
        );
        // Without a context → anonymous.
        assert_eq!(
            resolve_scope_id(
                RateLimitScope::Tenant,
                &route_for(Uuid::new_v4(), None),
                None,
                &hdrs
            ),
            "anonymous"
        );
        // IP scope reads the first X-Forwarded-For hop.
        let mut fwd = HeaderMap::new();
        fwd.insert(
            X_FORWARDED_FOR,
            HeaderValue::from_static("10.0.0.1, 10.0.0.2"),
        );
        assert_eq!(
            resolve_scope_id(
                RateLimitScope::Ip,
                &route_for(Uuid::new_v4(), None),
                None,
                &fwd
            ),
            "10.0.0.1"
        );
        // Global is fixed.
        assert_eq!(
            resolve_scope_id(
                RateLimitScope::Global,
                &route_for(Uuid::new_v4(), None),
                None,
                &hdrs
            ),
            "global"
        );
    }

    #[test]
    fn token_bucket_case_uses_flow_new_rates_and_rejects_burst() {
        // 3 tokens/s capacity 3 → 3 fast requests pass, the 4th is rejected.
        let u = upstream_with(Some(second(3)));
        let r = route_for(u.id, None);
        let limiter = RateLimiter::new();
        let plan = RateLimiter::plan_for(&u, &r, Some(&ctx(1)), &HeaderMap::new()).unwrap();
        assert!((plan.capacity - 3.0).abs() < f64::EPSILON);
        for _ in 0..3 {
            let d = limiter.try_acquire(&plan);
            assert!(d.allowed, "burst allowance");
        }
        let rejected = limiter.try_acquire(&plan);
        assert!(!rejected.allowed);
        assert_eq!(rejected.remaining, 0);
        assert_eq!(rejected.limit, 3);
        assert!(rejected.reset_after_secs >= 1);
    }

    #[tokio::test]
    async fn token_bucket_refills_with_time() {
        let u = upstream_with(Some(second(10))); // 10 tokens/s, capacity 10
        let r = route_for(u.id, None);
        let limiter = RateLimiter::new();
        let plan = RateLimiter::plan_for(&u, &r, Some(&ctx(1)), &HeaderMap::new()).unwrap();
        assert!(limiter.try_acquire(&plan).allowed); // 1 token used
        assert_eq!(limiter.try_acquire(&plan).remaining, 8); // 10-1-1
        tokio::time::sleep(Duration::from_millis(1200)).await;
        // ~12 tokens elapsed → refilled back to capacity 10.
        let d = limiter.try_acquire(&plan);
        assert!(d.allowed);
        assert_eq!(d.remaining, 9);
    }

    #[test]
    fn tenant_scoping_keys_buckets_apart() {
        let u = upstream_with(Some(second(1)));
        let r = route_for(u.id, None);
        let limiter = RateLimiter::new();
        let plan_a = RateLimiter::plan_for(&u, &r, Some(&ctx(1)), &HeaderMap::new()).unwrap();
        let plan_b = RateLimiter::plan_for(&u, &r, Some(&ctx(2)), &HeaderMap::new()).unwrap();
        assert_ne!(plan_a.key, plan_b.key);
        assert!(limiter.try_acquire(&plan_a).allowed); // drains tenant 1
        assert!(!limiter.try_acquire(&plan_a).allowed);
        assert!(limiter.try_acquire(&plan_b).allowed); // tenant 2 unaffected
    }

    #[test]
    fn reconfigured_plan_resets_the_bucket_immediately() {
        // Drain a 3/s bucket fully, then reconfigure the SAME upstream to
        // 5/s: the next acquire must observe the new plan (bucket reset), not
        // the frozen 3-token bucket created by `or_insert_with`.
        let u = upstream_with(Some(second(3)));
        let r = route_for(u.id, None);
        let limiter = RateLimiter::new();
        let plan = RateLimiter::plan_for(&u, &r, Some(&ctx(1)), &HeaderMap::new()).unwrap();
        for _ in 0..3 {
            assert!(limiter.try_acquire(&plan).allowed);
        }
        assert!(!limiter.try_acquire(&plan).allowed);

        let mut reconfigured = u;
        reconfigured.rate_limit = Some(second(5));
        let plan2 = RateLimiter::plan_for(&reconfigured, &r, Some(&ctx(1)), &HeaderMap::new())
            .unwrap();
        assert_eq!(plan.key, plan2.key, "same key, drift must reset the bucket");
        for _ in 0..5 {
            assert!(
                limiter.try_acquire(&plan2).allowed,
                "drift-reset burst against the new capacity"
            );
        }
        assert!(!limiter.try_acquire(&plan2).allowed);
    }

    #[test]
    fn clear_for_upstream_removes_only_that_prefix() {
        let u1 = upstream_with(Some(second(1)));
        let u2 = upstream_with(Some(second(1)));
        let r1 = route_for(u1.id, None);
        let r2 = route_for(u2.id, None);
        let limiter = RateLimiter::new();
        let _decision = limiter.try_acquire(
            &RateLimiter::plan_for(&u1, &r1, Some(&ctx(1)), &HeaderMap::new()).unwrap(),
        );
        let _decision = limiter.try_acquire(
            &RateLimiter::plan_for(&u2, &r2, Some(&ctx(1)), &HeaderMap::new()).unwrap(),
        );
        assert_eq!(limiter.len(), 2);
        limiter.clear_for_upstream(u1.id);
        assert_eq!(limiter.len(), 1);
    }
}
