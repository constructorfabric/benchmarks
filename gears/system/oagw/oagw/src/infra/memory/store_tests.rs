//! Tests for the in-memory [`ConfigStore`].
use super::MemoryStore;
use crate::domain::error::{DomainError, PluginReferences};
use crate::domain::gts;
use crate::domain::model::{
    Endpoint, EndpointScheme, HttpMatch, Plugin, PluginBinding, PluginKind, PluginsConfig,
    Protocol, Route, RouteMatcher, ServerConfig, Upstream,
};
use crate::domain::repo::ConfigStore;

fn tenant() -> uuid::Uuid {
    uuid::Uuid::new_v4()
}

/// A tenant id distinct from [`tenant`] — used to prove cross-tenant isolation.
fn other_tenant() -> uuid::Uuid {
    uuid::Uuid::new_v4()
}

fn endpoint(host: &str, port: u16) -> Endpoint {
    Endpoint::new(EndpointScheme::Https, host, Some(port)).expect("endpoint")
}

fn upstream(tenant_id: uuid::Uuid, alias: &str) -> Upstream {
    Upstream {
        id: uuid::Uuid::new_v4(),
        tenant_id,
        alias: alias.to_owned(),
        enabled: true,
        tags: vec![],
        server: ServerConfig {
            endpoints: vec![endpoint("api.example.com", 443)],
        },
        protocol: Protocol::Http,
        auth: None,
        headers: None,
        plugins: None,
        rate_limit: None,
        cors: None,
        created_at: 1,
        updated_at: 1,
    }
}

fn http_matcher(path: &str) -> RouteMatcher {
    RouteMatcher::Http(HttpMatch {
        methods: vec!["GET".to_owned()],
        path: path.to_owned(),
        query_allowlist: Vec::new(),
        path_suffix_mode: crate::domain::model::PathSuffixMode::Append,
    })
}

fn route(tenant_id: uuid::Uuid, upstream_id: uuid::Uuid) -> Route {
    Route {
        id: uuid::Uuid::new_v4(),
        tenant_id,
        upstream_id,
        name: None,
        tags: vec![],
        matcher: http_matcher("/v1"),
        priority: 0,
        enabled: true,
        plugins: None,
        rate_limit: None,
        cors: None,
        created_at: 1,
        updated_at: 1,
    }
}

fn plugin(tenant_id: uuid::Uuid, name: &str) -> Plugin {
    Plugin {
        id: uuid::Uuid::new_v4(),
        tenant_id,
        name: name.to_owned(),
        kind: PluginKind::Guard,
        description: None,
        config: None,
        source: "def plugin(): pass".to_owned(),
        created_at: 1,
        updated_at: 1,
    }
}

#[test]
fn insert_and_find_upstream() {
    let store = MemoryStore::new();
    let tenant = tenant();
    let upstream = upstream(tenant, "api.example.com");
    store.insert_upstream(&upstream).expect("insert");
    let found = store
        .find_upstream(tenant, upstream.id)
        .expect("read")
        .expect("present");
    assert_eq!(found.alias, "api.example.com");
    // A different tenant cannot see it.
    assert!(
        store
            .find_upstream(other_tenant(), upstream.id)
            .expect("read")
            .is_none()
    );
}

#[test]
fn duplicate_alias_is_rejected_per_tenant() {
    let store = MemoryStore::new();
    let tenant = tenant();
    store
        .insert_upstream(&upstream(tenant, "same.example.com"))
        .expect("store op");
    let error = store
        .insert_upstream(&upstream(tenant, "same.example.com"))
        .expect_err("duplicate alias");
    assert!(matches!(error, DomainError::AliasConflict { .. }));
    // The same alias in another tenant is fine.
    store
        .insert_upstream(&upstream(other_tenant(), "same.example.com"))
        .expect("other tenant");
}

#[test]
fn alias_lookup_is_scoped() {
    let store = MemoryStore::new();
    let tenant = tenant();
    store
        .insert_upstream(&upstream(tenant, "scoped.example.com"))
        .expect("store op");
    let found = store
        .find_upstream_by_alias(tenant, "scoped.example.com")
        .expect("read")
        .expect("present");
    assert_eq!(found.alias, "scoped.example.com");
    assert!(
        store
            .find_upstream_by_alias(tenant, "missing.example.com")
            .expect("read")
            .is_none()
    );
    assert!(
        store
            .find_upstream_by_alias(other_tenant(), "scoped.example.com")
            .expect("read")
            .is_none()
    );
}

#[test]
fn replace_upstream_round_trips() {
    let store = MemoryStore::new();
    let tenant = tenant();
    let mut upstream = upstream(tenant, "replace.example.com");
    store.insert_upstream(&upstream).expect("insert");
    upstream.enabled = false;
    upstream.updated_at = 42;
    let previous = store.replace_upstream(&upstream).expect("replace");
    assert!(previous.is_some());
    let found = store
        .find_upstream(tenant, upstream.id)
        .expect("read")
        .unwrap();
    assert!(!found.enabled);
    assert_eq!(found.updated_at, 42);
}

#[test]
fn replace_missing_upstream_reports_none() {
    let store = MemoryStore::new();
    let upstream = upstream(tenant(), "absent.example.com");
    assert!(
        store
            .replace_upstream(&upstream)
            .expect("replace")
            .is_none()
    );
}

#[test]
fn delete_upstream_cascade_removes_routes() {
    let store = MemoryStore::new();
    let tenant = tenant();
    let upstream = upstream(tenant, "cascade.example.com");
    store.insert_upstream(&upstream).expect("insert");
    let mut route = route(tenant, upstream.id);
    route.id = uuid::Uuid::new_v4();
    store.insert_route(&route).expect("insert");

    let (deleted, routes) = store
        .delete_upstream_cascade(tenant, upstream.id)
        .expect("cascade");
    assert_eq!(deleted.expect("upstream").id, upstream.id);
    assert_eq!(routes, 1);
    assert!(
        store
            .find_upstream(tenant, upstream.id)
            .expect("read")
            .is_none()
    );
    assert!(store.find_route(tenant, route.id).expect("read").is_none());
}

#[test]
fn delete_upstream_of_another_tenant_is_a_no_op() {
    let store = MemoryStore::new();
    let tenant = tenant();
    let upstream = upstream(tenant, "foreign.example.com");
    store.insert_upstream(&upstream).expect("insert");
    let (deleted, routes) = store
        .delete_upstream_cascade(other_tenant(), upstream.id)
        .expect("cascade");
    assert!(deleted.is_none());
    assert_eq!(routes, 0);
    // Still present for its owner.
    assert!(
        store
            .find_upstream(tenant, upstream.id)
            .expect("read")
            .is_some()
    );
}

#[test]
fn route_match_key_uniqueness_is_enforced() {
    let store = MemoryStore::new();
    let tenant = tenant();
    let owner = upstream(tenant, "routes.example.com");
    store.insert_upstream(&owner).expect("insert");

    store
        .insert_route(&route(tenant, owner.id))
        .expect("store op");
    let error = store
        .insert_route(&route(tenant, owner.id))
        .expect_err("duplicate match key");
    assert!(matches!(error, DomainError::RouteMatchConflict { .. }));

    // A different path is a different match key.
    let mut other = route(tenant, owner.id);
    other.matcher = http_matcher("/v2");
    store.insert_route(&other).expect("distinct path");

    // The same match key against another upstream is allowed.
    let other_upstream = upstream(tenant, "other.example.com");
    store.insert_upstream(&other_upstream).expect("store op");
    store
        .insert_route(&route(tenant, other_upstream.id))
        .expect("other upstream");
}

#[test]
fn route_match_key_check_respects_the_exception() {
    let store = MemoryStore::new();
    let tenant = tenant();
    let upstream = upstream(tenant, "except.example.com");
    store.insert_upstream(&upstream).expect("insert");
    let route = route(tenant, upstream.id);
    store.insert_route(&route).expect("insert");
    let key = route.uniqueness_key();
    // The route itself occupies the key, so the probe only reports free when
    // the route is excepted from the check.
    assert!(
        store
            .route_match_key_taken(tenant, upstream.id, &key, None)
            .expect("read")
    );
    assert!(
        !store
            .route_match_key_taken(tenant, upstream.id, &key, Some(route.id))
            .expect("read")
    );
}

#[test]
fn list_routes_by_upstream_only_returns_that_upstreams_routes() {
    let store = MemoryStore::new();
    let tenant = tenant();
    let a = upstream(tenant, "a.example.com");
    let b = upstream(tenant, "b.example.com");
    store.insert_upstream(&a).expect("store op");
    store.insert_upstream(&b).expect("store op");
    store.insert_route(&route(tenant, a.id)).expect("store op");
    let mut second = route(tenant, a.id);
    second.matcher = http_matcher("/other");
    store.insert_route(&second).expect("insert");
    store.insert_route(&route(tenant, b.id)).expect("store op");

    let listed = store.list_routes_by_upstream(tenant, a.id).expect("read");
    assert_eq!(listed.len(), 2);
    assert!(listed.iter().all(|route| route.upstream_id == a.id));
}

#[test]
fn plugin_names_are_unique_per_tenant() {
    let store = MemoryStore::new();
    let tenant = tenant();
    store
        .insert_plugin(&plugin(tenant, "signer"))
        .expect("insert");
    let error = store
        .insert_plugin(&plugin(tenant, "signer"))
        .expect_err("duplicate name");
    assert!(matches!(error, DomainError::Conflict { .. }));
    store
        .insert_plugin(&plugin(other_tenant(), "signer"))
        .expect("other tenant");
}

#[test]
fn plugin_references_reports_both_collections() {
    let store = MemoryStore::new();
    let tenant = tenant();
    let custom = plugin(tenant, "referenced");
    store.insert_plugin(&custom).expect("store op");
    // The service resolves a binding to the canonical ref plus the row UUID,
    // which is what the reference scan matches on.
    let chain = PluginsConfig {
        sharing: crate::domain::model::SharingMode::Private,
        items: vec![PluginBinding {
            plugin_ref: custom.gts_id(),
            plugin_uuid: Some(custom.id),
            config: None,
        }],
    };

    let upstream = upstream(tenant, "refs.example.com");
    let mut upstream = upstream;
    upstream.plugins = Some(chain.clone());
    store.insert_upstream(&upstream).expect("insert");

    let mut route = route(tenant, upstream.id);
    route.plugins = Some(chain);
    store.insert_route(&route).expect("insert");

    let references = store.plugin_references(tenant, custom.id).expect("read");
    assert_eq!(references.upstreams, vec![upstream.gts_id()]);
    assert_eq!(references.routes, vec![route.gts_id()]);
}

#[test]
fn unresolved_plugin_references_are_not_counted() {
    let store = MemoryStore::new();
    let tenant = tenant();
    let upstream = upstream(tenant, "bare.example.com");
    let mut upstream = upstream;
    upstream.plugins = Some(PluginsConfig {
        sharing: crate::domain::model::SharingMode::Private,
        items: vec![PluginBinding::bare(
            "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
        )],
    });
    store.insert_upstream(&upstream).expect("insert");

    let references = store
        .plugin_references(tenant, uuid::Uuid::new_v4())
        .expect("read");
    assert_eq!(references, PluginReferences::default());
}

#[test]
fn gts_ids_are_family_correct() {
    let tenant = tenant();
    let upstream = upstream(tenant, "family.example.com");
    assert!(
        upstream
            .gts_id()
            .starts_with(&format!("gts.{}~", gts::UPSTREAM_TYPE))
    );
    let route = route(tenant, upstream.id);
    assert!(
        route
            .gts_id()
            .starts_with(&format!("gts.{}~", gts::ROUTE_TYPE))
    );
    let plugin = plugin(tenant, "family");
    // A custom plugin is identified by its family, not by the generic plugin
    // type (`gts.cf.core.oagw.{type}_plugin.v1~{uuid}`).
    assert!(
        plugin
            .gts_id()
            .starts_with(&format!("gts.{}~", plugin.kind.base_type()))
    );
    assert!(plugin.gts_id().ends_with(&format!("~{id}", id = plugin.id)));
}
