//! Hierarchical effective rate-limit composition
//! (`cpt-cf-oagw-algo-ratelimit-effective-limit`,
//! `cpt-cf-oagw-dod-ratelimit-hierarchical-budget`).
//!
//! ADR-0003's `budget` object and `response_headers` toggle are not present
//! in the frozen `upstream.v1.schema.json`/`route.v1.schema.json`
//! `rate_limit` definitions (both declare `additionalProperties: false` over
//! exactly `sharing`/`algorithm`/`sustained`/`burst`/`scope`/`strategy`/
//! `cost`), so this module composes only from those seven fields: no
//! allocated/shared budget modes, no overcommit-ratio validation, and no
//! `response_headers` gate on header emission (`crate::policy::ratelimit::headers`
//! always emits `X-RateLimit-*` for an evaluated bucket, per
//! `cpt-cf-oagw-dod-ratelimit-hierarchical-budget`'s closing paragraph).
//!
//! `sustained.rate`/`sustained.window` values are normalized to a common
//! per-second rate for cross-window comparison, per
//! `inst-ratelimit-effective-limit-09`. This module's chosen "output window
//! unit" (`inst-ratelimit-effective-limit-15`) is `second`: the composed
//! [`EffectiveRateLimit`] always reports its per-second rate directly
//! (`display_rate`, floored), which can only be tighter than -- never
//! looser than -- any contributing level, satisfying that step's stated
//! intent without inventing an arbitrary preferred window among
//! second/minute/hour/day.
//!
//! This whole module is reachable only from its own tests today: the fixed
//! adapter `crate::policy::ratelimit::evaluate_budget` receives an
//! already-min-composed `Option<u32>`, not the Upstream/Route
//! `RateLimitConfig` pair this composition needs. See this feature's
//! manifest `richer_entry_point` note.
#![allow(dead_code)]

use crate::model::upstream::{
    RateLimitAlgorithm, RateLimitConfig, RateLimitScope, RateLimitStrategy, RateLimitWindow,
    Sharing,
};

/// One effective `rate_limit` to enforce for a request, or `None` sentinel
/// ("no rate limit configured") returned by [`compose_effective_limit`].
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EffectiveRateLimit {
    /// Accepted but not separately implemented, per
    /// `cpt-cf-oagw-dod-ratelimit-token-bucket`: enforcement always applies
    /// token-bucket accounting.
    #[allow(dead_code)]
    pub algorithm: RateLimitAlgorithm,
    /// The exact replenishment rate the bucket engine consumes, already
    /// normalized to tokens/second across every contributing level.
    pub rate_per_second: f64,
    /// `floor(rate_per_second)`, the whole-number rate surfaced on
    /// `X-RateLimit-Limit` (`inst-ratelimit-headers-01`).
    pub display_rate: u32,
    pub burst_capacity: u32,
    pub scope: RateLimitScope,
    pub strategy: RateLimitStrategy,
    pub cost: u32,
}

fn seconds_in(window: RateLimitWindow) -> f64 {
    match window {
        RateLimitWindow::Second => 1.0,
        RateLimitWindow::Minute => 60.0,
        RateLimitWindow::Hour => 3_600.0,
        RateLimitWindow::Day => 86_400.0,
    }
}

#[allow(
    clippy::cast_precision_loss,
    reason = "sustained.rate is a human-configured request-per-window count (schema minimum 1); it \
              never approaches 2^53, so the u32 -> f64 conversion below is exact"
)]
fn per_second(config: &RateLimitConfig) -> f64 {
    f64::from(config.sustained.rate) / seconds_in(config.sustained.window)
}

/// `burst.capacity` defaults to that level's own `sustained.rate` (the raw,
/// un-normalized number) when omitted, per the schema description.
fn burst_of(config: &RateLimitConfig) -> u32 {
    config
        .burst
        .as_ref()
        .and_then(|burst| burst.capacity)
        .unwrap_or(config.sustained.rate)
}

/// One level's contribution once it has "won" the categorical fields
/// (`algorithm`/`scope`/`strategy`/`cost`) and had its budget fields
/// normalized.
struct LevelContribution {
    rate_per_second: f64,
    burst_capacity: u32,
    algorithm: RateLimitAlgorithm,
    scope: RateLimitScope,
    strategy: RateLimitStrategy,
    cost: u32,
}

fn exact_contribution(config: &RateLimitConfig) -> LevelContribution {
    LevelContribution {
        rate_per_second: per_second(config),
        burst_capacity: burst_of(config),
        algorithm: config.algorithm,
        scope: config.scope,
        strategy: config.strategy,
        cost: config.cost,
    }
}

/// `inst-ratelimit-effective-limit-09`/`-10`/`-11`: the Upstream-versus-Route
/// `min()` branch, taken when the Upstream's `sharing` is `enforce`
/// (regardless of the Route), or `inherit` with the Route declaring its own
/// `rate_limit`. `route` is `None` only in the `enforce`-with-no-route case,
/// in which the missing level is neutral so the result equals the
/// Upstream's own value.
// @cpt-begin:cpt-cf-oagw-algo-ratelimit-effective-limit:p1:inst-ratelimit-effective-limit-09
// @cpt-begin:cpt-cf-oagw-algo-ratelimit-effective-limit:p1:inst-ratelimit-effective-limit-10
// @cpt-begin:cpt-cf-oagw-algo-ratelimit-effective-limit:p1:inst-ratelimit-effective-limit-11
fn min_contribution(
    upstream: &RateLimitConfig,
    route: Option<&RateLimitConfig>,
) -> LevelContribution {
    let upstream_rate = per_second(upstream);
    let upstream_burst = burst_of(upstream);
    let Some(route) = route else {
        return LevelContribution {
            rate_per_second: upstream_rate,
            burst_capacity: upstream_burst,
            algorithm: upstream.algorithm,
            scope: upstream.scope,
            strategy: upstream.strategy,
            cost: upstream.cost,
        };
    };
    LevelContribution {
        rate_per_second: upstream_rate.min(per_second(route)),
        burst_capacity: upstream_burst.min(burst_of(route)),
        // Categorical fields are selections, not budgets: the single
        // more-specific level (the Route, here) wins outright rather than
        // being min-composed (`inst-ratelimit-effective-limit-11`).
        algorithm: route.algorithm,
        scope: route.scope,
        strategy: route.strategy,
        cost: route.cost,
    }
}
// @cpt-end:cpt-cf-oagw-algo-ratelimit-effective-limit:p1:inst-ratelimit-effective-limit-11
// @cpt-end:cpt-cf-oagw-algo-ratelimit-effective-limit:p1:inst-ratelimit-effective-limit-10
// @cpt-end:cpt-cf-oagw-algo-ratelimit-effective-limit:p1:inst-ratelimit-effective-limit-09

/// `inst-ratelimit-effective-limit-03` through `-08`: compose the
/// Upstream-versus-Route pair per the `sharing`-mode inheritance table.
// @cpt-begin:cpt-cf-oagw-algo-ratelimit-effective-limit:p1:inst-ratelimit-effective-limit-03
// @cpt-begin:cpt-cf-oagw-algo-ratelimit-effective-limit:p1:inst-ratelimit-effective-limit-04
// @cpt-begin:cpt-cf-oagw-algo-ratelimit-effective-limit:p1:inst-ratelimit-effective-limit-05
// @cpt-begin:cpt-cf-oagw-algo-ratelimit-effective-limit:p1:inst-ratelimit-effective-limit-06
// @cpt-begin:cpt-cf-oagw-algo-ratelimit-effective-limit:p1:inst-ratelimit-effective-limit-07
// @cpt-begin:cpt-cf-oagw-algo-ratelimit-effective-limit:p1:inst-ratelimit-effective-limit-08
fn compose_pair(
    upstream: Option<&RateLimitConfig>,
    route: Option<&RateLimitConfig>,
) -> Option<LevelContribution> {
    let Some(upstream) = upstream else {
        return route.map(exact_contribution);
    };
    match upstream.sharing {
        Sharing::Private => route.map(exact_contribution),
        Sharing::Inherit if route.is_none() => Some(exact_contribution(upstream)),
        Sharing::Inherit | Sharing::Enforce => Some(min_contribution(upstream, route)),
    }
}
// @cpt-end:cpt-cf-oagw-algo-ratelimit-effective-limit:p1:inst-ratelimit-effective-limit-08
// @cpt-end:cpt-cf-oagw-algo-ratelimit-effective-limit:p1:inst-ratelimit-effective-limit-07
// @cpt-end:cpt-cf-oagw-algo-ratelimit-effective-limit:p1:inst-ratelimit-effective-limit-06
// @cpt-end:cpt-cf-oagw-algo-ratelimit-effective-limit:p1:inst-ratelimit-effective-limit-05
// @cpt-end:cpt-cf-oagw-algo-ratelimit-effective-limit:p1:inst-ratelimit-effective-limit-04
// @cpt-end:cpt-cf-oagw-algo-ratelimit-effective-limit:p1:inst-ratelimit-effective-limit-03

/// `cpt-cf-oagw-algo-ratelimit-effective-limit`: compose the effective
/// `rate_limit` from the resolved Upstream's/Route's own declarations plus
/// every ancestor tenant's `sharing: enforce` contribution (`ancestors`,
/// pre-filtered to enforcing levels by the caller, in any order since the
/// fold is a plain `min()`).
// @cpt-algo:cpt-cf-oagw-algo-ratelimit-effective-limit:p1
// @cpt-dod:cpt-cf-oagw-dod-ratelimit-hierarchical-budget:p1
// @cpt-begin:cpt-cf-oagw-algo-ratelimit-effective-limit:p1:inst-ratelimit-effective-limit-01
// @cpt-begin:cpt-cf-oagw-algo-ratelimit-effective-limit:p1:inst-ratelimit-effective-limit-02
// @cpt-begin:cpt-cf-oagw-algo-ratelimit-effective-limit:p1:inst-ratelimit-effective-limit-12
// @cpt-begin:cpt-cf-oagw-algo-ratelimit-effective-limit:p1:inst-ratelimit-effective-limit-13
// @cpt-begin:cpt-cf-oagw-algo-ratelimit-effective-limit:p1:inst-ratelimit-effective-limit-14
// @cpt-begin:cpt-cf-oagw-algo-ratelimit-effective-limit:p1:inst-ratelimit-effective-limit-15
// @cpt-begin:cpt-cf-oagw-algo-ratelimit-effective-limit:p1:inst-ratelimit-effective-limit-16
pub(crate) fn compose_effective_limit(
    upstream: Option<&RateLimitConfig>,
    route: Option<&RateLimitConfig>,
    ancestors: &[&RateLimitConfig],
) -> Option<EffectiveRateLimit> {
    if upstream.is_none() && route.is_none() {
        return None;
    }
    let mut contribution = compose_pair(upstream, route)?;

    for ancestor in ancestors {
        contribution.rate_per_second = contribution.rate_per_second.min(per_second(ancestor));
        contribution.burst_capacity = contribution.burst_capacity.min(burst_of(ancestor));
    }

    Some(finalize(contribution))
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "rate_per_second is a non-negative composition of u32-derived per-second rates; \
              flooring it back to a display integer never exceeds u32::MAX"
)]
fn finalize(contribution: LevelContribution) -> EffectiveRateLimit {
    let display_rate = contribution.rate_per_second.max(0.0).floor() as u32;
    EffectiveRateLimit {
        algorithm: contribution.algorithm,
        rate_per_second: contribution.rate_per_second,
        display_rate,
        burst_capacity: contribution.burst_capacity.max(1),
        scope: contribution.scope,
        strategy: contribution.strategy,
        cost: contribution.cost,
    }
}

/// RF-003: convert one already-selected `RateLimitConfig` into an
/// [`EffectiveRateLimit`] directly, bypassing [`compose_pair`]'s
/// Upstream-vs-Route `sharing` gate -- for a caller (`crate::proxy::merge::merge_rate_limit`)
/// that has already picked the single winning config through its own
/// ancestor/permission selection rule and only needs this module's
/// per-second normalization, display-rate floor, and burst default applied
/// to it.
pub(crate) fn effective_from_config(config: &RateLimitConfig) -> EffectiveRateLimit {
    finalize(exact_contribution(config))
}
// @cpt-end:cpt-cf-oagw-algo-ratelimit-effective-limit:p1:inst-ratelimit-effective-limit-16
// @cpt-end:cpt-cf-oagw-algo-ratelimit-effective-limit:p1:inst-ratelimit-effective-limit-15
// @cpt-end:cpt-cf-oagw-algo-ratelimit-effective-limit:p1:inst-ratelimit-effective-limit-14
// @cpt-end:cpt-cf-oagw-algo-ratelimit-effective-limit:p1:inst-ratelimit-effective-limit-13
// @cpt-end:cpt-cf-oagw-algo-ratelimit-effective-limit:p1:inst-ratelimit-effective-limit-12
// @cpt-end:cpt-cf-oagw-algo-ratelimit-effective-limit:p1:inst-ratelimit-effective-limit-02
// @cpt-end:cpt-cf-oagw-algo-ratelimit-effective-limit:p1:inst-ratelimit-effective-limit-01

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::upstream::{BurstConfig, SustainedRate};

    fn config(rate: u32, window: RateLimitWindow, sharing: Sharing) -> RateLimitConfig {
        RateLimitConfig {
            sharing,
            algorithm: RateLimitAlgorithm::TokenBucket,
            sustained: SustainedRate { rate, window },
            burst: None,
            scope: RateLimitScope::Tenant,
            strategy: RateLimitStrategy::Reject,
            cost: 1,
        }
    }

    #[test]
    fn neither_level_declares_a_rate_limit_yields_none() {
        assert!(compose_effective_limit(None, None, &[]).is_none());
    }

    #[test]
    fn private_upstream_yields_the_routes_limit_exactly() {
        let upstream = config(1000, RateLimitWindow::Second, Sharing::Private);
        let route = config(10, RateLimitWindow::Second, Sharing::Private);
        let effective = compose_effective_limit(Some(&upstream), Some(&route), &[]).unwrap();
        assert_eq!(effective.display_rate, 10);
        assert_eq!(effective.burst_capacity, 10);
    }

    #[test]
    fn private_upstream_with_no_route_limit_is_no_limit_configured() {
        let upstream = config(1000, RateLimitWindow::Second, Sharing::Private);
        assert!(compose_effective_limit(Some(&upstream), None, &[]).is_none());
    }

    #[test]
    fn inherit_upstream_with_no_route_limit_uses_the_upstreams_limit_exactly() {
        let upstream = config(42, RateLimitWindow::Second, Sharing::Inherit);
        let effective = compose_effective_limit(Some(&upstream), None, &[]).unwrap();
        assert_eq!(effective.display_rate, 42);
    }

    #[test]
    fn inherit_upstream_with_routes_own_limit_is_minned() {
        let upstream = config(100, RateLimitWindow::Second, Sharing::Inherit);
        let route = config(30, RateLimitWindow::Second, Sharing::Private);
        let effective = compose_effective_limit(Some(&upstream), Some(&route), &[]).unwrap();
        assert_eq!(effective.display_rate, 30);
    }

    #[test]
    fn enforce_upstream_always_minned_even_with_no_route_limit() {
        let upstream = config(50, RateLimitWindow::Second, Sharing::Enforce);
        let effective = compose_effective_limit(Some(&upstream), None, &[]).unwrap();
        assert_eq!(effective.display_rate, 50);
    }

    #[test]
    fn enforce_upstream_and_route_take_the_smaller_of_the_two() {
        let upstream = config(100, RateLimitWindow::Second, Sharing::Enforce);
        let route = config(20, RateLimitWindow::Second, Sharing::Private);
        let effective = compose_effective_limit(Some(&upstream), Some(&route), &[]).unwrap();
        assert_eq!(effective.display_rate, 20);
    }

    #[test]
    fn differing_windows_are_normalized_before_comparison() {
        // 120/minute == 2/second, smaller than a 5/second route.
        let upstream = config(120, RateLimitWindow::Minute, Sharing::Enforce);
        let route = config(5, RateLimitWindow::Second, Sharing::Private);
        let effective = compose_effective_limit(Some(&upstream), Some(&route), &[]).unwrap();
        assert_eq!(effective.display_rate, 2);
    }

    #[test]
    fn categorical_fields_come_from_the_route_when_it_declares_one() {
        let upstream = config(100, RateLimitWindow::Second, Sharing::Enforce);
        let mut route = config(20, RateLimitWindow::Second, Sharing::Private);
        route.strategy = RateLimitStrategy::Degrade;
        route.scope = RateLimitScope::Ip;
        let effective = compose_effective_limit(Some(&upstream), Some(&route), &[]).unwrap();
        assert_eq!(effective.strategy, RateLimitStrategy::Degrade);
        assert_eq!(effective.scope, RateLimitScope::Ip);
    }

    #[test]
    fn enforcing_ancestor_folds_into_a_tighter_running_minimum() {
        let upstream = config(100, RateLimitWindow::Second, Sharing::Inherit);
        let ancestor = config(10, RateLimitWindow::Second, Sharing::Enforce);
        let effective = compose_effective_limit(Some(&upstream), None, &[&ancestor]).unwrap();
        assert_eq!(effective.display_rate, 10);
    }

    #[test]
    fn missing_burst_defaults_to_the_levels_own_sustained_rate() {
        // `sharing: inherit` with no Route, so the composed pair is the
        // Upstream's own value exactly (`inst-ratelimit-effective-limit-07`),
        // exercising the burst default on that single contributing level.
        let mut upstream = config(75, RateLimitWindow::Second, Sharing::Inherit);
        upstream.burst = Some(BurstConfig { capacity: None });
        let effective = compose_effective_limit(Some(&upstream), None, &[]).unwrap();
        assert_eq!(effective.burst_capacity, 75);
    }
}
