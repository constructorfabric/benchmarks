//! The effective-limit fold of `cpt-cf-oagw-algo-effective-limit-fold`.
//!
//! Covers the no-limit outcome, the minimum of the visible sustained rates
//! across windows, the minimum of the visible `burst.capacity` values ADR
//! 0003's Example 1 performs beside the sustained one, and the four members
//! that carry no merge, taken from the last layer that declares each in the
//! upstream, then route, then tenant order.

// @cpt-dod:cpt-cf-oagw-dod-rate-limit-hierarchy:p1

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::time::Duration;

use oagw::domain::effective::EffectiveRateLimit;
use oagw::domain::upstream::{Algorithm, Burst, RateLimitConfig, RateLimitScope, SharingMode, Strategy, Sustained, Window};
use oagw::domain::{LimitLayers, fold};
use uuid::Uuid;

fn owner() -> Uuid {
    Uuid::from_u128(0xA11CE)
}

fn layer(rate: u64, window: Window, capacity: Option<u64>, members: Members) -> EffectiveRateLimit {
    EffectiveRateLimit {
        owner: owner(),
        mode: SharingMode::Enforce,
        rate_limit: RateLimitConfig {
            sharing: Some(SharingMode::Enforce),
            algorithm: members.algorithm,
            sustained: Some(Sustained {
                rate,
                window: Some(window),
            }),
            burst: capacity.map(|value| Burst { capacity: value }),
            scope: members.scope,
            strategy: members.strategy,
            cost: members.cost,
        },
    }
}

/// The four members a layer may declare, so a test states only the ones it
/// exercises.
#[derive(Default)]
struct Members {
    algorithm: Option<Algorithm>,
    scope: Option<RateLimitScope>,
    strategy: Option<Strategy>,
    cost: Option<u64>,
}

#[test]
fn no_layer_carries_a_limit() {
    // The no-limit outcome: no layer carries a `rate_limit`, so the check
    // enforces nothing and charges nothing (§1.5).
    let folded = fold(&LimitLayers::default());
    assert!(folded.is_none(), "an unconfigured upstream is not limited");
}

#[test]
fn a_limit_at_any_single_layer_is_enforced() {
    // A limit declared at the route layer alone is enforced, which is the
    // fold's outcome and not the two-layer look a guard once suggested.
    let route = layer(100, Window::Minute, None, Members::default());
    let folded = fold(&LimitLayers {
        upstream: None,
        route: Some(&route),
    })
    .expect("a limit at one layer is a limit");
    assert_eq!(folded.sustained.rate, 100);
    assert_eq!(folded.sustained.window, Some(Window::Minute));
    // The defaults ADR 0003's field table declares for the members no layer
    // states.
    assert_eq!(folded.algorithm, Algorithm::TokenBucket);
    assert_eq!(folded.scope, RateLimitScope::Tenant);
    assert_eq!(folded.strategy, Strategy::Reject);
    assert_eq!(folded.cost, 1);
    // The capacity defaults to the sustained rate.
    assert_eq!(folded.burst_capacity, 100);
}

#[test]
fn the_strictest_rate_wins_across_windows() {
    // `80/second` against `5000/minute` is not decidable without a common
    // unit; the merge normalized both to one scale and the fold compares on
    // it, so the sustained rate of 80 per second — 4800 per minute, under the
    // upstream's 5000 — wins and is reported in its own window.
    let upstream = layer(5000, Window::Minute, None, Members::default());
    let route = layer(80, Window::Second, None, Members::default());
    let folded = fold(&LimitLayers {
        upstream: Some(&upstream),
        route: Some(&route),
    })
    .expect("both layers carry a limit");
    assert_eq!(folded.sustained.rate, 80);
    assert_eq!(folded.sustained.window, Some(Window::Second));

    // The same two layers with the route's rate raised above the upstream's
    // answer the upstream's rate in the upstream's window.
    let looser = layer(6000, Window::Minute, None, Members::default());
    let folded = fold(&LimitLayers {
        upstream: Some(&upstream),
        route: Some(&looser),
    })
    .expect("both layers carry a limit");
    assert_eq!(folded.sustained.rate, 5000);
    assert_eq!(folded.sustained.window, Some(Window::Minute));
}

#[test]
fn the_ancestor_enforce_rate_arrives_already_folded() {
    // An ancestor `rate_limit` marked `enforce` at 10000 per minute with a
    // descendant declaring 1000 per minute enforces 1000; the same descendant
    // declaring 20000 enforces 10000. The merge folds the ancestor term into
    // the layer value, so the fold sees the two layer values the resolution
    // produced and re-walks no chain (§1.5).
    let ancestor = layer(10_000, Window::Minute, None, Members::default());
    let strict_descendant = layer(1000, Window::Minute, None, Members::default());
    let folded = fold(&LimitLayers {
        upstream: Some(&ancestor),
        route: Some(&strict_descendant),
    })
    .expect("both layers carry a limit");
    assert_eq!(folded.sustained.rate, 1000);

    let loose_descendant = layer(20_000, Window::Minute, None, Members::default());
    let folded = fold(&LimitLayers {
        upstream: Some(&ancestor),
        route: Some(&loose_descendant),
    })
    .expect("both layers carry a limit");
    assert_eq!(folded.sustained.rate, 10_000);
}

#[test]
fn the_capacity_is_min_merged_beside_the_rate() {
    // ADR 0003's Example 1: the ancestor's `burst.capacity` of 1000 against a
    // descendant's 100 enforces a capacity of 100, which is the merge performed
    // beside the sustained rate.
    let ancestor = layer(10_000, Window::Minute, Some(1000), Members::default());
    let descendant = layer(5000, Window::Minute, Some(100), Members::default());
    let folded = fold(&LimitLayers {
        upstream: Some(&ancestor),
        route: Some(&descendant),
    })
    .expect("both layers carry a limit");
    assert_eq!(folded.burst_capacity, 100);

    // A capacity no layer declares defaults to the sustained rate.
    let plain = layer(500, Window::Minute, None, Members::default());
    let folded = fold(&LimitLayers {
        upstream: Some(&plain),
        route: None,
    })
    .expect("the layer carries a limit");
    assert_eq!(folded.burst_capacity, 500);
}

#[test]
fn the_four_members_come_from_the_last_declaring_layer() {
    // The members that carry no merge are taken from the last layer that
    // declares them in the upstream, then route, then tenant order: a route
    // `cost` of 10 overrides an upstream `cost` of 1, which is ADR 0003's
    // Example 3, and a strategy declared only at one layer is the strategy
    // enforced.
    let upstream_members = Members {
        cost: Some(1),
        algorithm: Some(Algorithm::TokenBucket),
        ..Members::default()
    };
    let upstream = layer(1000, Window::Minute, None, upstream_members);

    let route_members = Members {
        cost: Some(10),
        strategy: Some(Strategy::Queue),
        scope: Some(RateLimitScope::Route),
        ..Members::default()
    };
    let route = layer(1000, Window::Minute, None, route_members);

    let folded = fold(&LimitLayers {
        upstream: Some(&upstream),
        route: Some(&route),
    })
    .expect("both layers carry a limit");
    assert_eq!(folded.cost, 10, "the route layer's cost prevails");
    assert_eq!(folded.strategy, Strategy::Queue, "declared only at the route layer");
    assert_eq!(folded.scope, RateLimitScope::Route);
    assert_eq!(folded.algorithm, Algorithm::TokenBucket, "declared only at the upstream layer");
}

#[test]
fn a_window_is_converted_to_its_length() {
    // The conversion of the `second`, `minute`, `hour`, and `day` literals the
    // shipped schema enumerates, which the sliding window expires charges on.
    assert_eq!(oagw::domain::window_millis(Some(Window::Second)), 1_000);
    assert_eq!(oagw::domain::window_millis(Some(Window::Minute)), 60_000);
    assert_eq!(oagw::domain::window_millis(Some(Window::Hour)), 3_600_000);
    assert_eq!(oagw::domain::window_millis(Some(Window::Day)), 86_400_000);
    assert_eq!(oagw::domain::window_millis(None), 1_000);
    // The common scale the fold compares on, in requests per day.
    let per_second = Sustained {
        rate: 10,
        window: Some(Window::Second),
    };
    assert_eq!(oagw::domain::per_common_scale(&per_second), 10 * 86_400);
    let _ = Duration::from_secs(1);
}
