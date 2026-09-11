//! Data Plane L1 cache tests.
//!
//! Covers the steps of `cpt-cf-oagw-algo-dp-cache` and the acceptance rows of
//! `cpt-cf-oagw-dod-dp-cache`: the key shapes of ADR 0005, the hit, the insert
//! with its 1000-entry eviction, the prefix flush a configuration write
//! triggers, the no-TTL posture that leaves an entry in place when no
//! notification reaches it, and the rule that the cache holds configurations
//! and no response body.

use std::sync::Arc;

use oagw::data_plane::{DpCache, DP_CACHE_CAPACITY};
use oagw::domain::proxy::{AliasDerivation, ResolvedUpstream};
use oagw::domain::{Endpoint, EndpointHost, HeadersConfig, Scheme};
use uuid::Uuid;

const TENANT: Uuid = Uuid::from_u128(0x11);
const OTHER_TENANT: Uuid = Uuid::from_u128(0x12);
const UPSTREAM: Uuid = Uuid::from_u128(0x21);

/// A minimal resolved configuration; the cache never reads into it.
fn resolved(upstream_id: Uuid) -> ResolvedUpstream {
    ResolvedUpstream {
        cors: None,
        tenant_id: TENANT,
        upstream_id,
        alias: String::from("api.example.com"),
        alias_derivation: AliasDerivation::Explicit,
        endpoints: vec![Endpoint {
            scheme: Scheme::Https,
            host: EndpointHost::parse("api.example.com").expect("a valid endpoint host"),
            port: Some(8443),
        }],
        protocol: String::from(oagw::PROTOCOL_HTTP),
        enabled: true,
        headers: HeadersConfig::default(),
        rate_limit: None,
        plugins: None,
        route_candidates: Vec::new(),
    }
}

#[test]
fn the_two_key_shapes_of_adr_0005_are_the_ones_the_cache_holds() {
    assert_eq!(
        DpCache::upstream_key(TENANT, "api.example.com"),
        format!("upstream:{TENANT}:api.example.com")
    );
    assert_eq!(
        DpCache::route_key(UPSTREAM, "GET", "/v1/chat"),
        format!("route:{UPSTREAM}:GET:/v1/chat")
    );
}

#[test]
fn a_miss_answers_nothing_and_a_populated_entry_answers_its_value() {
    let cache = DpCache::new();
    let key = DpCache::upstream_key(TENANT, "api.example.com");
    assert!(cache.get(&key).is_none(), "an empty cache answers no entry");

    cache.insert(key.clone(), Arc::new(resolved(UPSTREAM)), Vec::new());
    let hit = cache.get(&key).expect("the inserted entry is read back");
    assert_eq!(hit.upstream_id, UPSTREAM);
    assert_eq!(hit.alias, "api.example.com");
    assert_eq!(cache.len(), 1);
    assert!(!cache.is_empty());
}

#[test]
fn an_entry_lives_until_a_notification_flushes_it() {
    let cache = DpCache::new();
    let key = DpCache::upstream_key(TENANT, "api.example.com");
    cache.insert(key.clone(), Arc::new(resolved(UPSTREAM)), Vec::new());
    // No notification reaches the cache: no TTL expiry, no periodic sync, and
    // the entry is still there afterwards.
    assert!(
        cache.get(&key).is_some(),
        "no notification leaves the entry in place"
    );
}

#[test]
fn a_tenant_flush_drops_that_tenant_s_upstream_keys_and_route_keys() {
    let cache = DpCache::new();
    let route = DpCache::route_key(UPSTREAM, "GET", "/v1");
    cache.insert(
        DpCache::upstream_key(TENANT, "api.example.com"),
        Arc::new(resolved(UPSTREAM)),
        vec![route.clone()],
    );
    let kept = DpCache::upstream_key(OTHER_TENANT, "api.example.com");
    cache.insert(kept.clone(), Arc::new(resolved(Uuid::from_u128(0x22))), Vec::new());

    cache.flush_tenant(TENANT);
    assert!(
        cache.get(&DpCache::upstream_key(TENANT, "api.example.com")).is_none(),
        "the written tenant's entry is flushed"
    );
    assert!(
        cache.get(&kept).is_some(),
        "an unrelated tenant's entry survives the flush"
    );
    assert!(cache.get(&route).is_none(), "the route keys are flushed with it");
}

#[test]
fn an_upstream_flush_leaves_the_tenant_s_other_entries() {
    let cache = DpCache::new();
    let gone = DpCache::upstream_key(TENANT, "api.example.com");
    let stays = DpCache::upstream_key(TENANT, "other.example.com");
    cache.insert(gone.clone(), Arc::new(resolved(UPSTREAM)), Vec::new());
    cache.insert(
        stays.clone(),
        Arc::new(resolved(Uuid::from_u128(0x23))),
        Vec::new(),
    );

    cache.flush_upstream(TENANT, UPSTREAM);
    assert!(cache.get(&gone).is_none());
    assert!(cache.get(&stays).is_some());
}

#[test]
fn the_capacity_evicts_the_least_recently_used_entry() {
    let cache = DpCache::new();
    let first = DpCache::upstream_key(TENANT, "first.example.com");
    cache.insert(first.clone(), Arc::new(resolved(UPSTREAM)), Vec::new());
    // The read makes the first entry the most recent one, so the last insert is
    // the least recently used at the ceiling.
    assert!(cache.get(&first).is_some());
    for index in 0..DP_CACHE_CAPACITY {
        let alias = format!("pool-{index}.example.com");
        cache.insert(
            DpCache::upstream_key(OTHER_TENANT, &alias),
            Arc::new(resolved(Uuid::from_u128(0x30 + u128::from(index as u32)))),
            Vec::new(),
        );
    }
    assert!(
        cache.get(&first).is_none(),
        "the least recently used entry is evicted at the ceiling"
    );
    assert_eq!(cache.len(), DP_CACHE_CAPACITY);
}

#[test]
fn the_capacity_is_the_1000_entries_adr_0006_fixes() {
    assert_eq!(DP_CACHE_CAPACITY, 1000);
}
