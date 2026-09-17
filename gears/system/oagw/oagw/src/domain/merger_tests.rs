#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Unit tests of the hierarchical rate-limit merge.

use serde_json::json;
use uuid::Uuid;

use super::{EffectiveRateLimit, RateLimitLayer, merge, window_secs};
use crate::domain::model::{
    RateLimit, RateLimitAlgorithm, RateLimitScope, RateLimitStrategy, Route, SharingMode, Tag,
    Upstream,
};

/// A `rate_limit` block with the fields the tests name, everything else at its
/// schema default.
fn rate_limit(
    sharing: &str,
    rate: u32,
    window: &str,
    capacity: Option<u32>,
    cost: u32,
) -> RateLimit {
    serde_json::from_value(json!({
        "sharing": sharing,
        "sustained": { "rate": rate, "window": window },
        "burst": capacity.map(|value| json!({ "capacity": value })),
        "cost": cost,
    }))
    .expect("a valid rate_limit block")
}

fn tags(names: &[&str]) -> Vec<Tag> {
    names
        .iter()
        .map(|name| Tag::try_new(*name).expect("a valid tag"))
        .collect()
}

fn upstream(rate_limit: Option<RateLimit>, tag_names: &[&str]) -> Upstream {
    let mut upstream: Upstream = serde_json::from_value(json!({
        "server": { "endpoints": [ { "host": "api.example.com" } ] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
    }))
    .expect("a valid upstream");
    upstream.rate_limit = rate_limit;
    upstream.tags = tags(tag_names);
    upstream
}

fn route(upstream_id: Uuid, rate_limit: Option<RateLimit>, tag_names: &[&str]) -> Route {
    let mut route: Route = serde_json::from_value(json!({
        "upstream_id": upstream_id,
        "match": { "http": { "methods": ["GET"], "path": "/v1/things" } },
    }))
    .expect("a valid route");
    route.rate_limit = rate_limit;
    route.tags = tags(tag_names);
    route
}

/// The policy of exactly one layer.
fn only(limit: &RateLimit) -> EffectiveRateLimit {
    merge(&[RateLimitLayer::new(&[], Some(limit))]).expect("one layer declares a limit")
}

#[test]
fn a_single_layer_is_its_own_effective_policy() {
    let limit = rate_limit("inherit", 10, "second", Some(5), 1);
    let policy = only(&limit);

    assert_eq!(policy.sharing, SharingMode::Inherit);
    assert_eq!(policy.algorithm, RateLimitAlgorithm::TokenBucket);
    assert_eq!(policy.sustained.rate, 10);
    assert_eq!(
        policy.sustained.window,
        crate::domain::model::RateLimitWindow::Second
    );
    assert_eq!(policy.capacity, 5);
    assert_eq!(policy.scope, RateLimitScope::Tenant);
    assert_eq!(policy.strategy, RateLimitStrategy::Reject);
    assert_eq!(policy.cost, 1);
    assert!(policy.response_headers, "the ADR-0003 default is on");
}

#[test]
fn an_omitted_burst_capacity_defaults_to_the_sustained_rate() {
    let policy = only(&rate_limit("private", 7, "minute", None, 3));
    assert_eq!(policy.capacity, 7);
}

#[test]
fn enforce_merges_the_minimum_of_every_numeric_field() {
    // 6000/minute is 100/second: the child's 50/second is the stricter rate, so
    // it wins the sustained pair and the parent caps the rest.
    let parent = rate_limit("enforce", 6_000, "minute", Some(1_000), 5);
    let child = rate_limit("private", 50, "second", Some(200), 1);

    let policy = merge(&[
        RateLimitLayer::new(&tags(&["quota"]), Some(&parent)),
        RateLimitLayer::new(&tags(&["chat"]), Some(&child)),
    ])
    .expect("a merged policy");

    assert_eq!(
        policy.sharing,
        SharingMode::Enforce,
        "the cap stays in force"
    );
    assert_eq!(policy.sustained.rate, 50);
    assert_eq!(
        policy.sustained.window,
        crate::domain::model::RateLimitWindow::Second
    );
    assert_eq!(policy.capacity, 200);
    assert_eq!(policy.cost, 1);
    // The child keeps the non-numeric fields it declared.
    assert_eq!(policy.scope, RateLimitScope::Tenant);
    assert_eq!(policy.strategy, RateLimitStrategy::Reject);
    // Tags still union across the enforced layers.
    assert_eq!(
        policy.tags.iter().map(Tag::as_str).collect::<Vec<_>>(),
        vec!["quota", "chat"]
    );
}

#[test]
fn enforce_compares_rates_across_window_units() {
    // 100/minute is stricter than 10/second, so the parent's pair survives: the
    // two windows are never mixed into one number.
    let parent = rate_limit("enforce", 100, "minute", None, 1);
    let child = rate_limit("private", 10, "second", None, 1);

    let policy = merge(&[
        RateLimitLayer::new(&[], Some(&parent)),
        RateLimitLayer::new(&[], Some(&child)),
    ])
    .expect("a merged policy");

    assert_eq!(policy.sustained.rate, 100);
    assert_eq!(
        policy.sustained.window,
        crate::domain::model::RateLimitWindow::Minute
    );
    assert_eq!(
        policy.window_secs(),
        window_secs(crate::domain::model::RateLimitWindow::Minute)
    );
}

#[test]
fn inherit_yields_the_parent_configuration() {
    let parent = rate_limit("inherit", 100, "second", Some(50), 2);
    let child = rate_limit("private", 5, "second", Some(10), 1);

    let policy = merge(&[
        RateLimitLayer::new(&tags(&["shared"]), Some(&parent)),
        RateLimitLayer::new(&[], Some(&child)),
    ])
    .expect("a merged policy");

    assert_eq!(policy.sharing, SharingMode::Inherit);
    assert_eq!(policy.sustained.rate, 100, "the descendant inherits as-is");
    assert_eq!(policy.capacity, 50);
    assert_eq!(policy.cost, 2);
    assert_eq!(
        policy.tags.iter().map(Tag::as_str).collect::<Vec<_>>(),
        vec!["shared"]
    );
}

#[test]
fn private_yields_the_child_configuration_only() {
    let parent = rate_limit("private", 100, "second", Some(100), 4);
    let child = rate_limit("private", 5, "second", Some(10), 1);

    let policy = merge(&[
        RateLimitLayer::new(&tags(&["upstream"]), Some(&parent)),
        RateLimitLayer::new(&tags(&["route"]), Some(&child)),
    ])
    .expect("a merged policy");

    assert_eq!(policy.sharing, SharingMode::Private);
    assert_eq!(
        policy.sustained.rate, 5,
        "the parent does not cap the child"
    );
    assert_eq!(policy.capacity, 10);
    assert_eq!(policy.cost, 1);
    assert_eq!(
        policy.tags.iter().map(Tag::as_str).collect::<Vec<_>>(),
        vec!["upstream", "route"],
        "tags union even under a private block"
    );
}

#[test]
fn a_private_layer_stays_in_force_when_the_descendant_declares_none() {
    let parent = rate_limit("private", 100, "second", None, 1);
    let policy = merge(&[
        RateLimitLayer::new(&[], Some(&parent)),
        RateLimitLayer::new(&[], None),
    ])
    .expect("a configured limit is never silently dropped");

    assert_eq!(policy.sustained.rate, 100);
    assert_eq!(policy.capacity, 100);
}

#[test]
fn tags_merge_as_an_add_only_union() {
    let parent = rate_limit("enforce", 100, "second", None, 1);
    let child = rate_limit("private", 10, "second", None, 1);

    let policy = merge(&[
        RateLimitLayer::new(&tags(&["llm", "openai"]), Some(&parent)),
        RateLimitLayer::new(&tags(&["openai", "beta"]), Some(&child)),
    ])
    .expect("a merged policy");

    assert_eq!(
        policy.tags.iter().map(Tag::as_str).collect::<Vec<_>>(),
        vec!["llm", "openai", "beta"],
        "the ancestor's tags survive and a descendant cannot remove them"
    );
}

#[test]
fn a_resource_without_a_limit_is_not_limited() {
    let merged = merge(&[
        RateLimitLayer::new(&tags(&["llm"]), None),
        RateLimitLayer::new(&tags(&["chat"]), None),
        RateLimitLayer::tenant(),
    ]);
    assert!(merged.is_none(), "no layer declares a rate_limit block");
}

#[test]
fn the_merge_is_side_effect_free() {
    let parent = rate_limit("enforce", 100, "second", Some(500), 2);
    let child = rate_limit("private", 20, "second", Some(50), 1);
    let parent_before = parent.clone();
    let child_before = child.clone();

    let first = merge(&[
        RateLimitLayer::new(&tags(&["llm"]), Some(&parent)),
        RateLimitLayer::new(&[], Some(&child)),
    ])
    .expect("a merged policy");
    let second = merge(&[
        RateLimitLayer::new(&tags(&["llm"]), Some(&parent)),
        RateLimitLayer::new(&[], Some(&child)),
    ])
    .expect("a merged policy");

    assert_eq!(first, second);
    assert_eq!(parent, parent_before, "the merge reads the parent");
    assert_eq!(child, child_before, "the merge reads the child");
}

#[test]
fn the_proxy_path_merges_the_upstream_then_the_route() {
    let owner = upstream(
        Some(rate_limit("enforce", 100, "second", Some(500), 2)),
        &["llm"],
    );
    let matched = route(
        Uuid::new_v4(),
        Some(rate_limit("private", 20, "second", Some(50), 1)),
        &[],
    );

    let policy = super::for_upstream_route(&owner, Some(&matched)).expect("a merged policy");
    assert_eq!(policy.sustained.rate, 20);
    assert_eq!(policy.capacity, 50);
    assert_eq!(policy.cost, 1);
    assert_eq!(
        policy.tags.iter().map(Tag::as_str).collect::<Vec<_>>(),
        vec!["llm"]
    );

    // Without a matching route the upstream's own configuration is what the
    // proxy enforces.
    let policy = super::for_upstream_route(&owner, None).expect("a merged policy");
    assert_eq!(policy.sustained.rate, 100);
    assert_eq!(policy.capacity, 500);
    assert_eq!(policy.cost, 2);
}

#[test]
fn an_unconfigured_upstream_is_not_limited_even_by_a_route() {
    let owner = upstream(None, &[]);
    let matched = route(Uuid::new_v4(), None, &[]);

    assert!(super::for_upstream_route(&owner, Some(&matched)).is_none());
}

#[test]
fn the_tenant_layer_joins_the_hierarchy() {
    let parent = rate_limit("enforce", 100, "second", None, 1);
    let child = rate_limit("private", 10, "second", None, 1);
    let tenant = rate_limit("private", 30, "second", None, 1);

    let policy = merge(&[
        RateLimitLayer::new(&[], Some(&parent)),
        RateLimitLayer::new(&[], Some(&child)),
        RateLimitLayer::new(&[], Some(&tenant)),
    ])
    .expect("a merged policy");

    // The enforced cap survives the layers below it.
    assert_eq!(policy.sustained.rate, 10);
    assert_eq!(policy.sharing, SharingMode::Enforce);
}
