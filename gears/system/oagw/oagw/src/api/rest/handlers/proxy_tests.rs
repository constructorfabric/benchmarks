//! Unit tests of the data-plane helpers of [`crate::api::rest::handlers::proxy`].

use std::sync::Arc;
use std::time::SystemTime;

use uuid::Uuid;

use super::reconcile_dp_snapshot;
use crate::domain::model::{
    HeadersConfig, PluginConfig, Protocol, ResolvedProxyTarget, Route, RouteMatch, Upstream,
};
use crate::infra::storage::{CacheLimits, RegistryStore};

const TENANT: Uuid = Uuid::from_u128(0x10);
const UPSTREAM_ID: Uuid = Uuid::from_u128(0xA1);
const ROUTE_ID: Uuid = Uuid::from_u128(0xB1);
const OTHER_UPSTREAM_ID: Uuid = Uuid::from_u128(0xA2);

fn store() -> RegistryStore {
    RegistryStore::new(CacheLimits {
        upstream: 8,
        route: 8,
        plugin: 8,
        dp: 8,
    })
}

fn upstream(id: Uuid, alias: &str) -> Upstream {
    Upstream {
        id,
        enabled: true,
        alias: alias.to_owned(),
        tags: Vec::new(),
        server: crate::domain::model::ServerConfig {
            endpoints: Vec::new(),
        },
        protocol: Protocol::Http,
        auth: None,
        headers: HeadersConfig::default(),
        plugins: PluginConfig::default(),
        rate_limit: None,
        cors: None,
        tenant_id: TENANT,
        created_at: SystemTime::UNIX_EPOCH,
        updated_at: SystemTime::UNIX_EPOCH,
    }
}

fn route(upstream_id: Uuid) -> Route {
    Route {
        id: ROUTE_ID,
        upstream_id,
        r#match: RouteMatch::default(),
        headers: HeadersConfig::default(),
        plugins: PluginConfig::default(),
        rate_limit: None,
        cors: None,
        enabled: true,
        priority: 0,
        tags: Vec::new(),
        tenant_id: TENANT,
        created_at: SystemTime::UNIX_EPOCH,
        updated_at: SystemTime::UNIX_EPOCH,
    }
}

fn snapshot(upstream: Arc<Upstream>, route: Arc<Route>) -> Arc<ResolvedProxyTarget> {
    Arc::new(ResolvedProxyTarget {
        upstream,
        route: Some(route),
    })
}

/// Inserts the upstream and its route and returns the snapshot a resolution
/// would have cached, holding the very `Arc`s the registry handed out.
fn seeded(alias: &str) -> (RegistryStore, Arc<ResolvedProxyTarget>) {
    let store = store();
    let inserted = store
        .insert_upstream(upstream(UPSTREAM_ID, alias))
        .expect("inserts");
    let inserted_route = store.insert_route(route(inserted.id)).expect("inserts");
    let target = snapshot(Arc::clone(&inserted), Arc::clone(&inserted_route));
    (store, target)
}

#[test]
fn an_unchanged_snapshot_is_served_as_is() {
    let (store, cached) = seeded("orders");
    // The registry still holds the very `Arc`s the resolution produced, so the
    // reconcile hands the cached snapshot back untouched instead of rebuilding
    // it (the fast path every request that is not racing a mutation takes).
    let reconciled = reconcile_dp_snapshot(&store, &cached).expect("the snapshot survives");
    assert!(
        Arc::ptr_eq(&reconciled, &cached),
        "an unchanged snapshot must not be rebuilt"
    );
    assert_eq!(reconciled.upstream.alias, "orders");
}

#[test]
fn a_replaced_upstream_is_re_read_from_the_registry() {
    let (store, cached) = seeded("orders");
    // A concurrent mutation replaced the upstream after this request resolved:
    // the registry now holds a different `Arc` for the same id.
    let mut replacement = cached.upstream.as_ref().clone();
    replacement.tags = vec!["replaced".to_owned()];
    let fresh_upstream = store.replace_upstream(replacement).expect("replaces");
    assert!(!Arc::ptr_eq(&fresh_upstream, &cached.upstream));

    let reconciled = reconcile_dp_snapshot(&store, &cached).expect("the route still belongs");
    assert!(
        Arc::ptr_eq(&reconciled.upstream, &fresh_upstream),
        "the fresh upstream is served, not the cached one"
    );
    assert_eq!(reconciled.upstream.tags, vec!["replaced".to_owned()]);
    // The route is re-read too: it still belongs to this upstream.
    assert_eq!(reconciled.route.as_ref().expect("route").id, ROUTE_ID);
}

#[test]
fn a_deleted_upstream_makes_the_snapshot_stale() {
    let (store, cached) = seeded("orders");
    assert!(
        store.delete_upstream(TENANT, cached.upstream.id),
        "the upstream is deleted"
    );
    assert!(
        reconcile_dp_snapshot(&store, &cached).is_none(),
        "a flushed-away upstream is never served from the cache"
    );
}

#[test]
fn a_route_that_moved_away_makes_the_snapshot_stale() {
    let (store, cached) = seeded("orders");
    // A second upstream appears and the route is re-pointed at it, so the
    // cached route no longer belongs to the cached upstream.
    let other = store
        .insert_upstream(upstream(OTHER_UPSTREAM_ID, "billing"))
        .expect("inserts");
    let mut moved = cached.route.as_ref().expect("route").as_ref().clone();
    moved.upstream_id = other.id;
    store.replace_route(moved).expect("replaces");

    assert!(
        reconcile_dp_snapshot(&store, &cached).is_none(),
        "a route that no longer belongs to the upstream is not served"
    );
}

#[test]
fn a_disabled_route_makes_the_snapshot_stale() {
    let (store, cached) = seeded("orders");
    let mut disabled = cached.route.as_ref().expect("route").as_ref().clone();
    disabled.enabled = false;
    store.replace_route(disabled).expect("replaces");
    assert!(reconcile_dp_snapshot(&store, &cached).is_none());
}

#[test]
fn a_snapshot_without_a_route_is_reconciled_against_the_upstream_alone() {
    // A preflight resolution binds the alias to an upstream and nothing else,
    // so the upstream is all it has to re-validate.
    let (store, cached) = seeded("orders");
    let routeless = Arc::new(ResolvedProxyTarget {
        upstream: Arc::clone(&cached.upstream),
        route: None,
    });
    assert!(reconcile_dp_snapshot(&store, &routeless).is_some());

    let mut replacement = cached.upstream.as_ref().clone();
    replacement.tags = vec!["replaced".to_owned()];
    let fresh = store.replace_upstream(replacement).expect("replaces");
    let reconciled = reconcile_dp_snapshot(&store, &routeless).expect("reconciles");
    assert!(reconciled.route.is_none(), "still a route-less snapshot");
    assert!(Arc::ptr_eq(&reconciled.upstream, &fresh));

    // But the upstream disappearing leaves nothing to serve.
    assert!(store.delete_upstream(TENANT, fresh.id));
    assert!(reconcile_dp_snapshot(&store, &routeless).is_none());
}

#[test]
fn a_disabled_upstream_is_never_reconciled_back_into_service() {
    // `lookup_dp_cache` drops a snapshot whose upstream is disabled before it
    // reaches the reconcile, so a disabled upstream cannot be resurrected here;
    // the reconcile itself only ever sees an enabled one.
    let (store, cached) = seeded("orders");
    let mut disabled = cached.upstream.as_ref().clone();
    disabled.enabled = false;
    store.replace_upstream(disabled).expect("replaces");
    assert!(
        reconcile_dp_snapshot(&store, &cached).is_none(),
        "the route no longer belongs to an enabled upstream, so the snapshot is stale"
    );
}
