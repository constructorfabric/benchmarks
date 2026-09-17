// Created: 2026-09-04 by Constructor Tech
//! Token-bucket rate limiting of the data plane
//! (`docs/ADR/0003-rate-limiting.md`).
//!
//! The limiter is per-instance and in-memory (the MVP of the ADR): one bucket
//! per `resource:scope` key, refilled from the *sustained* rate and capped by
//! the *burst* capacity. Buckets live in a [`DashMap`] and are never held
//! across an `.await`.

use dashmap::DashMap;
use tokio::time::Instant;
use uuid::Uuid;

use crate::domain::{
    RateLimitAlgorithm, RateLimitConfig, RateLimitScope, RateLimitStrategy, SustainedRate,
};
use crate::error::OagwError;

/// `X-OAGW-RateLimit-Limit` — bucket capacity of the effective limit.
pub const RATE_LIMIT_LIMIT_HEADER: &str = "x-oagw-ratelimit-limit";
/// `X-OAGW-RateLimit-Remaining` — whole tokens left in the bucket.
pub const RATE_LIMIT_REMAINING_HEADER: &str = "x-oagw-ratelimit-remaining";
/// `X-OAGW-RateLimit-Reset` — seconds until the bucket is full again.
pub const RATE_LIMIT_RESET_HEADER: &str = "x-oagw-ratelimit-reset";

/// `X-RateLimit-Limit` — the RFC 6585 / draft-ietf-httpapi-ratelimit-headers
/// twin of [`RATE_LIMIT_LIMIT_HEADER`]
/// (`docs/ADR/0003-rate-limiting.md` "Response headers").
pub const LEGACY_LIMIT_HEADER: &str = "x-ratelimit-limit";
/// `X-RateLimit-Remaining` — see [`LEGACY_LIMIT_HEADER`].
pub const LEGACY_REMAINING_HEADER: &str = "x-ratelimit-remaining";
/// `X-RateLimit-Reset` — see [`LEGACY_LIMIT_HEADER`].
pub const LEGACY_RESET_HEADER: &str = "x-ratelimit-reset";

/// Scaling factor turning one token into integer units (nano-tokens), so the
/// refill arithmetic stays exact without floating point.
const SCALE: u128 = 1_000_000_000;

/// Identifies the client a rate-limit counter is charged to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeIdentity {
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Authenticated subject of the request.
    pub subject_id: Uuid,
    /// Client IP of the request, when known.
    pub client_ip: Option<String>,
}

/// Outcome of one bucket withdrawal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimitDecision {
    /// `false` when the request is rejected with `429 RateLimitExceeded`.
    pub allowed: bool,
    /// Bucket capacity in requests (the `Limit` header).
    pub limit: u64,
    /// Whole tokens left in the bucket (the `Remaining` header).
    pub remaining: u64,
    /// Seconds until the bucket is full again (the `Reset` header).
    pub reset_secs: u64,
    /// Seconds until one token is available again (`Retry-After`); `0` when
    /// the request was allowed.
    pub retry_after_secs: u64,
}

impl RateLimitDecision {
    /// Writes the rate-limit headers of a response from this decision.
    pub fn write_headers(&self, headers: &mut http::HeaderMap) {
        for (name, value) in [
            (RATE_LIMIT_LIMIT_HEADER, self.limit.to_string()),
            (RATE_LIMIT_REMAINING_HEADER, self.remaining.to_string()),
            (RATE_LIMIT_RESET_HEADER, self.reset_secs.to_string()),
            (LEGACY_LIMIT_HEADER, self.limit.to_string()),
            (LEGACY_REMAINING_HEADER, self.remaining.to_string()),
            (LEGACY_RESET_HEADER, self.reset_secs.to_string()),
        ] {
            set_header(headers, name, &value);
        }
    }

    /// The `429 RateLimitExceeded` error of a rejected request, carrying the
    /// `Retry-After` guidance.
    #[must_use]
    pub fn error(&self) -> OagwError {
        OagwError::RateLimitExceeded {
            retry_after_secs: self.retry_after_secs,
            detail: format!(
                "rate limit of {limit} requests exceeded",
                limit = self.limit
            ),
        }
    }
}

/// Inserts a header, ignoring a name the HTTP grammar refuses.
fn set_header(headers: &mut http::HeaderMap, name: &str, value: &str) {
    if let (Ok(name), Ok(value)) = (
        http::HeaderName::from_bytes(name.as_bytes()),
        http::HeaderValue::from_str(value),
    ) {
        headers.insert(name, value);
    }
}

/// One token bucket of one scope.
#[derive(Debug)]
struct Bucket {
    /// Capacity in nano-tokens.
    capacity: u128,
    /// Remaining tokens in nano-tokens.
    tokens: u128,
    /// Refill rate in nano-tokens per second.
    refill_per_sec: u128,
    /// Instant of the last refill.
    last: Instant,
}

impl Bucket {
    fn new(config: &RateLimitConfig, now: Instant) -> Self {
        Self {
            capacity: capacity_of(config),
            tokens: capacity_of(config),
            refill_per_sec: refill_of(config),
            last: now,
        }
    }

    /// `true` while the bucket still models `config`; a configuration change
    /// resets the counters.
    fn matches(&self, config: &RateLimitConfig) -> bool {
        self.capacity == capacity_of(config) && self.refill_per_sec == refill_of(config)
    }

    /// Credits the tokens accrued since `last`.
    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last);
        self.last = now;
        if elapsed.is_zero() || self.refill_per_sec == 0 {
            return;
        }
        let micros = elapsed.as_micros();
        let credited = self.refill_per_sec * micros / 1_000_000;
        self.tokens = (self.tokens + credited).min(self.capacity);
    }

    /// Withdraws `cost` tokens and reports the accounting of the exchange.
    fn take(&mut self, cost: u32, now: Instant) -> RateLimitDecision {
        self.refill(now);
        let needed = u128::from(cost) * SCALE;
        let decision = RateLimitDecision {
            allowed: self.tokens >= needed,
            limit: u64::try_from(self.capacity / SCALE).unwrap_or(u64::MAX),
            remaining: u64::try_from(self.tokens / SCALE).unwrap_or(u64::MAX),
            reset_secs: self.seconds_until(self.capacity),
            retry_after_secs: 0,
        };
        if decision.allowed {
            self.tokens -= needed;
            RateLimitDecision {
                remaining: u64::try_from(self.tokens / SCALE).unwrap_or(u64::MAX),
                ..decision
            }
        } else {
            RateLimitDecision {
                retry_after_secs: self.seconds_until(SCALE).max(1),
                ..decision
            }
        }
    }

    /// Whole seconds until `target` nano-tokens are available.
    fn seconds_until(&self, target: u128) -> u64 {
        let Some(deficit) = target.checked_sub(self.tokens) else {
            return 0;
        };
        if self.refill_per_sec == 0 {
            return u64::MAX;
        }
        u64::try_from(deficit.div_ceil(self.refill_per_sec)).unwrap_or(u64::MAX)
    }
}

/// Bucket capacity in nano-tokens: the burst capacity, defaulting to the
/// sustained rate (`docs/schemas/upstream.v1.schema.json` `rate_limit`).
fn capacity_of(config: &RateLimitConfig) -> u128 {
    let tokens = match config.algorithm {
        RateLimitAlgorithm::TokenBucket => config
            .burst
            .map_or_else(|| config.sustained.rate.get(), |burst| burst.capacity.get()),
        // A sliding window has no burst allowance: the token count of the
        // window is the whole capacity.
        RateLimitAlgorithm::SlidingWindow => config.sustained.rate.get(),
    };
    u128::from(tokens) * SCALE
}

/// Refill rate in nano-tokens per second: the sustained rate spread evenly
/// over its window.
fn refill_of(config: &RateLimitConfig) -> u128 {
    let rate = u128::from(config.sustained.rate.get());
    let window = u128::from(config.sustained.window.duration_secs()).max(1);
    (rate * SCALE) / window
}

/// In-memory token-bucket store of the data plane.
#[derive(Debug, Default)]
pub struct RateLimiter {
    buckets: DashMap<String, Bucket>,
}

impl RateLimiter {
    /// An empty limiter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Withdraws `config.cost` tokens from the bucket named by `key` and
    /// reports the accounting of the exchange; a rejected request keeps its
    /// `Retry-After` guidance in [`RateLimitDecision::retry_after_secs`].
    pub fn check(&self, key: &str, config: &RateLimitConfig, now: Instant) -> RateLimitDecision {
        let mut bucket = self
            .buckets
            .entry(key.to_owned())
            .and_modify(|existing| {
                if !existing.matches(config) {
                    *existing = Bucket::new(config, now);
                }
            })
            .or_insert_with(|| Bucket::new(config, now));
        bucket.take(config.cost.get(), now)
    }

    /// Drops every bucket (used when the configuration of a resource changes).
    pub fn clear(&self) {
        self.buckets.clear();
    }
}

/// Counter key of one rate-limit scope
/// (`docs/ADR/0003-rate-limiting.md` "Redis key structure").
#[must_use]
pub fn bucket_key(
    config: &RateLimitConfig,
    upstream_id: Uuid,
    route_id: Option<Uuid>,
    identity: &ScopeIdentity,
) -> String {
    let resource = match (config.scope, route_id) {
        (RateLimitScope::Route, Some(route_id)) => format!("route:{route_id}"),
        _ => format!("upstream:{upstream_id}"),
    };
    let scope_id = match config.scope {
        RateLimitScope::Global => String::from("global"),
        RateLimitScope::Tenant | RateLimitScope::Route => identity.tenant_id.to_string(),
        RateLimitScope::User => identity.subject_id.to_string(),
        RateLimitScope::Ip => identity
            .client_ip
            .clone()
            .unwrap_or_else(|| identity.tenant_id.to_string()),
    };
    format!(
        "oagw:ratelimit:{resource}:{scope}:{scope_id}",
        scope = scope_label(config.scope)
    )
}

/// Scope label of a counter key.
fn scope_label(scope: RateLimitScope) -> &'static str {
    match scope {
        RateLimitScope::Global => "global",
        RateLimitScope::Tenant => "tenant",
        RateLimitScope::User => "user",
        RateLimitScope::Ip => "ip",
        RateLimitScope::Route => "route",
    }
}

/// Effective rate limit of a proxied request
/// (`docs/ADR/0003-rate-limiting.md` "Hierarchical Budget Allocation").
///
/// The upstream limit is the ancestor, the route limit the descendant:
///
/// * a `private` upstream limit is invisible, so the route value (if any)
///   applies;
/// * an `inherit` or `enforce` upstream limit is combined with the route
///   limit through `min()`, so a descendant can only tighten it — the
///   strictest sustained rate and the smaller burst capacity win.
///
/// This is the data-plane reading of the ADR (`effective_rate =
/// min(parent, child)`); [`crate::domain::RateLimitConfig::effective`] keeps
/// the control-plane reading, where an `inherit` descendant replaces the
/// ancestor value outright.
#[must_use]
pub fn effective_limit(
    upstream: Option<&RateLimitConfig>,
    route: Option<&RateLimitConfig>,
) -> Option<RateLimitConfig> {
    let Some(parent) = upstream else {
        return route.cloned();
    };
    if !parent.sharing.is_visible_to_descendants() {
        return route.cloned();
    }
    let Some(child) = route else {
        return Some(parent.clone());
    };
    Some(strictest(parent, child))
}

/// Strictest combination of two limits, keeping the descendant's scope and
/// strategy.
fn strictest(parent: &RateLimitConfig, child: &RateLimitConfig) -> RateLimitConfig {
    let sustained: SustainedRate = parent.sustained.stricter_of(child.sustained);
    let burst = match (parent.burst, child.burst) {
        (Some(parent), Some(child)) => Some(if parent.capacity.get() <= child.capacity.get() {
            parent
        } else {
            child
        }),
        (Some(only), None) | (None, Some(only)) => Some(only),
        (None, None) => None,
    };
    RateLimitConfig {
        sharing: child.sharing,
        algorithm: child.algorithm,
        sustained,
        burst,
        scope: child.scope,
        strategy: child.strategy,
        cost: child.cost,
    }
}

/// `true` when the limit rejects the request instead of degrading it
/// (`docs/schemas/upstream.v1.schema.json` `rate_limit.strategy`); the `queue`
/// and `degrade` strategies are not implemented yet and fall back to
/// `reject`.
#[must_use]
pub const fn rejects(config: &RateLimitConfig) -> bool {
    matches!(
        config.strategy,
        RateLimitStrategy::Reject | RateLimitStrategy::Queue
    )
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "ratelimit_tests.rs"]
mod ratelimit_tests;
