//! Tests of the in-memory control-plane store.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(coverage_nightly, coverage(off))]

use uuid::Uuid;

use super::*;
use crate::test_support::{http_route, tenant, upstream, upstream_spec};

#[test]
fn lookups_are_scoped_to_the_tenant() {
    let owner = tenant();
    let other = tenant();
    let state = StoreState {
        upstreams: vec![upstream(&upstream_spec(owner, "api.example.com", None))],
        routes: Vec::new(),
        plugins: Vec::new(),
    };
    let id = state.upstreams[0].id;
    assert!(state.upstream(owner, id).is_some());
    assert!(state.upstream(other, id).is_none());
}

#[test]
fn alias_holder_reports_the_tenant_scoped_owner() {
    let owner = tenant();
    let other = tenant();
    let state = StoreState {
        upstreams: vec![upstream(&upstream_spec(owner, "api.example.com", None))],
        routes: Vec::new(),
        plugins: Vec::new(),
    };
    let alias = state.upstreams[0].alias.clone();
    assert!(state.alias_holder(owner, &alias).is_some());
    assert!(state.alias_holder(other, &alias).is_none());
    let holder = state.alias_holder(owner, &alias).unwrap();
    assert_eq!(
        state.alias_holder_excluding(owner, &alias, holder),
        None,
        "excluding the holder clears the collision"
    );
    assert_eq!(
        state.alias_holder_excluding(owner, &alias, Uuid::new_v4()),
        Some(holder)
    );
}

#[test]
fn route_match_keys_are_canonical_and_order_insensitive() {
    let tenant_id = tenant();
    let state = StoreState {
        upstreams: Vec::new(),
        routes: vec![http_route(
            tenant_id,
            Uuid::new_v4(),
            "/v1/chat",
            &["POST", "GET"],
        )],
        plugins: Vec::new(),
    };
    let route = &state.routes[0];
    let key = match_key_of(route);
    assert_eq!(key, "http /v1/chat GET,POST");
    assert_eq!(
        state.route_with_match_key(tenant_id, route.upstream_id, &key),
        Some(route.id)
    );
    // The key is order-insensitive: the same route declared with a different
    // method order canonicalizes to the identical key, so the store recognises
    // it as a duplicate.
    let reordered = http_route(tenant_id, route.upstream_id, "/v1/chat", &["POST", "GET"]);
    assert_eq!(match_key_of(&reordered), key);
    assert_eq!(
        state.route_with_match_key(tenant_id, Uuid::new_v4(), &key),
        None,
        "another upstream may claim the same match rule"
    );
}

#[test]
fn used_by_reports_the_first_referencing_resource() {
    let owner = tenant();
    let upstream_spec = upstream_spec(owner, "api.example.com", None);
    let state = StoreState {
        upstreams: vec![upstream(&upstream_spec)],
        routes: Vec::new(),
        plugins: Vec::new(),
    };
    let upstream_id = state.upstreams[0].id;
    assert_eq!(state.upstream_used_by(owner, upstream_id), None);

    let mut routes = state.routes.clone();
    routes.push(http_route(owner, upstream_id, "/v1", &["GET"]));
    let state = StoreState {
        upstreams: state.upstreams.clone(),
        routes,
        plugins: Vec::new(),
    };
    assert_eq!(
        state.upstream_used_by(owner, upstream_id),
        Some(format!("route {}", state.routes[0].id))
    );
}
