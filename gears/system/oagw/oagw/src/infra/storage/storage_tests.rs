//! In-memory store unit tests.

use crate::domain::error::DomainError;
use crate::domain::gts;
use crate::domain::model::{
    Endpoint, EndpointScheme, MatchConfig, Plugin, PluginType, Route, ServerConfig, Upstream,
};
use crate::infra::storage::InMemoryStore;

fn upstream(tenant: uuid::Uuid, alias: &str) -> Upstream {
    Upstream {
        id: uuid::Uuid::new_v4(),
        tenant_id: tenant,
        alias: alias.to_owned(),
        protocol: gts::PROTOCOL_HTTP.to_owned(),
        enabled: true,
        server: ServerConfig {
            endpoints: vec![Endpoint {
                scheme: EndpointScheme::Https,
                host: "api.vendor.com".to_owned(),
                port: 443,
            }],
        },
        auth: None,
        headers: None,
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: Vec::new(),
        created_at: 0,
    }
}

fn route(tenant: uuid::Uuid, upstream_id: uuid::Uuid) -> Route {
    Route {
        id: uuid::Uuid::new_v4(),
        tenant_id: tenant,
        upstream_id,
        match_config: MatchConfig::default(),
        priority: 0,
        enabled: true,
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: Vec::new(),
        created_at: 0,
    }
}

#[test]
fn alias_is_unique_per_tenant() {
    let store = InMemoryStore::new();
    let tenant = uuid::Uuid::new_v4();
    store.put_upstream(upstream(tenant, "vendor.com")).unwrap();
    let err = store
        .put_upstream(upstream(tenant, "vendor.com"))
        .unwrap_err();
    assert!(matches!(err, DomainError::Conflict(_)));
    // Same alias in another tenant is fine.
    assert!(
        store
            .put_upstream(upstream(uuid::Uuid::new_v4(), "vendor.com"))
            .is_ok()
    );
    assert_eq!(store.upstream_count(), 2);
    assert_eq!(store.upstream_count(), store.all_upstreams().len());
    assert_eq!(store.route_count(), 0);
    assert_eq!(store.plugin_count(), 0);
}

#[test]
fn alias_lookup_is_case_insensitive() {
    let store = InMemoryStore::new();
    let tenant = uuid::Uuid::new_v4();
    store
        .put_upstream(upstream(tenant, "api.vendor.com"))
        .unwrap();
    assert!(store.upstream_by_alias(tenant, "API.VENDOR.COM").is_some());
    assert!(store.upstream_by_alias(tenant, "api.vendor.com.").is_none());
}

#[test]
fn tenant_scoping_hides_ancestors() {
    let store = InMemoryStore::new();
    let parent = uuid::Uuid::new_v4();
    let child = uuid::Uuid::new_v4();
    let parent_upstream = upstream(parent, "vendor.com");
    let id = parent_upstream.id;
    store.put_upstream(parent_upstream).unwrap();

    assert!(store.upstream(child, id).is_none());
    assert!(store.upstreams(child).is_empty());
    assert!(!store.remove_upstream(child, id));
    assert!(store.remove_upstream(parent, id));
}

#[test]
fn replace_rejects_foreign_tenant() {
    let store = InMemoryStore::new();
    let tenant = uuid::Uuid::new_v4();
    let mut created = upstream(tenant, "vendor.com");
    store.put_upstream(created.clone()).unwrap();
    created.alias = "renamed.vendor.com".to_owned();
    assert!(store.save_upstream(created).is_ok());

    let mut forged = upstream(tenant, "other");
    forged.id = uuid::Uuid::new_v4();
    assert!(matches!(
        store.save_upstream(forged),
        Err(DomainError::Validation(_))
    ));
}

#[test]
fn routes_are_indexed_by_upstream() {
    let store = InMemoryStore::new();
    let tenant = uuid::Uuid::new_v4();
    let created = upstream(tenant, "vendor.com");
    store.put_upstream(created.clone()).unwrap();
    let route = route(tenant, created.id);
    let route_id = route.id;
    store.put_route(route).unwrap();
    assert_eq!(store.routes_of_upstream(created.id).len(), 1);
    assert!(store.route_any_tenant(route_id).is_some());
    assert!(store.route(tenant, route_id).is_some());
    assert!(store.remove_route(tenant, route_id));
}

#[test]
fn plugins_are_name_unique_and_typed() {
    let store = InMemoryStore::new();
    let tenant = uuid::Uuid::new_v4();
    let plugin = Plugin {
        id: uuid::Uuid::new_v4(),
        tenant_id: tenant,
        plugin_type: PluginType::Guard,
        name: "my-guard".to_owned(),
        config_schema: serde_json::json!({}),
        source_code: "def guard(ctx): pass".to_owned(),
        last_used_at: None,
        gc_eligible_at: None,
        created_at: 0,
    };
    store.put_plugin(plugin.clone()).unwrap();
    let conflict = store.put_plugin(plugin.clone()).unwrap_err();
    assert!(matches!(conflict, DomainError::Conflict(_)));
    assert_eq!(store.plugins(tenant, Some(PluginType::Guard)).len(), 1);
    assert_eq!(store.plugins(tenant, Some(PluginType::Auth)).len(), 0);
    assert!(store.plugin_by_name(tenant, "my-guard").is_some());
    assert!(store.remove_plugin(plugin.id));
    assert_eq!(store.plugin_count(), 0);
}
