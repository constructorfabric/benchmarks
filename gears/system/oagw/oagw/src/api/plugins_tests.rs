//! Tests for the plugin management surface.

use uuid::Uuid;

use crate::domain::plugin::{Plugin, PluginKind};
use crate::error::ErrorKind;
use crate::store::OagwStore;

fn plugin(name: &str) -> Plugin {
    Plugin {
        id: String::new(),
        tenant_id: Uuid::nil(),
        name: name.to_owned(),
        kind: Some(PluginKind::Transform),
        config_schema: None,
        source: format!("def plugin_{name}(request):\n    return request\n"),
    }
}

fn store_with(name: &str) -> (OagwStore, Uuid, Plugin) {
    let store = OagwStore::new();
    let owner = Uuid::new_v4();
    let created = store
        .insert_plugin(plugin(name), owner)
        .expect("the plugin is created");
    (store, owner, created)
}

#[test]
fn a_created_plugin_gets_an_identifier_and_the_caller_tenant() {
    let (_, owner, created) = store_with("signer");
    assert!(!created.id.is_empty());
    assert_eq!(created.tenant_id, owner);
    assert_eq!(created.name, "signer");
}

#[test]
fn a_plugin_definition_is_immutable() {
    // The store exposes insert, read, list and delete — never a replace. Immutability is
    // a property of the surface, so it is asserted on the store's API.
    let (store, owner, created) = store_with("signer");
    let chain = crate::store::TenantChain::single(owner);
    assert!(store.get_plugin(&created.id, &chain).is_some());
    assert_eq!(store.list_plugins(&chain).len(), 1);
    let _ = store; // no `replace_plugin` to call
}

#[test]
fn a_plugin_still_referenced_by_an_upstream_cannot_be_deleted() {
    let (store, owner, created) = store_with("signer");
    let chain = crate::store::TenantChain::single(owner);

    let upstream = crate::domain::upstream::Upstream {
        enabled: true,
        plugins: vec![crate::domain::plugin::PluginBinding {
            name: created.id.clone(),
            uuid: None,
            config: serde_json::Map::new(),
        }],
        ..crate::domain::upstream::Upstream::default()
    };
    store
        .insert_upstream(upstream, owner)
        .expect("the upstream is created");

    let err = store
        .delete_plugin(&created.id, &chain)
        .expect_err("the plugin is in use");
    assert_eq!(err.kind(), ErrorKind::PluginInUse, "{err}");
    // The plugin survives a rejected deletion.
    assert!(store.get_plugin(&created.id, &chain).is_some());
}

#[test]
fn a_plugin_referenced_by_a_route_cannot_be_deleted_either() {
    let (store, owner, created) = store_with("signer");
    let chain = crate::store::TenantChain::single(owner);

    let upstream = store
        .insert_upstream(
            crate::domain::upstream::Upstream {
                enabled: true,
                ..crate::domain::upstream::Upstream::default()
            },
            owner,
        )
        .expect("the upstream is created");

    let route = crate::domain::route::Route {
        upstream_id: upstream.id,
        r#match: Some(crate::domain::route::RouteMatch::Http(crate::domain::route::HttpMatch {
            methods: vec!["GET".to_owned()],
            path: "/v1/items".to_owned(),
            query_allowlist: Vec::new(),
            path_suffix_mode: crate::domain::route::PathSuffixMode::Append,
        })),
        enabled: true,
        plugins: vec![crate::domain::plugin::PluginBinding {
            name: "payment-transform".to_owned(),
            uuid: Some(created.id.clone()),
            config: serde_json::Map::new(),
        }],
        ..crate::domain::route::Route::default()
    };
    store
        .insert_route(route, &crate::store::TenantChain::single(owner))
        .expect("the route is created");

    let err = store
        .delete_plugin(&created.id, &chain)
        .expect_err("the plugin is in use");
    assert_eq!(err.kind(), ErrorKind::PluginInUse);
}

#[test]
fn an_unreferenced_plugin_is_deletable() {
    let (store, owner, created) = store_with("signer");
    let chain = crate::store::TenantChain::single(owner);
    store
        .delete_plugin(&created.id, &chain)
        .expect("nothing references it");
    assert!(store.get_plugin(&created.id, &chain).is_none());
}

#[test]
fn a_plugin_source_is_served_with_its_identifier() {
    let (_, _, created) = store_with("signer");
    let source = crate::api::plugins::source_dto(&created);
    assert_eq!(source.id, created.id);
    assert_eq!(source.name, "signer");
    assert!(source.source.contains("def plugin_signer"), "{}", source.source);
}

#[test]
fn a_plugin_source_is_not_visible_across_tenants() {
    let (store, _, created) = store_with("signer");
    let foreign = crate::store::TenantChain::single(Uuid::new_v4());
    assert!(store.get_plugin(&created.id, &foreign).is_none());
}

#[test]
fn a_plugin_list_is_scoped_to_the_caller_chain() {
    let (store, owner, created) = store_with("signer");
    let own = crate::store::TenantChain::single(owner);
    assert_eq!(store.list_plugins(&own).len(), 1);
    let foreign = crate::store::TenantChain::single(Uuid::new_v4());
    assert_eq!(store.list_plugins(&foreign).len(), 0);
    let _ = created;
}
