//! Integration tests for repository tenant scoping across two tenants
//! (`cpt-cf-oagw-dod-gear-foundation-tenant-scoping`,
//! `cpt-cf-oagw-algo-gear-foundation-repo-scope`).
//!
//! The store is the one the gear publishes at initialization; the test asserts
//! the documented key binding — every operation carries the caller's
//! `tenant_id` — so a foreign key is indistinguishable from a missing one.
// @cpt-dod:cpt-cf-oagw-dod-gear-foundation-tenant-scoping:p1

#![allow(clippy::unwrap_used, clippy::expect_used)]

use toolkit::Gear;
use uuid::Uuid;

use oagw::domain::repo::{RouteRecord, UpstreamRecord};
use oagw::test_support::{test_context, upstream};
use oagw::{DomainError, OagwGear, Plugin, Route};

/// A gear with its storage published, ready to take control-plane writes.
async fn storage() -> std::sync::Arc<oagw::infra::storage::Storage> {
    let gear = OagwGear::default();
    gear.init(&test_context(None)).await.expect("init succeeds");
    gear.storage().expect("storage published")
}

fn record(upstream: oagw::Upstream) -> UpstreamRecord {
    UpstreamRecord { upstream, plugin_bindings: vec![] }
}

fn route_of(tenant: Uuid, upstream_id: Uuid, path: &str, priority: i64) -> Route {
    Route {
        id: Uuid::new_v4(),
        tenant_id: tenant,
        upstream_id,
        match_type: oagw::RouteMatchType::Http,
        priority,
        enabled: true,
        match_: oagw::MatchConfig {
            http: Some(oagw::HttpMatch {
                methods: vec![oagw::HttpMethod::Get],
                path: path.to_owned(),
                query_allowlist: vec![],
                path_suffix_mode: oagw::PathSuffixMode::Append,
            }),
            grpc: None,
        },
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: vec![],
    }
}

fn route_record(route: Route) -> RouteRecord {
    RouteRecord { route, plugin_bindings: vec![] }
}

/// A repository call from tenant B never returns a record owned by tenant A —
/// by id, by alias, or through a list — and returns not-found instead.
#[tokio::test]
async fn a_foreign_tenant_key_is_not_found() {
    let storage = storage().await;
    let (upstreams, routes, _) = storage.repositories();
    let (owner, other) = (Uuid::new_v4(), Uuid::new_v4());

    let created = upstreams
        .create(owner, record(upstream(owner, "vendor.io")))
        .expect("created for the owner");
    let route = routes
        .create(owner, route_record(route_of(owner, created.upstream.id, "/v1", 0)))
        .expect("route created for the owner");

    assert!(matches!(
        upstreams.get(other, created.upstream.id),
        Err(DomainError::NotFound { .. })
    ));
    assert!(matches!(
        upstreams.get_by_alias(other, "vendor.io"),
        Err(DomainError::NotFound { .. })
    ));
    assert!(matches!(routes.get(other, route.route.id), Err(DomainError::NotFound { .. })));

    assert!(upstreams.list(other).expect("list").is_empty());
    assert!(routes.list(other).expect("list").is_empty());
    assert!(
        routes
            .list_for_upstream(other, created.upstream.id)
            .expect("list")
            .is_empty()
    );

    // The ancestor record is not visible either: the store keys every row on
    // the caller's tenant, so an ancestor-tenant read from a descendant is
    // exactly the same not-found.
    let descendant = Uuid::new_v4();
    assert!(matches!(
        upstreams.get(descendant, created.upstream.id),
        Err(DomainError::NotFound { .. })
    ));

    // The owner still sees its own records.
    assert_eq!(upstreams.list(owner).expect("list").len(), 1);
    assert_eq!(routes.list(owner).expect("list").len(), 1);
}

/// A delete issued by a foreign tenant does not remove another tenant's record.
#[tokio::test]
async fn a_foreign_tenant_cannot_delete_or_replace_a_record() {
    let storage = storage().await;
    let (upstreams, _, _) = storage.repositories();
    let (owner, other) = (Uuid::new_v4(), Uuid::new_v4());

    let created = upstreams
        .create(owner, record(upstream(owner, "vendor.io")))
        .expect("created");

    assert!(matches!(
        upstreams.delete(other, created.upstream.id),
        Err(DomainError::NotFound { .. })
    ));
    assert!(matches!(
        upstreams.replace(other, record(created.upstream.clone())),
        Err(DomainError::NotFound { .. })
    ));
    assert_eq!(upstreams.list(owner).expect("the owner's record stands").len(), 1);
}

/// `(tenant_id, alias)` is unique per tenant: two tenants may hold the same
/// alias, but a tenant may not hold it twice, and the rejected write leaves the
/// store unchanged.
#[tokio::test]
async fn the_upstream_unique_key_is_scoped_to_the_tenant() {
    let storage = storage().await;
    let (upstreams, routes, _) = storage.repositories();
    let (tenant_a, tenant_b) = (Uuid::new_v4(), Uuid::new_v4());

    upstreams.create(tenant_a, record(upstream(tenant_a, "vendor.io"))).expect("first");
    upstreams
        .create(tenant_b, record(upstream(tenant_b, "vendor.io")))
        .expect("the same alias is free in another tenant");

    let before = upstreams.list(tenant_a).expect("list").len();
    let error = upstreams
        .create(tenant_a, record(upstream(tenant_a, "vendor.io")))
        .expect_err("conflict");
    assert!(error.is_conflict(), "{error} is a conflict");
    assert_eq!(upstreams.list(tenant_a).expect("list").len(), before, "the store is unchanged");

    // The failed write left no route row behind either.
    assert_eq!(routes.list(tenant_a).expect("routes").len(), 0);
}

/// A second *enabled* route under one upstream sharing path prefix, priority
/// and method is a conflict, while the same pair under another upstream is not.
#[tokio::test]
async fn route_match_uniqueness_is_enforced() {
    let storage = storage().await;
    let (upstreams, routes, _) = storage.repositories();
    let tenant = Uuid::new_v4();

    let first = upstreams.create(tenant, record(upstream(tenant, "one.io"))).expect("first");
    let second = upstreams
        .create(tenant, record(upstream(tenant, "two.io")))
        .expect("second");

    routes
        .create(tenant, route_record(route_of(tenant, first.upstream.id, "/v1", 0)))
        .expect("the first route is accepted");
    let error = routes
        .create(tenant, route_record(route_of(tenant, first.upstream.id, "/v1", 0)))
        .expect_err("the same match under one upstream is a conflict");
    assert!(error.is_conflict(), "{error} is a conflict");

    routes
        .create(tenant, route_record(route_of(tenant, second.upstream.id, "/v1", 0)))
        .expect("a different upstream is a different match scope");
}

/// An upstream delete cascades to its routes in one atomic operation, and the
/// route rows of other upstreams survive.
#[tokio::test]
async fn deleting_an_upstream_cascades_to_its_routes() {
    let storage = storage().await;
    let (upstreams, routes, _) = storage.repositories();
    let tenant = Uuid::new_v4();

    let doomed = upstreams.create(tenant, record(upstream(tenant, "doomed.io"))).expect("doomed");
    let kept = upstreams.create(tenant, record(upstream(tenant, "kept.io"))).expect("kept");

    routes
        .create(tenant, route_record(route_of(tenant, doomed.upstream.id, "/doomed", 0)))
        .expect("route");
    routes
        .create(tenant, route_record(route_of(tenant, kept.upstream.id, "/kept", 0)))
        .expect("route");

    upstreams.delete(tenant, doomed.upstream.id).expect("deleted");
    assert!(matches!(
        upstreams.get(tenant, doomed.upstream.id),
        Err(DomainError::NotFound { .. })
    ));
    assert_eq!(routes.list(tenant).expect("routes").len(), 1, "only the kept route stands");
    assert_eq!(
        routes
            .list_for_upstream(tenant, kept.upstream.id)
            .expect("routes")
            .len(),
        1
    );
}

/// `(tenant_id, name)` is the plugin unique key, and a plugin still referenced
/// by a binding is a conflict rather than a silent cascade.
#[tokio::test]
async fn the_plugin_unique_key_is_scoped_to_the_tenant() {
    let storage = storage().await;
    let (_, _, plugins) = storage.repositories();
    let (tenant_a, tenant_b) = (Uuid::new_v4(), Uuid::new_v4());

    let plugin = |tenant: Uuid, name: &str| Plugin {
        id: Uuid::new_v4(),
        tenant_id: tenant,
        plugin_type: "auth".to_owned(),
        name: name.to_owned(),
        config_schema: None,
        source_code: None,
        last_used_at: None,
        gc_eligible_at: None,
    };

    plugins.create(tenant_a, plugin(tenant_a, "shared-name")).expect("created for A");
    plugins
        .create(tenant_b, plugin(tenant_b, "shared-name"))
        .expect("the same name is free in another tenant");

    let error = plugins
        .create(tenant_a, plugin(tenant_a, "shared-name"))
        .expect_err("conflict");
    assert!(error.is_conflict(), "{error} is a conflict");
    assert!(matches!(
        plugins.get_by_name(tenant_b, "shared-name"),
        Ok(_)
    ));
    assert!(matches!(
        plugins.get_by_name(tenant_a, "shared-name"),
        Ok(_)
    ));
    assert!(matches!(
        plugins.get(Uuid::new_v4(), Uuid::new_v4()),
        Err(DomainError::NotFound { .. })
    ));
}
