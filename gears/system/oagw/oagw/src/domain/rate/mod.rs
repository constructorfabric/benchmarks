//! Rate limiting domain (feature `cpt-cf-oagw-feature-rate-limiting`, ADR
//! 0003).
//!
//! Two dual-rate strategies backed by the same ADR parameters — token bucket
//! (burst then sustained) and sliding window (no boundary burst) — plus an
//! in-process [`Decider`] that the Data Plane consults on every proxied
//! request (algorithm `cpt-cf-oagw-algo-rate-limiting-cache-lookup`).
//!
//! Composition model (algorithm `cpt-cf-oagw-algo-rate-limiting-effective-min`):
//! upstream and route each carry their own [`RateLimitConfig`]; the effective
//! rate is the *minimum* of the publisher and the route (ADR 0003 §inheritance),
//! merged by the caller before a decision is taken.  Buckets are keyed by the
//! `(tenant_id, upstream_id, route_id)` tuple so publishers never share
//! counters (DoD `cpt-cf-oagw-dod-rate-limiting-hierarchy`).
//!
//! Strategies (DoD `cpt-cf-oagw-dod-rate-limiting-strategies`): `reject`
//! returns the 429 error (algorithm `cpt-cf-oagw-algo-rate-limiting-emit-429`,
//! step `inst-rl-429-headers` — `X-RateLimit-Limit/Remaining/Reset`,
//! `Retry-After`); `queue` hands the decision to the bounded admission queue
//! (Data Plane p5); `degrade` skips the upstream call and serves the gateway
//! fallback.

pub mod limiter;
pub mod token_bucket;

use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use uuid::Uuid;

use crate::domain::entity::config::{RateLimitConfig, RateLimitStrategy};
use crate::domain::error::DomainError;

pub use limiter::{Clock, RateLimiter};

/// `X-RateLimit-Limit` header (burst capacity).
pub const HEADER_RATE_LIMIT_LIMIT: &str = "x-rate-limit-limit";
/// `X-RateLimit-Remaining` header.
pub const HEADER_RATE_LIMIT_REMAINING: &str = "x-rate-limit-remaining";
/// `X-RateLimit-Reset` header (epoch seconds when the counter resets).
pub const HEADER_RATE_LIMIT_RESET: &str = "x-rate-limit-reset";
/// `Retry-After` header (seconds until budget is available again).
pub const HEADER_RETRY_AFTER: &str = "retry-after";

/// The inputs the Data Plane hands to the rate [`Decider`].
///
/// `config` must already be the *effective* config — the ADR minimum of the
/// upstream and route configurations (algorithm
/// `cpt-cf-oagw-algo-rate-limiting-effective-min`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitRequest {
    /// The tenant owning the upstream (scoping key component).
    pub tenant_id: Uuid,
    /// The upstream whose rate limit applies.
    pub upstream_id: Uuid,
    /// The matched route, for per-route counters; `None` for upstream-only.
    pub route_id: Option<Uuid>,
    /// The effective rate-limit configuration (already min-merged).
    pub config: RateLimitConfig,
}

/// The outcome of a rate decision, carrying the `X-RateLimit-*` projections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RateLimitDecision {
    /// Within budget — the request proceeds to the upstream.
    Allow(RateLimitInfo),
    /// Budget exhausted, strategy `queue` — the request may be admitted to
    /// the bounded queue (Data Plane p5); `retry_after` is the earliest wait.
    Queue(RateLimitInfo),
    /// Budget exhausted, strategy `degrade` — the upstream call is skipped
    /// and the gateway fallback is served.
    Degrade(RateLimitInfo),
    /// Budget exhausted, strategy `reject` — the request is rejected with the
    /// 429 [`DomainError`] (`inst-rl-429-headers`).
    Rejected(RateLimitInfo),
}

impl RateLimitDecision {
    /// The [`RateLimitInfo`] carried by any decision variant.
    #[must_use]
    pub const fn info(&self) -> &RateLimitInfo {
        match self {
            Self::Allow(info) | Self::Queue(info) | Self::Degrade(info) | Self::Rejected(info) => {
                info
            }
        }
    }

    /// Whether the decision allows proxying to the upstream (only `Allow`).
    #[must_use]
    pub const fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow(_))
    }
}

/// The `X-RateLimit-*` / `Retry-After` projections of a decision (step
/// `inst-rl-429-headers`; DoD `cpt-cf-oagw-dod-rate-limiting-headers`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimitInfo {
    /// Burst capacity — `X-RateLimit-Limit`.
    pub limit: u64,
    /// Remaining budget after the decision — `X-RateLimit-Remaining`.
    pub remaining: u64,
    /// Time until the counter resets to full — `X-RateLimit-Reset`
    /// (see [`RateLimitInfo::reset_epoch`]).
    pub reset_in: Duration,
    /// Earliest wait for budget — `Retry-After` (`0` when allowed).
    pub retry_after: Duration,
    /// The strategy in effect (for the Data Plane dispatch).
    pub strategy: RateLimitStrategy,
}

impl RateLimitInfo {
    /// The `X-RateLimit-Reset` value in epoch seconds from a wall-clock `now`.
    #[must_use]
    pub fn reset_epoch(&self, now: SystemTime) -> u64 {
        now.checked_add(self.reset_in)
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// Maps an exhausted bucket onto the 429 gateway error (algorithm
    /// `cpt-cf-oagw-algo-rate-limiting-emit-429`), carrying the retry timing.
    #[must_use]
    pub fn exceeded_error(&self, detail: impl Into<String>) -> DomainError {
        DomainError::rate_limit_exceeded(detail.into(), Some(self.retry_after))
    }
}

/// The rate-limiting interface for the Data Plane (algorithm
/// `cpt-cf-oagw-algo-rate-limiting-cache-lookup`).
///
/// Implementations must be cheap, thread-safe, and in-process — no external
/// round-trip on the hot path (DoD `cpt-cf-oagw-dod-rate-limiting-cache`).
#[async_trait]
pub trait Decider: Send + Sync {
    /// Evaluates the effective [`RateLimitConfig`] against the current
    /// counters and returns a [`RateLimitDecision`].  Side-effect-free on the
    /// caller's state (no mutate-on-reject debt).
    async fn decide(&self, request: &RateLimitRequest) -> RateLimitDecision;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::entity::config::{RateLimitScope, SharingMode};

    #[test]
    fn header_constants_match_the_adr_contract() {
        assert_eq!(HEADER_RATE_LIMIT_LIMIT, "x-rate-limit-limit");
        assert_eq!(HEADER_RATE_LIMIT_REMAINING, "x-rate-limit-remaining");
        assert_eq!(HEADER_RATE_LIMIT_RESET, "x-rate-limit-reset");
        assert_eq!(HEADER_RETRY_AFTER, "retry-after");
    }

    #[test]
    fn rejected_info_maps_to_the_429_rate_limit_exceeded_instance() {
        let info = RateLimitInfo {
            limit: 100,
            remaining: 0,
            reset_in: Duration::from_secs(30),
            retry_after: Duration::from_secs(30),
            strategy: RateLimitStrategy::Reject,
        };
        let err = info.exceeded_error("rate limit exceeded");
        assert_eq!(err.status(), 429);
        assert_eq!(
            err.instance(),
            "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1"
        );
        assert_eq!(err.retry_after(), Some(Duration::from_secs(30)));
    }

    #[test]
    fn reset_epoch_projects_reset_in_onto_epoch_seconds() {
        let info = RateLimitInfo {
            limit: 100,
            remaining: 0,
            reset_in: Duration::from_secs(30),
            retry_after: Duration::ZERO,
            strategy: RateLimitStrategy::Reject,
        };
        let epoch = SystemTime::UNIX_EPOCH;
        assert_eq!(info.reset_epoch(epoch), 30);
    }

    #[test]
    fn decision_info_and_allow_projection() {
        let info = RateLimitInfo {
            limit: 5,
            remaining: 4,
            reset_in: Duration::ZERO,
            retry_after: Duration::ZERO,
            strategy: RateLimitStrategy::Queue,
        };
        let decision = RateLimitDecision::Queue(info);
        assert!(decision.info().remaining == 4);
        assert!(!decision.is_allowed());
        // Allow is the only proxying decision.
        let allow = RateLimitDecision::Allow(RateLimitInfo {
            limit: 5,
            remaining: 4,
            reset_in: Duration::ZERO,
            retry_after: Duration::ZERO,
            strategy: RateLimitStrategy::Queue,
        });
        assert!(allow.is_allowed());
        // The request carries the merged effective config untouched.
        let req = RateLimitRequest {
            tenant_id: Uuid::from_u128(1),
            upstream_id: Uuid::from_u128(2),
            route_id: Some(Uuid::from_u128(3)),
            config: RateLimitConfig {
                sharing: SharingMode::Private,
                sustained: crate::domain::entity::config::SustainedRate {
                    rate: 10,
                    window: crate::domain::entity::config::RateLimitWindow::Second,
                },
                scope: RateLimitScope::Tenant,
                burst: None,
                strategy: crate::domain::entity::config::RateLimitStrategy::Reject,
                algorithm: crate::domain::entity::config::RateLimitAlgorithm::TokenBucket,
                cost: 1,
            },
        };
        assert_eq!(req.route_id, Some(Uuid::from_u128(3)));
    }
}
