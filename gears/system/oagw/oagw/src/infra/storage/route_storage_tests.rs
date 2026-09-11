//! Unit tests for the route store of entry 2.3: the stored route-level
//! overrides, the binding-row agreement, the removal of a cascaded route and
//! the conflict the store itself raises
//! (`cpt-cf-oagw-dod-route-management-route-overrides`).
//!
//! The tenant scoping, the cascade, the table shapes and the match-collision
//! rule are the foundation entry's tests; this module covers the parts the
//! route-management write path relies on.

use uuid::Uuid;

use super::*;
use crate::domain::dto::{
    BurstCapacity, CorsConfig, Endpoint, EndpointScheme, HttpMethod, HttpMatch, MatchConfig,
    PathSuffixMode, PluginsConfig, RateLimitConfig, RateWindow, ServerConfig, SharingMode,
    SustainedRate,
};
use crate::domain::gts_helpers::{NOOP_AUTH_PLUGIN_ID, REQUIRED_HEADERS_GUARD_PLUGIN_ID};

fn record(upstream: Upstream) -> UpstreamRecord {
    UpstreamRecord { upstream, plugin_bindings: vec![] }
}

fn route_record(route: Route) -> RouteRecord {
    RouteRecord { route, plugin_bindings: vec![] }
}

fn tenant() -> Uuid {
    Uuid::from_u128(0x5EED_1001)
}

fn other_tenant() -> Uuid {
    Uuid::from_u128(0x5EED_1002)
}

fn upstream(tenant_id: Uuid, alias: &str) -> Upstream {
    Upstream {
        id: Uuid::new_v4(),
        tenant_id,
        alias: alias.to_owned(),
        protocol: crate::domain::gts_helpers::PROTOCOL_HTTP.to_owned(),
        enabled: true,
        server: ServerConfig {
            endpoints: vec![Endpoint { scheme: EndpointScheme::Https, host: "api.vendor.com".to_owned(), port: 443 }],
        },
        auth: None,
        headers: None,
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: vec![],
    }
}

fn route(tenant_id: Uuid, upstream_id: Uuid, path: &str) -> Route {
    Route {
        id: Uuid::new_v4(),
        tenant_id,
        upstream_id,
        match_type: RouteMatchType::Http,
        priority: 0,
        enabled: true,
        match_: MatchConfig {
            http: Some(HttpMatch {
                methods: vec![HttpMethod::Get],
                path: path.to_owned(),
                query_allowlist: vec![],
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        },
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: vec![],
    }
}

/// A route-level override block is stored exactly as it was handed over: no
/// implicit block is materialized and none is dropped.
#[test]
fn the_route_level_overrides_are_stored_as_given() {
    let storage = Storage::new();
    let (upstreams, routes, _plugins) = storage.repositories();
    let owner = tenant();
    let parent = upstreams.create(owner, record(upstream(owner, "api.vendor.com"))).expect("created");

    let mut candidate = route(owner, parent.upstream.id, "/v1/orders");
    candidate.rate_limit = Some(RateLimitConfig {
        sharing: SharingMode::Inherit,
        sustained: SustainedRate { rate: 120, window: RateWindow::default() },
        burst: Some(BurstCapacity { capacity: 10 }),
        ..serde_json::from_str::<RateLimitConfig>("{\"sustained\":{\"rate\":120}}").expect("defaults")
    });
    candidate.cors = Some(CorsConfig {
        sharing: SharingMode::Inherit,
        enabled: true,
        allowed_origins: Some(vec!["https://console.example.com".to_owned()]),
        allowed_methods: vec!["GET".to_owned(), "POST".to_owned()],
        expose_headers: vec!["x-request-id".to_owned()],
        allow_credentials: false,
    });
    candidate.plugins = Some(PluginsConfig {
        sharing: SharingMode::Inherit,
        items: vec![NOOP_AUTH_PLUGIN_ID.to_owned()],
    });
    candidate.tags = vec!["beta".to_owned(), "orders".to_owned()];

    let stored = routes.create(owner, route_record(candidate)).expect("created");
    let read = routes.get(owner, stored.route.id).expect("read back");
    assert_eq!(read.route.rate_limit, stored.route.rate_limit, "the rate limit override");
    assert_eq!(read.route.cors, stored.route.cors, "the cors override");
    assert_eq!(read.route.plugins, stored.route.plugins, "the plugin overrides");
    assert_eq!(read.route.tags, vec!["beta".to_owned(), "orders".to_owned()]);
    assert_eq!(read.route.match_.http.as_ref().expect("http block").path, "/v1/orders");
    assert_eq!(
        read.route.match_.http.as_ref().expect("http block").path_suffix_mode,
        PathSuffixMode::Append,
        "the stored match block keeps the declared default it was given"
    );

    // A route without any override block carries none.
    let plain = routes
        .create(owner, route_record(route(owner, parent.upstream.id, "/v2/billing")))
        .expect("created");
    let read = routes.get(owner, plain.route.id).expect("read back");
    assert!(read.route.rate_limit.is_none(), "no implicit rate limit");
    assert!(read.route.cors.is_none(), "no implicit cors block");
    assert!(read.route.plugins.is_none(), "no implicit plugin block");
    assert!(read.route.tags.is_empty(), "no implicit tag row");
}

/// Every binding row keeps its `plugin_ref`; `plugin_uuid` is present exactly
/// when the reference is UUID-backed; positions stay contiguous from zero.
#[test]
fn the_binding_rows_agree_with_their_references() {
    let storage = Storage::new();
    let (upstreams, routes, _plugins) = storage.repositories();
    let owner = tenant();
    let parent = upstreams.create(owner, record(upstream(owner, "api.vendor.com"))).expect("created");

    let custom = Uuid::new_v4();
    let mut candidate = route(owner, parent.upstream.id, "/v1/orders");
    candidate.plugins = Some(PluginsConfig {
        sharing: SharingMode::Inherit,
        items: vec![
            NOOP_AUTH_PLUGIN_ID.to_owned(),
            custom.to_string(),
            REQUIRED_HEADERS_GUARD_PLUGIN_ID.to_owned(),
        ],
    });
    let stored = routes
        .create(
            owner,
            RouteRecord {
                route: candidate,
                plugin_bindings: vec![
                    PluginBinding { position: 0, plugin_ref: NOOP_AUTH_PLUGIN_ID.to_owned(), plugin_uuid: None },
                    PluginBinding { position: 1, plugin_ref: custom.to_string(), plugin_uuid: Some(custom) },
                    PluginBinding {
                        position: 2,
                        plugin_ref: REQUIRED_HEADERS_GUARD_PLUGIN_ID.to_owned(),
                        plugin_uuid: None,
                    },
                ],
            },
        )
        .expect("created");
    assert_eq!(stored.plugin_bindings.len(), 3, "one row per entry");
    for (position, binding) in stored.plugin_bindings.iter().enumerate() {
        assert_eq!(binding.position, position as u32, "positions are contiguous from zero");
        assert!(!binding.plugin_ref.is_empty(), "`plugin_ref` is always stored");
    }
    assert_eq!(stored.plugin_bindings[0].plugin_uuid, None, "a GTS reference is not UUID-backed");
    assert_eq!(stored.plugin_bindings[1].plugin_uuid, Some(custom), "a UUID reference is");
    assert_eq!(stored.plugin_bindings[2].plugin_ref, REQUIRED_HEADERS_GUARD_PLUGIN_ID);

    // The bindings survive a read and are ordered by position.
    let read = routes.get(owner, stored.route.id).expect("read back");
    assert_eq!(read.plugin_bindings, stored.plugin_bindings);

    // A route delete cascades to its binding rows.
    routes.delete(owner, stored.route.id).expect("deleted");
    assert_eq!(storage.row_counts()["oagw_route_plugin"], 0, "the bindings cascaded");
}

/// The store enforces the match-rule uniqueness itself, inside the write-path
/// critical section, so a detected collision leaves the store unchanged.
#[test]
fn a_detected_collision_leaves_the_store_unchanged() {
    let storage = Storage::new();
    let (upstreams, routes, _plugins) = storage.repositories();
    let owner = tenant();
    let parent = upstreams.create(owner, record(upstream(owner, "api.vendor.com"))).expect("created");
    routes.create(owner, route_record(route(owner, parent.upstream.id, "/v1/orders"))).expect("first");
    let before = storage.row_counts().clone();

    let error = routes
        .create(owner, route_record(route(owner, parent.upstream.id, "/v1/orders")))
        .expect_err("the same method, path and priority collides");
    assert!(error.is_conflict(), "`{error}` is a conflict");
    assert_eq!(storage.row_counts(), before, "no row was written");
}

/// A route whose owning upstream was deleted is removed with it: every
/// operation on the removed identifier behaves as not-found.
#[test]
fn a_removed_route_behaves_as_not_found() {
    let storage = Storage::new();
    let (upstreams, routes, _plugins) = storage.repositories();
    let owner = tenant();
    let parent = upstreams.create(owner, record(upstream(owner, "api.vendor.com"))).expect("created");
    let stored = routes
        .create(owner, route_record(route(owner, parent.upstream.id, "/v1/orders")))
        .expect("created");

    upstreams.delete(owner, parent.upstream.id).expect("the upstream is deleted");
    let error = routes.get(owner, stored.route.id).expect_err("removed with its upstream");
    assert!(error.is_not_found(), "`{error}` is a not-found");
    assert!(routes.get(other_tenant(), stored.route.id).is_err());
    assert!(routes.list(owner).expect("list").is_empty());
    assert!(routes.list_for_upstream(owner, parent.upstream.id).expect("list").is_empty());
    assert!(routes.replace(owner, stored.clone()).is_err(), "a write at a removed record");
    assert!(routes.delete(owner, stored.route.id).is_err(), "a second delete is not-found");
    assert_eq!(storage.row_counts()["oagw_route"], 0);
}
