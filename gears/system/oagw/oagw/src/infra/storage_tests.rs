//! Tenant scoping and uniqueness invariants of the configuration store.

use super::*;
use crate::domain::model::{
    ConfigMap, Endpoint, HttpMatch, MatchConfig, PathSuffixMode, PluginBinding, PluginKind,
    PluginsConfig, Protocol, Scheme, ServerConfig, SharingMode,
};

fn upstream(tenant: Uuid, alias: &str) -> Upstream {
    Upstream {
        id: Uuid::new_v4(),
        tenant_id: tenant,
        alias: alias.to_owned(),
        enabled: true,
        protocol: Protocol::Http,
        server: ServerConfig {
            endpoints: vec![Endpoint {
                scheme: Scheme::Https,
                host: "api.example.com".to_owned(),
                port: 443,
            }],
        },
        auth: None,
        headers: None,
        plugins: None,
        rate_limit: None,
        cors: None,
        tags: Vec::new(),
    }
}

fn route(tenant: Uuid, upstream_id: Uuid, path: &str, method: &str, priority: i32) -> Route {
    Route {
        id: Uuid::new_v4(),
        tenant_id: tenant,
        upstream_id,
        enabled: true,
        priority,
        r#match: MatchConfig {
            http: Some(HttpMatch {
                methods: vec![method.to_owned()],
                path: path.to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        },
        plugins: None,
        rate_limit: None,
        cors: None,
        tags: Vec::new(),
    }
}

fn plugin(tenant: Uuid, name: &str) -> PluginDef {
    PluginDef {
        id: Uuid::new_v4(),
        tenant_id: tenant,
        plugin_type: PluginKind::Guard,
        name: name.to_owned(),
        description: None,
        phases: Vec::new(),
        config_schema: None,
        source_code: "def on_request(ctx): pass".to_owned(),
        last_used_at: None,
        gc_eligible_at: None,
    }
}

#[test]
fn an_alias_is_unique_per_tenant_not_globally() {
    let store = InMemoryStore::new();
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    UpstreamRepository::insert(&store, upstream(a, "api.example.com")).unwrap();
    assert_eq!(
        UpstreamRepository::insert(&store, upstream(a, "api.example.com"))
            .unwrap_err()
            .status(),
        409
    );
    UpstreamRepository::insert(&store, upstream(b, "api.example.com")).unwrap();
}

#[test]
fn reads_and_writes_are_scoped_to_the_owning_tenant() {
    let store = InMemoryStore::new();
    let owner = Uuid::new_v4();
    let other = Uuid::new_v4();
    let created = UpstreamRepository::insert(&store, upstream(owner, "api.example.com")).unwrap();

    assert!(UpstreamRepository::get(&store, other, created.id).is_none());
    assert!(UpstreamRepository::get(&store, owner, created.id).is_some());
    assert!(!UpstreamRepository::delete(&store, other, created.id));
    assert!(UpstreamRepository::delete(&store, owner, created.id));
}

#[test]
fn the_data_plane_can_read_an_upstream_across_tenants() {
    let store = InMemoryStore::new();
    let owner = Uuid::new_v4();
    let created = UpstreamRepository::insert(&store, upstream(owner, "api.example.com")).unwrap();
    assert!(UpstreamRepository::get_unscoped(&store, created.id).is_some());
    assert_eq!(store.find_alias_across_tenants("api.example.com").len(), 1);
}

#[test]
fn replacing_an_upstream_under_another_tenant_is_a_not_found() {
    let store = InMemoryStore::new();
    let owner = Uuid::new_v4();
    let mut created = UpstreamRepository::insert(&store, upstream(owner, "api.example.com")).unwrap();
    created.tenant_id = Uuid::new_v4();
    assert_eq!(
        UpstreamRepository::replace(&store, created).unwrap_err().status(),
        404
    );
}

#[test]
fn two_enabled_routes_may_not_share_path_priority_and_method() {
    let store = InMemoryStore::new();
    let tenant = Uuid::new_v4();
    let upstream_id = Uuid::new_v4();
    RouteRepository::insert(&store, route(tenant, upstream_id, "/v1", "GET", 0))
        .unwrap();
    assert_eq!(
        RouteRepository::insert(&store, route(tenant, upstream_id, "/v1", "GET", 0))
            .unwrap_err()
            .status(),
        409
    );
    // A different method, priority or upstream is fine.
    RouteRepository::insert(&store, route(tenant, upstream_id, "/v1", "POST", 0))
        .unwrap();
    RouteRepository::insert(&store, route(tenant, upstream_id, "/v1", "GET", 5))
        .unwrap();
    RouteRepository::insert(&store, route(tenant, Uuid::new_v4(), "/v1", "GET", 0))
        .unwrap();
}

#[test]
fn a_disabled_route_does_not_reserve_its_match_rule() {
    let store = InMemoryStore::new();
    let tenant = Uuid::new_v4();
    let upstream_id = Uuid::new_v4();
    let mut disabled = route(tenant, upstream_id, "/v1", "GET", 0);
    disabled.enabled = false;
    RouteRepository::insert(&store, disabled).unwrap();
    RouteRepository::insert(&store, route(tenant, upstream_id, "/v1", "GET", 0))
        .unwrap();
}

#[test]
fn routes_cascade_when_their_upstream_goes_away() {
    let store = InMemoryStore::new();
    let tenant = Uuid::new_v4();
    let upstream_id = Uuid::new_v4();
    RouteRepository::insert(&store, route(tenant, upstream_id, "/a", "GET", 0))
        .unwrap();
    RouteRepository::insert(&store, route(tenant, upstream_id, "/b", "GET", 0))
        .unwrap();
    assert_eq!(store.delete_by_upstream(upstream_id), 2);
    assert!(RouteRepository::list_by_upstream(&store, upstream_id).is_empty());
}

#[test]
fn a_plugin_name_is_unique_per_tenant() {
    let store = InMemoryStore::new();
    let tenant = Uuid::new_v4();
    PluginRepository::insert(&store, plugin(tenant, "guard")).unwrap();
    assert_eq!(
        PluginRepository::insert(&store, plugin(tenant, "guard"))
            .unwrap_err()
            .status(),
        409
    );
    PluginRepository::insert(&store, plugin(Uuid::new_v4(), "guard")).unwrap();
}

#[test]
fn plugin_references_are_found_through_auth_and_chain_bindings() {
    let store = InMemoryStore::new();
    let tenant = Uuid::new_v4();
    let plugin_id = Uuid::new_v4();

    let mut bound_upstream = upstream(tenant, "chain.example.com");
    bound_upstream.plugins = Some(PluginsConfig {
        sharing: SharingMode::Private,
        items: vec![PluginBinding {
            plugin_ref: format!("{}{plugin_id}", crate::domain::gts::GUARD_PLUGIN_BASE),
            plugin_uuid: Some(plugin_id),
            config: ConfigMap::new(),
        }],
    });
    let chain_id = UpstreamRepository::insert(&store, bound_upstream).unwrap().id;

    let mut auth_upstream = upstream(tenant, "auth.example.com");
    auth_upstream.auth = Some(crate::domain::model::AuthConfig {
        plugin_type: Some(format!(
            "{}{plugin_id}",
            crate::domain::gts::AUTH_PLUGIN_BASE
        )),
        sharing: SharingMode::Private,
        config: ConfigMap::new(),
    });
    let auth_id = UpstreamRepository::insert(&store, auth_upstream).unwrap().id;

    let mut bound_route = route(tenant, chain_id, "/v1", "GET", 0);
    bound_route.plugins = Some(PluginsConfig {
        sharing: SharingMode::Private,
        items: vec![PluginBinding {
            plugin_ref: plugin_id.to_string(),
            plugin_uuid: Some(plugin_id),
            config: ConfigMap::new(),
        }],
    });
    let route_id = RouteRepository::insert(&store, bound_route).unwrap().id;

    let (upstreams, routes) = store.references_to_plugin(plugin_id);
    assert!(upstreams.contains(&chain_id));
    assert!(upstreams.contains(&auth_id));
    assert_eq!(routes, vec![route_id]);
    assert!(store.unlinked_plugins().is_empty());
}

#[test]
fn garbage_collection_removes_only_plugins_past_their_deadline() {
    let store = InMemoryStore::new();
    let tenant = Uuid::new_v4();
    let due = PluginRepository::insert(&store, plugin(tenant, "due")).unwrap();
    let later = PluginRepository::insert(&store, plugin(tenant, "later")).unwrap();

    PluginRepository::set_gc_eligible_at(&store, due.id, Some(100));
    PluginRepository::set_gc_eligible_at(&store, later.id, Some(10_000));

    let collected = store.collect_garbage(500);
    assert_eq!(collected, vec![due.id]);
    assert!(PluginRepository::get(&store, tenant, due.id).is_none());
    assert!(PluginRepository::get(&store, tenant, later.id).is_some());
}

#[test]
fn touching_a_plugin_clears_its_collection_deadline() {
    let store = InMemoryStore::new();
    let tenant = Uuid::new_v4();
    let created = PluginRepository::insert(&store, plugin(tenant, "used")).unwrap();
    PluginRepository::set_gc_eligible_at(&store, created.id, Some(100));
    PluginRepository::touch(&store, created.id, 12_345);

    let reloaded = PluginRepository::get(&store, tenant, created.id).unwrap();
    assert_eq!(reloaded.last_used_at, Some(12_345));
    assert_eq!(reloaded.gc_eligible_at, None);
}

#[test]
fn an_unlinked_plugin_is_reported_as_collectable() {
    let store = InMemoryStore::new();
    let tenant = Uuid::new_v4();
    let created = PluginRepository::insert(&store, plugin(tenant, "orphan")).unwrap();
    assert_eq!(store.unlinked_plugins(), vec![created.id]);
}

#[test]
fn listings_are_stable_and_tenant_scoped() {
    let store = InMemoryStore::new();
    let tenant = Uuid::new_v4();
    UpstreamRepository::insert(&store, upstream(tenant, "b.example.com")).unwrap();
    UpstreamRepository::insert(&store, upstream(tenant, "a.example.com")).unwrap();
    UpstreamRepository::insert(&store, upstream(Uuid::new_v4(), "c.example.com")).unwrap();

    let aliases: Vec<String> = UpstreamRepository::list(&store, tenant)
        .into_iter()
        .map(|u| u.alias)
        .collect();
    assert_eq!(aliases, ["a.example.com", "b.example.com"]);
}

#[test]
fn the_sweep_marks_then_collects_only_long_unlinked_plugins() {
    let store = InMemoryStore::new();
    let tenant = Uuid::new_v4();
    let orphan = PluginRepository::insert(&store, plugin(tenant, "orphan")).unwrap();

    // First pass only sets the deadline.
    assert!(store.run_gc(1_000, 100).is_empty());
    assert_eq!(
        PluginRepository::get(&store, tenant, orphan.id)
            .unwrap()
            .gc_eligible_at,
        Some(1_100)
    );

    // Still inside the window.
    assert!(store.run_gc(1_050, 100).is_empty());
    // Past it.
    assert_eq!(store.run_gc(1_200, 100), vec![orphan.id]);
    assert!(PluginRepository::get(&store, tenant, orphan.id).is_none());
}

#[test]
fn rebinding_a_plugin_clears_its_deadline() {
    let store = InMemoryStore::new();
    let tenant = Uuid::new_v4();
    let plugin_row = PluginRepository::insert(&store, plugin(tenant, "reused")).unwrap();

    store.run_gc(1_000, 100);
    assert!(
        PluginRepository::get(&store, tenant, plugin_row.id)
            .unwrap()
            .gc_eligible_at
            .is_some()
    );

    let mut binder = upstream(tenant, "binder.example.com");
    binder.plugins = Some(PluginsConfig {
        sharing: SharingMode::Private,
        items: vec![PluginBinding {
            plugin_ref: format!("{}{}", crate::domain::gts::GUARD_PLUGIN_BASE, plugin_row.id),
            plugin_uuid: Some(plugin_row.id),
            config: ConfigMap::new(),
        }],
    });
    UpstreamRepository::insert(&store, binder).unwrap();

    // Now referenced again: the sweep must forget the deadline, not collect it.
    assert!(store.run_gc(1_200, 100).is_empty());
    assert_eq!(
        PluginRepository::get(&store, tenant, plugin_row.id)
            .unwrap()
            .gc_eligible_at,
        None
    );
}
