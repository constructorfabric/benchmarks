//! Tests for the upstream DTO and the store's upstream surface.

use uuid::Uuid;

use crate::api::dto::UpstreamDto;
use crate::domain::upstream::{Endpoint, Server, Upstream};
use crate::store::{OagwStore, TenantChain};

fn tenant() -> Uuid {
    Uuid::new_v4()
}

fn endpoint(host: &str, port: u16, scheme: &str) -> Endpoint {
    Endpoint {
        scheme: scheme.to_owned(),
        host: host.to_owned(),
        port,
    }
}

fn upstream(host: &str) -> Upstream {
    Upstream {
        enabled: true,
        server: Server {
            endpoints: vec![endpoint(host, 443, "https")],
            ..Server::default()
        },
        ..Upstream::default()
    }
}

#[test]
fn a_created_upstream_gets_an_identifier_and_the_caller_tenant() {
    let store = OagwStore::new();
    let owner = tenant();
    let created = store
        .insert_upstream(upstream("api.example.com"), owner)
        .expect("the upstream is created");
    assert!(!created.id.is_empty());
    assert_eq!(created.tenant_id, owner);
    assert_eq!(created.alias, "api.example.com");
}

#[test]
fn the_alias_is_derived_from_the_endpoint_pool() {
    let mut entity = upstream("a.example.com");
    entity.server.endpoints.push(endpoint("b.example.com", 443, "https"));
    let store = OagwStore::new();
    let created = store
        .insert_upstream(entity, tenant())
        .expect("the upstream is created");
    assert_eq!(created.alias, "example.com");
}

#[test]
fn a_tenant_sees_its_own_upstream_and_not_another_tenants() {
    let store = OagwStore::new();
    let mine = tenant();
    let theirs = tenant();
    let created = store
        .insert_upstream(upstream("api.example.com"), mine)
        .expect("created");

    let own_chain = TenantChain::single(mine);
    let foreign_chain = TenantChain::single(theirs);
    assert!(store
        .find_upstream_by_alias("api.example.com", &own_chain)
        .is_some());
    assert!(
        store
            .find_upstream_by_alias("api.example.com", &foreign_chain)
            .is_none(),
        "an alias must not resolve across tenants"
    );
    assert!(store.get_upstream(&created.id, &foreign_chain).is_none());
    assert_eq!(store.list_upstreams(&foreign_chain).len(), 0);
    assert_eq!(store.list_upstreams(&own_chain).len(), 1);
}

#[test]
fn an_ancestor_sees_a_descendants_upstream() {
    let store = OagwStore::new();
    let parent = tenant();
    let child = tenant();
    store
        .insert_upstream(upstream("api.example.com"), parent)
        .expect("created");

    let descendant_chain = TenantChain::new(vec![child, parent]);
    assert!(
        store
            .find_upstream_by_alias("api.example.com", &descendant_chain)
            .is_some(),
        "the closest tenant wins, so an ancestor's upstream answers a descendant"
    );
}

#[test]
fn an_ancestor_does_not_see_a_descendants_upstream() {
    let store = OagwStore::new();
    let parent = tenant();
    let child = tenant();
    store
        .insert_upstream(upstream("api.example.com"), child)
        .expect("created");

    let chain = TenantChain::single(parent);
    assert!(
        store
            .find_upstream_by_alias("api.example.com", &chain)
            .is_none(),
        "the parent's chain does not reach a descendant's upstream"
    );
}

#[test]
fn a_replacement_cannot_move_an_upstream_to_another_tenant() {
    let store = OagwStore::new();
    let owner = tenant();
    let created = store
        .insert_upstream(upstream("api.example.com"), owner)
        .expect("created");

    let mut replacement = upstream("api.example.com");
    replacement.enabled = false;
    let replaced = store
        .replace_upstream(&created.id, replacement, &TenantChain::single(owner))
        .expect("replaced");
    assert_eq!(replaced.tenant_id, owner);
    assert!(!replaced.enabled);
}

/// FR-011: replacement is a full replacement. A payload that omits an optional field the
/// previous version carried leaves it cleared, not carried over.
#[test]
fn a_replacement_clears_the_fields_the_payload_omits() {
    use crate::domain::plugin::PluginBinding;

    let store = OagwStore::new();
    let owner = tenant();
    let mut original = upstream("api.example.com");
    original.protocol = crate::domain::upstream::PROTOCOLS[0].to_owned();
    original.headers.request.set = vec![crate::domain::upstream::HeaderOp {
        name: "x-signed".to_owned(),
        value: "always".to_owned(),
    }];
    original.headers.request.remove = vec!["x-caller-signature".to_owned()];
    original.plugins = vec![PluginBinding {
        name: "required_headers".to_owned(),
        uuid: None,
        config: serde_json::json!({ "required_request_headers": "x-signed" })
            .as_object()
            .cloned()
            .expect("a JSON object"),
    }];
    original.tags = vec!["payments".to_owned()];
    let created = store
        .insert_upstream(original, owner)
        .expect("the upstream is created");

    // The replacement names only the endpoint pool: everything else is omitted.
    let replacement = upstream("api.example.com");
    let replaced = store
        .replace_upstream(&created.id, replacement, &TenantChain::single(owner))
        .expect("replaced");

    assert!(replaced.headers.request.set.is_empty(), "the header set is gone");
    assert!(replaced.headers.request.remove.is_empty(), "the header removal is gone");
    assert!(
        replaced.plugins.is_empty(),
        "the plugin binding is gone: {:?}",
        replaced.plugins
    );
    assert!(replaced.tags.is_empty(), "the tags are gone");
}

#[test]
fn an_alias_collision_inside_one_tenant_is_rejected() {
    let store = OagwStore::new();
    let owner = tenant();
    store
        .insert_upstream(upstream("api.example.com"), owner)
        .expect("created");
    let err = store
        .insert_upstream(upstream("api.example.com"), owner)
        .expect_err("the alias is already taken");
    assert_eq!(err.kind(), crate::error::ErrorKind::AlreadyExists);
}

#[test]
fn deletion_removes_the_upstream_and_its_routes() {
    let store = OagwStore::new();
    let owner = tenant();
    let created = store
        .insert_upstream(upstream("api.example.com"), owner)
        .expect("created");
    store
        .delete_upstream(&created.id, &TenantChain::single(owner))
        .expect("deleted");
    assert!(
        store
            .find_upstream_by_alias("api.example.com", &TenantChain::single(owner))
            .is_none()
    );
}

#[test]
fn an_alias_is_matched_case_insensitively() {
    let store = OagwStore::new();
    let owner = tenant();
    store
        .insert_upstream(upstream("api.example.com"), owner)
        .expect("created");
    assert!(store
        .find_upstream_by_alias("API.example.COM", &TenantChain::single(owner))
        .is_some());
}

#[test]
fn a_disabled_upstream_still_resolves_so_the_relay_can_refuse_it() {
    let mut entity = upstream("api.example.com");
    entity.enabled = false;
    let store = OagwStore::new();
    let owner = tenant();
    store
        .insert_upstream(entity, owner)
        .expect("created");
    // Resolution succeeds; the relay is what refuses it with `LinkUnavailable`.
    assert!(store
        .find_upstream_by_alias("api.example.com", &TenantChain::single(owner))
        .is_some());
}

/// FR-010: an ancestor-disabled upstream stays disabled for descendants, because a
/// management write on a row the caller does not own is refused.
#[test]
fn a_descendant_cannot_re_enable_an_upstream_the_ancestor_disabled() {
    let store = OagwStore::new();
    let parent = tenant();
    let child = tenant();
    let created = store
        .insert_upstream(upstream("api.example.com"), parent)
        .expect("created");

    let mut disabled = upstream("api.example.com");
    disabled.enabled = false;
    store
        .replace_upstream(&created.id, disabled, &TenantChain::single(parent))
        .expect("the ancestor disables its own upstream");

    let chain = TenantChain::new(vec![child, parent]);
    let dto = UpstreamDto {
        id: String::new(),
        alias: Some("api.example.com".to_owned()),
        enabled: true,
        server: Server {
            endpoints: vec![endpoint("api.example.com", 443, "https")],
        },
        protocol: "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1".to_owned(),
        auth: None,
        headers: crate::domain::upstream::HeadersConfig::default(),
        rate_limit: None,
        cors: None,
        plugins: Vec::new(),
        tags: Vec::new(),
    };
    let replacement = dto.into_entity(created.id.clone(), child);
    assert!(
        store.replace_upstream(&created.id, replacement, &chain).is_err(),
        "the descendant's re-enable is refused"
    );
    assert!(
        !store
            .get_upstream(&created.id, &chain)
            .expect("still visible to the descendant")
            .enabled,
        "the ancestor's disabling stands"
    );
}
