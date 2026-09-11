//! Tests for the route DTO and the store's route surface.

use uuid::Uuid;

use crate::domain::route::{HttpMatch, PathSuffixMode, Route, RouteMatch};
use crate::domain::upstream::Upstream;
use crate::error::ErrorKind;
use crate::store::{OagwStore, TenantChain};

fn route(upstream_id: &str) -> Route {
    Route {
        upstream_id: upstream_id.to_owned(),
        r#match: Some(RouteMatch::Http(HttpMatch {
            methods: vec!["GET".to_owned()],
            path: "/v1/items".to_owned(),
            query_allowlist: Vec::new(),
            path_suffix_mode: PathSuffixMode::Append,
        })),
        priority: 10,
        enabled: true,
        ..Route::default()
    }
}

fn upstream(host: &str) -> Upstream {
    Upstream {
        enabled: true,
        server: crate::domain::upstream::Server {
            endpoints: vec![crate::domain::upstream::Endpoint {
                scheme: "https".to_owned(),
                host: host.to_owned(),
                port: 443,
            }],
        },
        ..Upstream::default()
    }
}

fn setup() -> (OagwStore, Uuid, String) {
    let store = OagwStore::new();
    let owner = Uuid::new_v4();
    let upstream = store
        .insert_upstream(upstream("api.example.com"), owner)
        .expect("the upstream is created");
    (store, owner, upstream.id)
}

#[test]
fn a_created_route_gets_an_identifier_and_its_upstream() {
    let (store, owner, upstream_id) = setup();
    let created = store
        .insert_route(route(&upstream_id), &TenantChain::single(owner))
        .expect("the route is created");
    assert!(!created.id.is_empty());
    assert_eq!(created.upstream_id, upstream_id);
    assert_eq!(created.tenant_id, owner);
}

#[test]
fn a_route_binds_to_an_upstream_outside_the_chain() {
    let (store, owner, _) = setup();
    let foreign = Uuid::new_v4();
    let err = store
        .insert_route(route("gts.cf.core.oagw.upstream.v1~missing"), &TenantChain::single(owner))
        .expect_err("the upstream does not exist");
    let _ = foreign;
    assert_eq!(err.kind(), ErrorKind::ValidationError, "{err}");
}

#[test]
fn routes_are_listed_for_their_upstream_only() {
    let (store, owner, upstream_id) = setup();
    let other = store
        .insert_upstream(upstream("other.example.com"), owner)
        .expect("created");
    store
        .insert_route(route(&upstream_id), &TenantChain::single(owner))
        .expect("created");
    store
        .insert_route(route(&other.id), &TenantChain::single(owner))
        .expect("created");

    let chain = TenantChain::single(owner);
    assert_eq!(store.routes_for_upstream(&upstream_id, &chain).len(), 1);
    assert_eq!(store.list_routes(&chain).len(), 2);
    assert_eq!(
        store.routes_for_upstream("gts.cf.core.oagw.upstream.v1~none", &chain)
            .len(),
        0
    );
}

#[test]
fn a_replacement_cannot_move_a_route_to_another_upstream() {
    let (store, owner, upstream_id) = setup();
    let created = store
        .insert_route(route(&upstream_id), &TenantChain::single(owner))
        .expect("created");
    let other = store
        .insert_upstream(upstream("other.example.com"), owner)
        .expect("created");

    let mut replacement = route(&other.id);
    replacement.enabled = false;
    let replaced = store
        .replace_route(&created.id, replacement, &TenantChain::single(owner))
        .expect("replaced");
    assert_eq!(replaced.upstream_id, upstream_id, "the upstream is immutable");
    assert!(!replaced.enabled);
}

#[test]
fn a_deleted_route_is_no_longer_listed() {
    let (store, owner, upstream_id) = setup();
    let created = store
        .insert_route(route(&upstream_id), &TenantChain::single(owner))
        .expect("created");
    store
        .delete_route(&created.id, &TenantChain::single(owner))
        .expect("deleted");
    assert_eq!(
        store.routes_for_upstream(&upstream_id, &TenantChain::single(owner))
            .len(),
        0
    );
}

#[test]
fn a_route_is_invisible_to_a_foreign_tenant() {
    let (store, owner, upstream_id) = setup();
    let created = store
        .insert_route(route(&upstream_id), &TenantChain::single(owner))
        .expect("created");
    let foreign = TenantChain::single(Uuid::new_v4());
    assert!(store.get_route(&created.id, &foreign).is_none());
    assert!(store.list_routes(&foreign).is_empty());
}

#[test]
fn an_ancestor_sees_a_descendants_route_but_not_the_reverse() {
    let (store, parent, upstream_id) = setup();
    let child = Uuid::new_v4();
    let created = store
        .insert_route(route(&upstream_id), &TenantChain::new(vec![parent]))
        .expect("created");

    let from_descendant = TenantChain::new(vec![child, parent]);
    assert_eq!(store.list_routes(&from_descendant).len(), 1);

    // A route owned by the child is invisible to the parent's chain.
    let child_owned = store
        .insert_route(route(&upstream_id), &TenantChain::new(vec![child, parent]))
        .expect("created");
    let parent_chain = TenantChain::single(parent);
    assert!(
        store.get_route(&child_owned.id, &parent_chain).is_none(),
        "an ancestor does not see a descendant's route"
    );
    assert!(
        store.get_route(&created.id, &parent_chain).is_some(),
        "the parent's own route is still visible"
    );
}

#[test]
fn a_route_validates_its_match_rule_through_the_store() {
    let (store, owner, upstream_id) = setup();
    let mut invalid = route(&upstream_id);
    invalid.r#match = Some(RouteMatch::Http(HttpMatch {
        methods: vec!["BREW".to_owned()],
        path: "/v1/coffee".to_owned(),
        query_allowlist: Vec::new(),
        path_suffix_mode: PathSuffixMode::Append,
    }));
    let err = store
        .insert_route(invalid, &TenantChain::single(owner))
        .expect_err("BREW is not an HTTP method the gateway answers");
    assert_eq!(err.kind(), ErrorKind::ValidationError, "{err}");
}

#[test]
fn a_route_needs_a_live_upstream() {
    let (store, owner, upstream_id) = setup();
    store
        .delete_upstream(&upstream_id, &TenantChain::single(owner))
        .expect("deleted");
    let err = store
        .insert_route(route(&upstream_id), &TenantChain::single(owner))
        .expect_err("the upstream is gone");
    assert_eq!(err.kind(), ErrorKind::ValidationError, "{err}");
}

/// FR-012: a second enabled route matching the same `(path, method, priority)` under the
/// same upstream is refused with a conflict, because two rows would claim one call.
#[test]
fn a_second_enabled_route_on_the_same_key_is_refused() {
    let (store, owner, upstream_id) = setup();
    let chain = TenantChain::single(owner);
    store
        .insert_route(route(&upstream_id), &chain)
        .expect("the first route is created");
    let err = store
        .insert_route(route(&upstream_id), &chain)
        .expect_err("the key is already claimed");
    assert_eq!(err.kind(), ErrorKind::AlreadyExists, "{err}");

    // A different method on the same path is a different route, so it is accepted.
    let mut other_method = route(&upstream_id);
    other_method.r#match = Some(RouteMatch::Http(HttpMatch {
        methods: vec!["POST".to_owned()],
        path: "/v1/items".to_owned(),
        query_allowlist: Vec::new(),
        path_suffix_mode: PathSuffixMode::Append,
    }));
    store
        .insert_route(other_method, &chain)
        .expect("the method is part of the key");

    // So is a different priority.
    let mut other_priority = route(&upstream_id);
    other_priority.priority = 11;
    store
        .insert_route(other_priority, &chain)
        .expect("the priority is part of the key");
}

/// A disabled route matches nothing, so it holds no key: disabling the first route frees
/// the slot for a twin, and a twin created disabled is accepted outright.
#[test]
fn a_disabled_route_holds_no_conflict_key() {
    let (store, owner, upstream_id) = setup();
    let chain = TenantChain::single(owner);
    let first = store
        .insert_route(route(&upstream_id), &chain)
        .expect("created");

    let mut disabled_twin = route(&upstream_id);
    disabled_twin.enabled = false;
    store
        .insert_route(disabled_twin, &chain)
        .expect("a disabled route claims nothing");

    // Disabling the first frees the key, so a new enabled twin is accepted.
    let mut off = route(&upstream_id);
    off.enabled = false;
    let replaced = store
        .replace_route(&first.id, off, &chain)
        .expect("the route is disabled");
    assert!(!replaced.enabled);

    let mut enabled_twin = route(&upstream_id);
    enabled_twin.enabled = true;
    store
        .insert_route(enabled_twin, &chain)
        .expect("the disabled first route is no longer in the way");
}

/// A replacement that keeps the route's own key does not conflict with itself, and one
/// that moves the key does not leave the old key reserved.
#[test]
fn a_replacement_never_conflicts_with_itself() {
    let (store, owner, upstream_id) = setup();
    let chain = TenantChain::single(owner);
    let created = store
        .insert_route(route(&upstream_id), &chain)
        .expect("created");

    // The same key again: the only conflicting row is the route being replaced.
    store
        .replace_route(&created.id, route(&upstream_id), &chain)
        .expect("the route may keep its own key");

    // Moving the path frees the old one.
    let mut moved = route(&upstream_id);
    moved.r#match = Some(RouteMatch::Http(HttpMatch {
        methods: vec!["GET".to_owned()],
        path: "/v2/items".to_owned(),
        query_allowlist: Vec::new(),
        path_suffix_mode: PathSuffixMode::Append,
    }));
    store
        .replace_route(&created.id, moved, &chain)
        .expect("the route moved");
    let mut twin = route(&upstream_id);
    twin.r#match = Some(RouteMatch::Http(HttpMatch {
        methods: vec!["GET".to_owned()],
        path: "/v1/items".to_owned(),
        query_allowlist: Vec::new(),
        path_suffix_mode: PathSuffixMode::Append,
    }));
    store
        .insert_route(twin, &chain)
        .expect("the vacated path is free again");
}
