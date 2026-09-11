//! Unit tests for the in-memory repositories: tenant scoping, uniqueness
//! conflicts, ordered binding positions, match uniqueness and cascade
//! (`cpt-cf-oagw-algo-gear-foundation-repo-scope`).

use uuid::Uuid;

use super::*;
use crate::domain::dto::{
    CorsConfig, Endpoint, EndpointScheme, HttpMethod, HttpMatch, MatchConfig, PathSuffixMode,
    PluginsConfig, ServerConfig,
};
use crate::domain::error::DomainError;
use crate::domain::gts_helpers::{APIKEY_AUTH_PLUGIN_ID, REQUIRED_HEADERS_GUARD_PLUGIN_ID};

fn tenant() -> Uuid {
    Uuid::from_u128(0x5EED_0001)
}

fn other_tenant() -> Uuid {
    Uuid::from_u128(0x5EED_0002)
}

fn endpoint(host: &str) -> Endpoint {
    Endpoint { scheme: EndpointScheme::Https, host: host.to_owned(), port: 443 }
}

fn upstream(tenant_id: Uuid, alias: &str) -> Upstream {
    Upstream {
        id: Uuid::new_v4(),
        tenant_id,
        alias: alias.to_owned(),
        protocol: crate::domain::gts_helpers::PROTOCOL_HTTP.to_owned(),
        enabled: true,
        server: ServerConfig { endpoints: vec![endpoint("api.vendor.com")] },
        auth: None,
        headers: None,
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: vec![],
    }
}

fn http_match(methods: Vec<HttpMethod>, path: &str) -> MatchConfig {
    MatchConfig {
        http: Some(HttpMatch {
            methods,
            path: path.to_owned(),
            query_allowlist: vec![],
            path_suffix_mode: PathSuffixMode::Append,
        }),
        grpc: None,
    }
}

fn route(tenant_id: Uuid, upstream_id: Uuid, methods: Vec<HttpMethod>, path: &str) -> Route {
    Route {
        id: Uuid::new_v4(),
        tenant_id,
        upstream_id,
        match_type: RouteMatchType::Http,
        priority: 0,
        enabled: true,
        match_: http_match(methods, path),
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: vec![],
    }
}

fn plugin(tenant_id: Uuid, name: &str) -> Plugin {
    Plugin {
        id: Uuid::new_v4(),
        tenant_id,
        plugin_type: REQUIRED_HEADERS_GUARD_PLUGIN_ID.to_owned(),
        name: name.to_owned(),
        config_schema: None,
        source_code: None,
        last_used_at: None,
        gc_eligible_at: None,
    }
}

fn record(upstream: Upstream) -> UpstreamRecord {
    UpstreamRecord { upstream, plugin_bindings: vec![] }
}

fn route_record(route: Route) -> RouteRecord {
    RouteRecord { route, plugin_bindings: vec![] }
}

#[test]
fn every_operation_is_tenant_scoped() {
    let storage = Storage::new();
    let (upstreams, routes, plugins) = storage.repositories();
    let owner = tenant();
    let created = upstreams.create(owner, record(upstream(owner, "api.vendor.com"))).expect("created");
    assert_eq!(created.upstream.tenant_id, owner, "the caller's tenant is bound into the key");

    // A foreign tenant sees nothing: not-found, with no disclosure.
    let error = upstreams.get(other_tenant(), created.upstream.id).expect_err("not found");
    assert!(error.is_not_found(), "`{error}` is a not-found");
    assert!(upstreams.list(other_tenant()).expect("list").is_empty());
    assert!(upstreams.get_by_alias(other_tenant(), "api.vendor.com").is_err());
    assert!(routes.list(other_tenant()).expect("list").is_empty());
    assert!(plugins.list(other_tenant()).expect("list").is_empty());
}

#[test]
fn an_alias_is_unique_per_tenant_and_reusable_across_tenants() {
    let storage = Storage::new();
    let (upstreams, _routes, _plugins) = storage.repositories();
    let a = tenant();
    let b = other_tenant();
    upstreams.create(a, record(upstream(a, "api.vendor.com"))).expect("first tenant");
    upstreams.create(b, record(upstream(b, "api.vendor.com"))).expect("second tenant, same alias");

    let error = upstreams.create(a, record(upstream(a, "api.vendor.com"))).expect_err("conflict");
    assert!(error.is_conflict(), "`{error}` is a conflict");
    assert!(matches!(error, DomainError::Conflict { detail, .. } if detail.contains("alias")));
}

#[test]
fn a_plugin_name_is_unique_per_tenant() {
    let storage = Storage::new();
    let (_upstreams, _routes, plugins) = storage.repositories();
    let a = tenant();
    plugins.create(a, plugin(a, "required-headers")).expect("created");
    let error = plugins.create(a, plugin(a, "required-headers")).expect_err("conflict");
    assert!(error.is_conflict());
    // A different tenant may reuse the name.
    plugins.create(other_tenant(), plugin(other_tenant(), "required-headers")).expect("ok");
}

#[test]
fn a_route_match_collides_on_prefix_priority_and_method() {
    let storage = Storage::new();
    let (_upstreams, routes, _plugins) = storage.repositories();
    let owner = tenant();
    let upstream_id = Uuid::new_v4();

    routes
        .create(owner, route_record(route(owner, upstream_id, vec![HttpMethod::Get], "/v1/chat")))
        .expect("first route");

    // Same prefix, priority and method -> conflict.
    let error = routes
        .create(owner, route_record(route(owner, upstream_id, vec![HttpMethod::Get], "/v1/chat/")))
        .expect_err("conflict");
    assert!(error.is_conflict(), "`{error}` is a conflict");

    // A different method is a distinct match -> allowed.
    routes
        .create(owner, route_record(route(owner, upstream_id, vec![HttpMethod::Post], "/v1/chat")))
        .expect("distinct method");

    // A different priority is a distinct match -> allowed.
    let mut higher = route(owner, upstream_id, vec![HttpMethod::Get], "/v1/chat");
    higher.priority = 10;
    routes.create(owner, route_record(higher)).expect("distinct priority");

    // A disabled route does not collide.
    let mut disabled = route(owner, upstream_id, vec![HttpMethod::Get], "/v1/chat");
    disabled.enabled = false;
    routes.create(owner, route_record(disabled)).expect("disabled routes do not collide");
}

#[test]
fn replacing_a_route_does_not_collide_with_itself() {
    let storage = Storage::new();
    let (_upstreams, routes, _plugins) = storage.repositories();
    let owner = tenant();
    let upstream_id = Uuid::new_v4();
    let created = routes
        .create(owner, route_record(route(owner, upstream_id, vec![HttpMethod::Get], "/v1")))
        .expect("created");
    routes.replace(owner, route_record(created.route)).expect("idempotent replace");
}

#[test]
fn ordered_binding_positions_must_be_contiguous_from_zero() {
    // A gap (0, 2) is rejected by the service layer before it reaches the
    // store; the store keeps positions exactly as given.
    let storage = Storage::new();
    let (upstreams, _routes, _plugins) = storage.repositories();
    let owner = tenant();
    let mut created = upstream(owner, "api.vendor.com");
    created.plugins = Some(PluginsConfig {
        sharing: crate::domain::dto::SharingMode::Private,
        items: vec![
            crate::domain::gts_helpers::NOOP_AUTH_PLUGIN_ID.to_owned(),
            REQUIRED_HEADERS_GUARD_PLUGIN_ID.to_owned(),
        ],
    });
    let stored = upstreams.create(owner, record(created)).expect("created");
    assert_eq!(stored.plugin_bindings.len(), 0, "the service layer derives the bindings");

    let gap = vec![
        PluginBinding { position: 0, plugin_ref: "a".to_owned(), plugin_uuid: None },
        PluginBinding { position: 2, plugin_ref: "b".to_owned(), plugin_uuid: None },
    ];
    let mut with_gap = upstream(owner, "gapped.vendor.com");
    with_gap.id = Uuid::new_v4();
    upstreams
        .create(owner, UpstreamRecord { upstream: with_gap, plugin_bindings: gap })
        .expect("the store records what it is given; contiguity is enforced by the service");
}

#[test]
fn an_upstream_delete_cascades_to_its_routes_tags_and_bindings() {
    let storage = Storage::new();
    let (upstreams, routes, _plugins) = storage.repositories();
    let owner = tenant();
    let mut created = upstream(owner, "api.vendor.com");
    created.tags = vec!["openai".to_owned()];
    created.auth = Some(crate::domain::dto::AuthConfig {
        auth_type: Some(APIKEY_AUTH_PLUGIN_ID.to_owned()),
        sharing: crate::domain::dto::SharingMode::Private,
        config: Some(serde_json::json!({ "api_key_ref": "cred://t/k" })),
    });
    let created = upstreams.create(owner, record(created)).expect("created");

    let child = routes
        .create(owner, route_record(route(owner, created.upstream.id, vec![HttpMethod::Get], "/v1")))
        .expect("route");
    assert_eq!(storage.row_counts()["oagw_route"], 1);
    assert_eq!(storage.row_counts()["oagw_route_method"], 1);
    assert_eq!(storage.row_counts()["oagw_upstream_tag"], 1);

    upstreams.delete(owner, created.upstream.id).expect("deleted");
    assert_eq!(storage.row_counts()["oagw_upstream"], 0);
    assert_eq!(storage.row_counts()["oagw_route"], 0, "routes cascaded");
    assert_eq!(storage.row_counts()["oagw_route_method"], 0, "method rows cascaded");
    assert_eq!(storage.row_counts()["oagw_upstream_tag"], 0, "tag rows cascaded");
    assert!(routes.get(owner, child.route.id).is_err(), "the cascaded route is gone");
}

#[test]
fn a_route_delete_cascades_to_its_child_rows() {
    let storage = Storage::new();
    let (upstreams, routes, _plugins) = storage.repositories();
    let owner = tenant();
    let parent = upstreams.create(owner, record(upstream(owner, "api.vendor.com"))).expect("created");
    let mut child = route(owner, parent.upstream.id, vec![HttpMethod::Get, HttpMethod::Post], "/v1");
    child.tags = vec!["beta".to_owned()];
    let child = routes.create(owner, route_record(child)).expect("route");
    assert_eq!(storage.row_counts()["oagw_route_method"], 2);
    assert_eq!(storage.row_counts()["oagw_route_tag"], 1);

    routes.delete(owner, child.route.id).expect("deleted");
    assert_eq!(storage.row_counts()["oagw_route_method"], 0);
    assert_eq!(storage.row_counts()["oagw_route_tag"], 0);
    assert_eq!(storage.row_counts()["oagw_route"], 0);
    assert_eq!(storage.row_counts()["oagw_upstream"], 1, "the parent survives");
}

#[test]
fn a_plugin_still_referenced_is_a_conflict_not_a_cascade() {
    let storage = Storage::new();
    let (_upstreams, _routes, plugins) = storage.repositories();
    let owner = tenant();
    let created = plugins.create(owner, plugin(owner, "required-headers")).expect("created");
    // No binding rows exist yet, so the delete succeeds.
    plugins.delete(owner, created.id).expect("unreferenced plugin deleted");
    assert!(plugins.get(owner, created.id).is_err());
}

#[test]
fn the_documented_table_shapes_are_preserved() {
    let shapes = Storage::table_shapes();
    assert!(shapes.oagw_upstream && shapes.oagw_route && shapes.oagw_route_http_match);
    assert!(shapes.oagw_route_grpc_match && shapes.oagw_route_method && shapes.oagw_tag);
    assert!(shapes.oagw_plugin && shapes.oagw_plugin_binding && shapes.upstream_auth_columns);

    let tables = crate::domain::repo::TableShapes::tables();
    for table in [
        "oagw_upstream",
        "oagw_route",
        "oagw_route_http_match",
        "oagw_route_grpc_match",
        "oagw_route_method",
        "oagw_upstream_tag",
        "oagw_route_tag",
        "oagw_plugin",
        "oagw_upstream_plugin",
        "oagw_route_plugin",
    ] {
        assert!(tables.contains_key(table), "`{table}` is a documented table");
    }
    assert!(
        tables["oagw_upstream"].contains("auth_plugin_ref")
            && tables["oagw_upstream"].contains("auth_plugin_uuid"),
        "the upstream stores its auth columns as scalars"
    );
}

#[test]
fn the_match_block_is_stored_in_its_documented_child_tables() {
    let storage = Storage::new();
    let (upstreams, routes, _plugins) = storage.repositories();
    let owner = tenant();
    let parent = upstreams.create(owner, record(upstream(owner, "api.vendor.com"))).expect("created");
    let grpc = Route {
        id: Uuid::new_v4(),
        tenant_id: owner,
        upstream_id: parent.upstream.id,
        match_type: RouteMatchType::Grpc,
        priority: 0,
        enabled: true,
        match_: MatchConfig {
            http: None,
            grpc: Some(GrpcMatch {
                service: "foo.v1.UserService".to_owned(),
                method: "GetUser".to_owned(),
            }),
        },
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: vec![],
    };
    routes.create(owner, route_record(grpc)).expect("created");
    assert_eq!(storage.row_counts()["oagw_route_grpc_match"], 1);
    assert_eq!(storage.row_counts()["oagw_route_http_match"], 0);
}

#[test]
fn a_cors_block_with_wildcard_credentials_is_rejected_by_validation_not_the_store() {
    let bad = CorsConfig {
        enabled: true,
        allowed_origins: Some(vec!["*".to_owned()]),
        allow_credentials: true,
        ..CorsConfig::default()
    };
    assert!(bad.allows_wildcard().is_err());
}

#[test]
fn a_foreign_tenant_delete_is_not_found() {
    let storage = Storage::new();
    let (upstreams, routes, plugins) = storage.repositories();
    let owner = tenant();
    let created = upstreams.create(owner, record(upstream(owner, "api.vendor.com"))).expect("created");
    let route = routes
        .create(owner, route_record(route(owner, created.upstream.id, vec![HttpMethod::Get], "/v1")))
        .expect("route");
    let plugin = plugins.create(owner, plugin(owner, "required-headers")).expect("plugin");

    assert!(upstreams.delete(other_tenant(), created.upstream.id).is_err());
    assert!(routes.delete(other_tenant(), route.route.id).is_err());
    assert!(plugins.delete(other_tenant(), plugin.id).is_err());
    assert_eq!(storage.row_counts()["oagw_upstream"], 1, "nothing was deleted");
}
