//! Rate-limit extension point. Implemented by DECOMPOSITION entry 2.8
//! (rate-limiting).
//!
//! [`evaluate_budget`] is the fixed, narrow call site `crate::proxy::engine`
//! (owned by entry 2.5) drives: it takes only an already-min-composed
//! `Option<u32>` sustained rate and the calling tenant's id, with no burst,
//! scope, strategy, or resource identity. The full FEATURE
//! (`docs/features/rate-limiting.md`) needs all of that, so this module
//! implements the complete token-bucket engine -- hierarchical
//! composition, scope-based counter keys, the `429`/`X-RateLimit-*`
//! contract, and `reject`/`queue`/`degrade` strategies -- as its own public
//! functions in the `bucket`/`key`/`limit`/`headers`/`engine` submodules,
//! each unit-tested directly, and makes [`evaluate_budget`] a thin adapter
//! over that engine that this round's fixed call site can actually drive.
//! See this file's `evaluate_budget` doc comment for exactly which part of
//! the documented contract that adapter can and cannot deliver, and this
//! feature's manifest for the richer entry point a follow-up round should
//! switch `crate::proxy::engine` to.

mod bucket;
mod clock;
mod engine;
/// Exposed at the `policy::ratelimit` boundary (RF-003/RF-004): `crate::proxy::engine`
/// needs [`headers::RateLimitHeaders`] to decorate an *allowed* response, not
/// just a rejected one -- see [`RateLimitOutcome::Continue`].
pub(crate) mod headers;
/// Exposed at the `policy::ratelimit` boundary (RF-003): `crate::proxy::merge`
/// and `crate::proxy::engine` need the real [`key::ScopeContext`]/
/// [`key::ResourceRef`] shapes to drive a genuine per-resource/per-scope
/// counter key instead of the hardcoded-tenant adapter this file used to be.
pub(crate) mod key;
/// Exposed at the `policy::ratelimit` boundary (RF-003): `crate::proxy::merge`
/// needs [`limit::EffectiveRateLimit`] (and its `effective_from_config`
/// conversion) to carry the full merged `rate_limit` object -- scope,
/// strategy, burst, algorithm, cost -- through to this module's real engine,
/// instead of collapsing it to a bare `Option<u32>`.
pub(crate) mod limit;

use std::sync::OnceLock;

use axum::response::Response;

use clock::SystemClock;
use engine::{RateLimitEngine, StrategyOutcome};
use headers::RateLimitHeaders;
use key::ScopeContext;
use limit::EffectiveRateLimit;

/// Outcome of a rate-limit budget check.
pub(crate) enum RateLimitOutcome {
    /// The request may proceed. `Some(headers)` when a bucket was actually
    /// evaluated (`rate_limit` was configured), so the caller can decorate
    /// the eventual response with the documented `X-RateLimit-*` headers
    /// (RF-003's "other half of the contract": previously this variant
    /// carried nothing to decorate with). `None` when no rate limit is
    /// configured at all -- nothing to attach.
    Continue(Option<RateLimitHeaders>),
    ShortCircuit(Response),
}

/// The single per-Data-Plane-instance engine this adapter drives
/// (`cpt-cf-oagw-dod-ratelimit-position`'s per-instance ownership),
/// lazily created on first use since the fixed call site has no place to
/// inject one explicitly.
// @cpt-dod:cpt-cf-oagw-dod-ratelimit-position:p1
fn global_engine() -> &'static RateLimitEngine {
    static ENGINE: OnceLock<RateLimitEngine> = OnceLock::new();
    ENGINE.get_or_init(RateLimitEngine::new)
}

/// Post-resolution policy hook, second in the fixed order CORS -> rate-limit
/// -> plugin chain (`inst-proxy-fwd-policy-hook`): evaluates the merged
/// effective rate-limit budget against the resolved scope's token-bucket
/// state. Implemented by DECOMPOSITION entry 2.8 (rate-limiting).
///
/// RF-003: this now drives the real engine with the genuine, fully-merged
/// `EffectiveRateLimit` (`scope`/`strategy`/`burst`/`algorithm`/`cost`, not
/// a bare `Option<u32>`) and the caller-resolved [`ScopeContext`] (resource
/// id, route id, principal id, client IP) `crate::proxy::merge`/
/// `crate::proxy::engine` compute from the real request -- so a configured
/// `scope: ip`/`user`/`route`/`global` and `strategy: queue`/`degrade` are
/// genuinely enforced, never silently coerced to `tenant`/`reject`.
///
/// - `rate_limit: None` ("no rate limit configured") always continues with
///   no bucket evaluated, matching `inst-ratelimit-within-limit-03`/`-04`.
/// - On DENY this returns the full documented `429` contract
///   (`cpt-cf-oagw-dod-ratelimit-rejection-contract`): the GTS
///   `rate_limit.exceeded` type, `X-OAGW-Error-Source: gateway`,
///   `Retry-After`, and `X-RateLimit-*`.
/// - On ALLOW this returns `Continue(Some(headers))`, so the caller can
///   decorate the eventual success response with `X-RateLimit-*`
///   (`inst-ratelimit-within-limit-09`'s "attach headers to the allowed
///   response" half of the contract).
// @cpt-begin:cpt-cf-oagw-flow-ratelimit-within-limit:p1:inst-ratelimit-within-limit-01
// @cpt-begin:cpt-cf-oagw-flow-ratelimit-exceeded:p1:inst-ratelimit-exceeded-01
pub(crate) fn evaluate_budget(
    rate_limit: Option<&EffectiveRateLimit>,
    scope_ctx: &ScopeContext,
) -> RateLimitOutcome {
    // @cpt-begin:cpt-cf-oagw-flow-ratelimit-within-limit:p1:inst-ratelimit-within-limit-03
    let Some(config) = rate_limit else {
        // @cpt-begin:cpt-cf-oagw-flow-ratelimit-within-limit:p1:inst-ratelimit-within-limit-04
        return RateLimitOutcome::Continue(None);
        // @cpt-end:cpt-cf-oagw-flow-ratelimit-within-limit:p1:inst-ratelimit-within-limit-04
    };
    // @cpt-end:cpt-cf-oagw-flow-ratelimit-within-limit:p1:inst-ratelimit-within-limit-03

    // @cpt-begin:cpt-cf-oagw-flow-ratelimit-within-limit:p1:inst-ratelimit-within-limit-05
    // @cpt-begin:cpt-cf-oagw-flow-ratelimit-within-limit:p1:inst-ratelimit-within-limit-02
    let counter_key = key::select_counter_key(config.scope, scope_ctx);
    // @cpt-end:cpt-cf-oagw-flow-ratelimit-within-limit:p1:inst-ratelimit-within-limit-02

    // @cpt-begin:cpt-cf-oagw-flow-ratelimit-within-limit:p1:inst-ratelimit-within-limit-10
    // @cpt-begin:cpt-cf-oagw-flow-ratelimit-exceeded:p1:inst-ratelimit-exceeded-13
    match engine::evaluate_reject_or_degrade(global_engine(), counter_key, config, &SystemClock) {
        StrategyOutcome::Forward(headers) => RateLimitOutcome::Continue(Some(headers)),
        StrategyOutcome::Reject(headers) => {
            // @cpt-begin:cpt-cf-oagw-flow-ratelimit-exceeded:p1:inst-ratelimit-exceeded-03
            RateLimitOutcome::ShortCircuit(engine::reject_response(&headers))
            // @cpt-end:cpt-cf-oagw-flow-ratelimit-exceeded:p1:inst-ratelimit-exceeded-03
        }
    }
    // @cpt-end:cpt-cf-oagw-flow-ratelimit-exceeded:p1:inst-ratelimit-exceeded-13
    // @cpt-end:cpt-cf-oagw-flow-ratelimit-within-limit:p1:inst-ratelimit-within-limit-10
    // @cpt-end:cpt-cf-oagw-flow-ratelimit-within-limit:p1:inst-ratelimit-within-limit-05
}
// @cpt-end:cpt-cf-oagw-flow-ratelimit-exceeded:p1:inst-ratelimit-exceeded-01
// @cpt-end:cpt-cf-oagw-flow-ratelimit-within-limit:p1:inst-ratelimit-within-limit-01

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::model::upstream::RateLimitAlgorithm;
    use key::ResourceRef;
    use uuid::Uuid;

    fn config(rate: u32, scope: crate::model::upstream::RateLimitScope) -> EffectiveRateLimit {
        EffectiveRateLimit {
            algorithm: RateLimitAlgorithm::TokenBucket,
            rate_per_second: f64::from(rate),
            display_rate: rate,
            burst_capacity: rate.max(1),
            scope,
            strategy: crate::model::upstream::RateLimitStrategy::Reject,
            cost: 1,
        }
    }

    fn ctx_for(resource: ResourceRef, tenant_id: Uuid, client_ip: Option<&str>) -> ScopeContext {
        ScopeContext {
            contributing_resource: resource,
            route_id: Uuid::new_v4(),
            tenant_id,
            principal_id: Uuid::new_v4(),
            client_ip: client_ip.map(str::to_owned),
        }
    }

    #[test]
    fn no_configured_rate_always_continues() {
        let ctx = ctx_for(ResourceRef::Upstream(Uuid::new_v4()), Uuid::new_v4(), None);
        assert!(matches!(
            evaluate_budget(None, &ctx),
            RateLimitOutcome::Continue(None)
        ));
    }

    #[test]
    fn requests_within_budget_continue_and_carry_headers_to_decorate() {
        let tenant_id = Uuid::new_v4();
        let ctx = ctx_for(ResourceRef::Upstream(Uuid::new_v4()), tenant_id, None);
        let config = config(5, crate::model::upstream::RateLimitScope::Tenant);
        match evaluate_budget(Some(&config), &ctx) {
            RateLimitOutcome::Continue(Some(_headers)) => {}
            RateLimitOutcome::Continue(None) => panic!("expected headers to decorate the response"),
            RateLimitOutcome::ShortCircuit(_) => {
                panic!("expected the request within budget to continue")
            }
        }
    }

    #[test]
    fn exhausting_the_budget_short_circuits_with_429_and_headers() {
        let tenant_id = Uuid::new_v4();
        let resource = ResourceRef::Upstream(Uuid::new_v4());
        let config = config(2, crate::model::upstream::RateLimitScope::Tenant);
        // burst defaults to the rate itself, so `rate` back-to-back calls
        // exhaust a freshly-seeded bucket for this key.
        for _ in 0..2 {
            let ctx = ctx_for(resource, tenant_id, None);
            assert!(matches!(
                evaluate_budget(Some(&config), &ctx),
                RateLimitOutcome::Continue(_)
            ));
        }
        let ctx = ctx_for(resource, tenant_id, None);
        match evaluate_budget(Some(&config), &ctx) {
            RateLimitOutcome::ShortCircuit(response) => {
                assert_eq!(response.status().as_u16(), 429);
                assert!(response.headers().get("retry-after").is_some());
                assert!(response.headers().get("x-ratelimit-limit").is_some());
                assert!(response.headers().get("x-ratelimit-remaining").is_some());
                assert!(response.headers().get("x-ratelimit-reset").is_some());
            }
            RateLimitOutcome::Continue(_) => panic!("expected the exhausted bucket to reject"),
        }
    }

    #[test]
    fn different_tenants_get_independent_buckets() {
        let resource = ResourceRef::Upstream(Uuid::new_v4());
        let exhausted_tenant = Uuid::new_v4();
        let fresh_tenant = Uuid::new_v4();
        let config = config(1, crate::model::upstream::RateLimitScope::Tenant);
        assert!(matches!(
            evaluate_budget(Some(&config), &ctx_for(resource, exhausted_tenant, None)),
            RateLimitOutcome::Continue(_)
        ));
        assert!(matches!(
            evaluate_budget(Some(&config), &ctx_for(resource, exhausted_tenant, None)),
            RateLimitOutcome::ShortCircuit(_)
        ));
        // A different tenant's bucket is untouched by the above.
        assert!(matches!(
            evaluate_budget(Some(&config), &ctx_for(resource, fresh_tenant, None)),
            RateLimitOutcome::Continue(_)
        ));
    }

    /// RF-003 required test: two Upstreams (distinct `ResourceRef`s) under
    /// one tenant with `scope: ip` must not share a bucket -- exhausting
    /// one Upstream's budget for a given IP must not reject the other
    /// Upstream's budget for the same IP.
    #[test]
    fn ip_scoped_budgets_are_independent_per_upstream() {
        let tenant_id = Uuid::new_v4();
        let client_ip = "203.0.113.9";
        let upstream_a = ResourceRef::Upstream(Uuid::new_v4());
        let upstream_b = ResourceRef::Upstream(Uuid::new_v4());
        let config = config(1, crate::model::upstream::RateLimitScope::Ip);

        // Exhaust upstream A's bucket for this IP.
        let ctx_a = ctx_for(upstream_a, tenant_id, Some(client_ip));
        assert!(matches!(
            evaluate_budget(Some(&config), &ctx_a),
            RateLimitOutcome::Continue(_)
        ));
        let ctx_a_again = ctx_for(upstream_a, tenant_id, Some(client_ip));
        assert!(matches!(
            evaluate_budget(Some(&config), &ctx_a_again),
            RateLimitOutcome::ShortCircuit(_)
        ));

        // Upstream B, same tenant, same client IP: must still have its own
        // fresh budget -- no cross-resource bleed.
        let ctx_b = ctx_for(upstream_b, tenant_id, Some(client_ip));
        assert!(matches!(
            evaluate_budget(Some(&config), &ctx_b),
            RateLimitOutcome::Continue(_)
        ));
    }
}
