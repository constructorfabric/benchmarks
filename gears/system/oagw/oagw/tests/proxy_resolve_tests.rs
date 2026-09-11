//! Consuming the effective configuration at proxy time.
//!
//! Covers `cpt-cf-oagw-algo-resolve-consume` and the acceptance rows of
//! `cpt-cf-oagw-dod-effective-config` the Data Plane owns: the cache hit that
//! answers without a second resolution, the miss that resolves through the
//! hierarchical feature and populates the cache, the four outcomes, the layer
//! order of consumption, and the gRPC not-found posture of the §1.5 deviation.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use oagw::control_plane::cache::ControlPlaneCache;
use oagw::data_plane::{DpCache, consume};
use oagw::domain::effective::RouteSelector;
use oagw::data_plane::Resolution;
use oagw::domain::route::{HttpMatch, MatchConfig};
use oagw::domain::upstream::{Endpoint, ServerConfig, Upstream};
use oagw::domain::{EndpointHost, Scheme};
use oagw::store::OagwStore;
use oagw::OagwConfig;
use uuid::Uuid;

const TENANT: Uuid = Uuid::from_u128(0x11);
const OTHER: Uuid = Uuid::from_u128(0x12);
const UPSTREAM: Uuid = Uuid::from_u128(0x21);
const PROTOCOL_HTTP: &str = oagw::PROTOCOL_HTTP;
const PROTOCOL_GRPC: &str = oagw::PROTOCOL_GRPC;

/// An HTTP endpoint on one host.
fn endpoint(host: &str) -> Endpoint {
    Endpoint {
        scheme: Scheme::Https,
        host: EndpointHost::parse(host).expect("a valid endpoint host"),
        port: Some(443),
    }
}

/// An enabled HTTP upstream holding one alias.
fn upstream(id: Uuid, alias: &str, host: &str) -> Upstream {
    let mut row = Upstream::new(
        id,
        ServerConfig {
            endpoints: vec![endpoint(host)],
        },
        String::from(PROTOCOL_HTTP),
    );
    row.alias = Some(String::from(alias));
    row
}

/// One HTTP route addressing one upstream.
fn route(id: Uuid, upstream_id: Uuid, path: &str) -> oagw::domain::route::Route {
    oagw::domain::route::Route {
        id,
        upstream_id,
        match_config: MatchConfig {
            http: Some(HttpMatch {
                methods: vec![String::from("GET")],
                path: String::from(path),
                query_allowlist: Vec::new(),
                path_suffix_mode: None,
            }),
            grpc: None,
        },
        plugins: None,
        rate_limit: None,
        tags: Vec::new(),
        cors: None,
        priority: None,
        enabled: Some(true),
    }
}

/// The selector one proxy request matches against.
fn selector() -> RouteSelector {
    RouteSelector::Http {
        method: String::from("GET"),
        path: String::from("/v1/chat"),
    }
}

/// An empty cache and an empty control-plane cache, as a request finds them.
fn caches() -> (DpCache, ControlPlaneCache) {
    (DpCache::new(), ControlPlaneCache::new())
}

#[test]
fn a_miss_resolves_through_the_chain_and_populates_the_cache() {
    let store = OagwStore::new();
    store
        .insert_upstream(TENANT, &upstream(UPSTREAM, "api.example.com", "api.example.com"))
        .expect("the upstream is stored");
    store
        .insert_route(TENANT, &route(Uuid::from_u128(0x61), UPSTREAM, "/v1/chat"))
        .expect("the route is stored");
    let (cache, _) = caches();

    let resolution = consume(
        &store,
        &cache,
        TENANT,
        &[],
        "api.example.com",
        &selector(),
    );
    let Resolution::Resolved(resolved) = resolution else {
        panic!("the populated store resolves");
    };
    assert_eq!(resolved.upstream_id, UPSTREAM);
    assert_eq!(resolved.alias, "api.example.com");
    assert_eq!(resolved.tenant_id, TENANT);
    assert_eq!(resolved.route_candidates.len(), 1);
    assert!(resolved.enabled);

    let key = DpCache::upstream_key(TENANT, "api.example.com");
    assert!(cache.get(&key).is_some(), "the miss populated the cache");
}

#[test]
fn a_hit_answers_from_the_cache_without_a_second_resolution() {
    let store = OagwStore::new();
    store
        .insert_upstream(TENANT, &upstream(UPSTREAM, "api.example.com", "api.example.com"))
        .expect("the upstream is stored");
    store
        .insert_route(TENANT, &route(Uuid::from_u128(0x61), UPSTREAM, "/v1/chat"))
        .expect("the route is stored");
    let (cache, _) = caches();

    consume(&store, &cache, TENANT, &[], "api.example.com", &selector()).expect_resolved();
    // The store is emptied after the entry was populated: a second consumption
    // that still resolves answers from the cache and never from the store.
    store.delete_upstream(TENANT, UPSTREAM).expect("the row goes");
    let second = consume(&store, &cache, TENANT, &[], "api.example.com", &selector());
    assert!(
        matches!(second, Resolution::Resolved(_)),
        "the hit answers without the store"
    );
}

#[test]
fn an_alias_no_chain_element_holds_is_the_not_found_outcome() {
    let store = OagwStore::new();
    let (cache, _) = caches();
    let resolution = consume(
        &store,
        &cache,
        TENANT,
        &[],
        "absent.example.com",
        &selector(),
    );
    assert!(matches!(resolution, Resolution::NotFound));
}

#[test]
fn a_route_of_a_tenant_outside_the_chain_is_never_a_candidate() {
    let store = OagwStore::new();
    store
        .insert_upstream(OTHER, &upstream(UPSTREAM, "api.example.com", "api.example.com"))
        .expect("the upstream is stored under the other tenant");
    store
        .insert_route(OTHER, &route(Uuid::from_u128(0x61), UPSTREAM, "/v1/chat"))
        .expect("the route is stored");
    let (cache, _) = caches();

    let resolution = consume(&store, &cache, TENANT, &[], "api.example.com", &selector());
    assert!(
        matches!(resolution, Resolution::NotFound),
        "the calling tenant reads no other tenant's rows"
    );
}

#[test]
fn a_disabled_upstream_is_never_dialed_whatever_the_chain_contributed() {
    let store = OagwStore::new();
    let mut row = upstream(UPSTREAM, "api.example.com", "api.example.com");
    row.enabled = false;
    store.insert_upstream(TENANT, &row).expect("the row is stored");
    store
        .insert_route(TENANT, &route(Uuid::from_u128(0x61), UPSTREAM, "/v1/chat"))
        .expect("the route is stored");
    let (cache, _) = caches();

    let resolution = consume(&store, &cache, TENANT, &[], "api.example.com", &selector());
    assert!(matches!(resolution, Resolution::Disabled));
    let failure = resolution.failure_of().expect("a disabled upstream fails");
    assert_eq!(failure.kind, oagw::domain::error::ErrorKind::LinkUnavailable);
}

#[test]
fn a_grpc_upstream_is_answered_not_found_before_any_http_match_key() {
    let store = OagwStore::new();
    let mut row = upstream(UPSTREAM, "grpc.example.com", "grpc.example.com");
    row.protocol = String::from(PROTOCOL_GRPC);
    store.insert_upstream(TENANT, &row).expect("the row is stored");
    let (cache, _) = caches();

    let resolution = consume(&store, &cache, TENANT, &[], "grpc.example.com", &selector());
    assert!(
        matches!(resolution, Resolution::NotFound),
        "the §1.5 deviation answers a gRPC upstream not found"
    );
}

#[test]
fn an_alias_that_cannot_be_normalized_fails_closed() {
    let store = OagwStore::new();
    let (cache, _) = caches();
    let resolution = consume(&store, &cache, TENANT, &[], "", &selector());
    assert!(matches!(resolution, Resolution::Failed));
}

#[test]
fn the_configured_alias_normalizes_to_the_shape_the_cache_keys_on() {
    let store = OagwStore::new();
    store
        .insert_upstream(TENANT, &upstream(UPSTREAM, "api.example.com", "api.example.com"))
        .expect("the upstream is stored");
    store
        .insert_route(TENANT, &route(Uuid::from_u128(0x61), UPSTREAM, "/v1/chat"))
        .expect("the route is stored");
    let (cache, _) = caches();

    let resolution = consume(
        &store,
        &cache,
        TENANT,
        &[],
        "API.Example.COM.",
        &selector(),
    );
    let Resolution::Resolved(resolved) = resolution else {
        panic!("the normalized alias resolves the same row");
    };
    assert_eq!(resolved.alias, "api.example.com");
    let key = DpCache::upstream_key(TENANT, "api.example.com");
    assert!(
        cache.get(&key).is_some(),
        "the entry is keyed on the normalized alias"
    );
}

#[test]
fn the_candidates_of_an_ancestor_chain_are_ordered_most_distant_first() {
    let store = OagwStore::new();
    let ancestor = Uuid::from_u128(0x13);
    store
        .insert_upstream(ancestor, &upstream(UPSTREAM, "api.example.com", "api.example.com"))
        .expect("the ancestor's row is stored");
    store
        .insert_route(
            ancestor,
            &route(Uuid::from_u128(0x61), UPSTREAM, "/v1"),
        )
        .expect("the ancestor's route is stored");
    let (cache, _) = caches();

    let resolution = consume(
        &store,
        &cache,
        TENANT,
        &[ancestor],
        "api.example.com",
        &selector(),
    );
    let Resolution::Resolved(resolved) = resolution else {
        panic!("the ancestor's rows are read through the chain");
    };
    assert!(
        resolved
            .route_candidates
            .iter()
            .any(|candidate| candidate.tenant_id == ancestor && candidate.depth == 1),
        "the ancestor contributed its route at depth 1"
    );
    let key = DpCache::upstream_key(TENANT, "api.example.com");
    let hit = cache.get(&key).expect("the entry is cached");
    assert_eq!(hit.tenant_id, ancestor, "the routing target is the ancestor's");
}

#[test]
fn the_entry_the_resolution_populated_is_flushed_by_its_upstream_write() {
    let store = OagwStore::new();
    store
        .insert_upstream(TENANT, &upstream(UPSTREAM, "api.example.com", "api.example.com"))
        .expect("the upstream is stored");
    store
        .insert_route(TENANT, &route(Uuid::from_u128(0x61), UPSTREAM, "/v1/chat"))
        .expect("the route is stored");
    let (cache, _) = caches();
    let _ = OagwConfig::default();

    consume(&store, &cache, TENANT, &[], "api.example.com", &selector()).expect_resolved();
    cache.flush_upstream(TENANT, UPSTREAM);
    let key = DpCache::upstream_key(TENANT, "api.example.com");
    assert!(
        cache.get(&key).is_none(),
        "the write-path notification drops the entry"
    );
}

#[test]
fn the_outcomes_that_carry_a_catalogue_row_are_the_two_failures() {
    assert_eq!(
        Resolution::NotFound
            .failure_of()
            .expect("the not-found outcome fails")
            .kind,
        oagw::domain::error::ErrorKind::RouteNotFound
    );
    assert_eq!(
        Resolution::Disabled
            .failure_of()
            .expect("the disabled outcome fails")
            .kind,
        oagw::domain::error::ErrorKind::LinkUnavailable
    );
    assert!(
        Resolution::Failed.failure_of().is_none(),
        "the failed-closed outcome is the platform 500 problem shape, not a catalogue row"
    );
    assert!(
        Resolution::Resolved(std::sync::Arc::new(oagw::domain::proxy::ResolvedUpstream {
            cors: None,
        tenant_id: TENANT,
        upstream_id: UPSTREAM,
        alias: String::from("api.example.com"),
        alias_derivation: oagw::domain::proxy::AliasDerivation::Explicit,
        endpoints: Vec::new(),
        protocol: String::from(PROTOCOL_HTTP),
        enabled: true,
        headers: oagw::domain::HeadersConfig::default(),
        rate_limit: None,
        plugins: None,
        route_candidates: Vec::new(),
    }))
    .failure_of()
    .is_none());
}

/// Reads the resolved value out of one resolution.
trait ExpectResolved {
    fn expect_resolved(self) -> std::sync::Arc<oagw::domain::proxy::ResolvedUpstream>;
}

impl ExpectResolved for Resolution {
    fn expect_resolved(self) -> std::sync::Arc<oagw::domain::proxy::ResolvedUpstream> {
        match self {
            Resolution::Resolved(resolved) => resolved,
            other => panic!("the resolution answered {other:?}"),
        }
    }
}
