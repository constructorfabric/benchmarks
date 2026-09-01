//! Tests for [`crate::infra::storage`].

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use axum::response::IntoResponse;
use uuid::Uuid;

use crate::domain::error::{ERROR_SOURCE_HEADER, OagwError};
use crate::domain::model::{
    AuthConfig, Plugin, PluginBinding, Protocol, ResolvedProxyTarget, Route, RouteMatch,
    ServerConfig, SharingMode, Upstream, format_plugin_id, format_upstream_id,
};
use crate::infra::storage::{
    CacheLimits, RegistryStore, dp_cache_key, plugin_cache_key, route_cache_key, upstream_cache_key,
};

const PARENT_TENANT: Uuid = Uuid::from_u128(0x10);
const CHILD_TENANT: Uuid = Uuid::from_u128(0x20);
const UNRELATED_TENANT: Uuid = Uuid::from_u128(0x30);

fn limits() -> CacheLimits {
    CacheLimits {
        upstream: 8,
        route: 8,
        plugin: 8,
        dp: 8,
    }
}

fn upstream(tenant: Uuid, id: u64, alias: &str) -> Upstream {
    Upstream {
        id: Uuid::from_u128(u128::from(id)),
        enabled: true,
        alias: alias.to_owned(),
        tags: Vec::new(),
        server: ServerConfig {
            endpoints: Vec::new(),
        },
        protocol: Protocol::Http,
        auth: None,
        headers: crate::domain::model::HeadersConfig::default(),
        plugins: crate::domain::model::PluginConfig::default(),
        rate_limit: None,
        cors: None,
        tenant_id: tenant,
        created_at: SystemTime::UNIX_EPOCH + Duration::from_secs(id),
        updated_at: SystemTime::UNIX_EPOCH + Duration::from_secs(id),
    }
}

fn http_match(path: &str) -> RouteMatch {
    http_match_with(&["GET"], path)
}

fn http_method(method: &str) -> crate::domain::model::HttpMethod {
    match method {
        "POST" => crate::domain::model::HttpMethod::Post,
        "PUT" => crate::domain::model::HttpMethod::Put,
        "DELETE" => crate::domain::model::HttpMethod::Delete,
        "PATCH" => crate::domain::model::HttpMethod::Patch,
        _ => crate::domain::model::HttpMethod::Get,
    }
}

fn http_match_with(methods: &[&str], path: &str) -> RouteMatch {
    RouteMatch {
        http: Some(crate::domain::model::HttpMatch {
            methods: methods.iter().map(|method| http_method(method)).collect(),
            path: path.to_owned(),
            query_allowlist: Vec::new(),
            path_suffix_mode: crate::domain::model::PathSuffixMode::Append,
        }),
        grpc: None,
    }
}

fn route(tenant: Uuid, id: u64, upstream_id: Uuid) -> Route {
    route_with_match(tenant, id, upstream_id, RouteMatch::default(), 0)
}

fn route_with_match(
    tenant: Uuid,
    id: u64,
    upstream_id: Uuid,
    r#match: RouteMatch,
    priority: i32,
) -> Route {
    Route {
        id: Uuid::from_u128(u128::from(id)),
        upstream_id,
        r#match,
        priority,
        headers: crate::domain::model::HeadersConfig::default(),
        plugins: crate::domain::model::PluginConfig::default(),
        rate_limit: None,
        cors: None,
        enabled: true,
        tags: Vec::new(),
        tenant_id: tenant,
        created_at: SystemTime::UNIX_EPOCH,
        updated_at: SystemTime::UNIX_EPOCH,
    }
}

fn store() -> RegistryStore {
    RegistryStore::new(limits())
}

#[test]
fn upstream_crud_is_tenant_scoped() {
    let registry = store();
    let inserted = registry
        .insert_upstream(upstream(PARENT_TENANT, 0xA1, "payments"))
        .expect("insert");
    assert_eq!(inserted.alias, "payments");

    assert!(registry.get_upstream(PARENT_TENANT, inserted.id).is_some());
    assert!(registry.get_upstream(CHILD_TENANT, inserted.id).is_none());
    assert!(
        registry
            .get_upstream(UNRELATED_TENANT, inserted.id)
            .is_none()
    );

    registry
        .replace_upstream(upstream(PARENT_TENANT, 0xA1, "payments-v2"))
        .expect("replace");
    let reloaded = registry
        .get_upstream(PARENT_TENANT, inserted.id)
        .expect("still there");
    assert_eq!(reloaded.alias, "payments-v2");

    assert!(registry.delete_upstream(PARENT_TENANT, inserted.id));
    assert!(!registry.delete_upstream(PARENT_TENANT, inserted.id));
    assert!(registry.get_upstream(PARENT_TENANT, inserted.id).is_none());
}

#[test]
fn duplicate_alias_conflicts_within_a_tenant_but_not_across_tenants() {
    let registry = store();
    registry
        .insert_upstream(upstream(PARENT_TENANT, 0xA1, "payments"))
        .expect("insert");

    let clash = registry.insert_upstream(upstream(PARENT_TENANT, 0xA2, "payments"));
    assert!(matches!(clash, Err(OagwError::Conflict(..))), "{clash:?}");

    let same_id_new_alias = registry.replace_upstream(upstream(PARENT_TENANT, 0xA1, "renamed"));
    assert!(same_id_new_alias.is_ok(), "{same_id_new_alias:?}");
    assert!(
        registry
            .resolve_upstream_alias(&[PARENT_TENANT], "payments")
            .is_none()
    );
    assert!(
        registry
            .resolve_upstream_alias(&[PARENT_TENANT], "renamed")
            .is_some()
    );

    let other_tenant = registry.insert_upstream(upstream(CHILD_TENANT, 0xB1, "payments"));
    assert!(other_tenant.is_ok(), "{other_tenant:?}");
}

#[test]
fn alias_resolution_walks_the_ancestor_chain_nearest_first() {
    let registry = store();
    registry
        .insert_upstream(upstream(PARENT_TENANT, 0xA1, "shared"))
        .expect("parent");
    registry
        .insert_upstream(upstream(CHILD_TENANT, 0xB1, "shared"))
        .expect("child");

    let child_chain = [CHILD_TENANT, PARENT_TENANT];
    let resolved = registry
        .resolve_upstream_alias(&child_chain, "shared")
        .expect("child wins");
    assert_eq!(resolved.tenant_id, CHILD_TENANT);

    let parent_chain = [PARENT_TENANT];
    let parent_only = registry
        .resolve_upstream_alias(&parent_chain, "shared")
        .expect("parent wins");
    assert_eq!(parent_only.tenant_id, PARENT_TENANT);

    assert!(
        registry
            .resolve_upstream_alias(&[UNRELATED_TENANT], "shared")
            .is_none()
    );
    assert!(registry.upstream_alias_exists(&child_chain, "shared"));
    assert!(!registry.upstream_alias_exists(&[UNRELATED_TENANT], "shared"));
}

#[test]
fn list_upstreams_is_ordered_newest_first_and_tenant_scoped() {
    let registry = store();
    for (id, tenant) in [
        (0xA1, PARENT_TENANT),
        (0xA2, PARENT_TENANT),
        (0xB1, CHILD_TENANT),
    ] {
        registry
            .insert_upstream(upstream(tenant, id, &format!("u{id:02x}")))
            .expect("insert");
    }
    let visible = registry.list_upstreams(&[PARENT_TENANT]);
    assert_eq!(visible.len(), 2);
    assert_eq!(visible[0].id, Uuid::from_u128(0xA2));
    assert_eq!(visible[1].id, Uuid::from_u128(0xA1));

    let all = registry.list_upstreams(&[PARENT_TENANT, CHILD_TENANT]);
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].id, Uuid::from_u128(0xB1));
}

#[test]
fn routes_require_a_tenant_local_upstream_and_unique_match_rules() {
    let registry = store();
    let upstream_id = Uuid::from_u128(0xA1);
    registry
        .insert_upstream(upstream(PARENT_TENANT, 0xA1, "payments"))
        .expect("upstream");

    let orphan = registry.insert_route(route(CHILD_TENANT, 0xB1, upstream_id));
    assert!(matches!(orphan, Err(OagwError::NotFound(..))), "{orphan:?}");

    registry
        .insert_route(route(PARENT_TENANT, 0xB1, upstream_id))
        .expect("insert");
    let duplicate = registry.insert_route(route(PARENT_TENANT, 0xB2, upstream_id));
    assert!(
        matches!(duplicate, Err(OagwError::Conflict(..))),
        "{duplicate:?}"
    );

    assert!(
        registry
            .replace_route(route(PARENT_TENANT, 0xB1, upstream_id))
            .is_ok()
    );
    assert!(matches!(
        registry.replace_route(route(CHILD_TENANT, 0xB1, upstream_id)),
        Err(OagwError::NotFound(..))
    ));
    assert!(registry.delete_route(PARENT_TENANT, Uuid::from_u128(0xB1)));
    assert!(!registry.delete_route(PARENT_TENANT, Uuid::from_u128(0xB1)));
}

#[test]
fn overlapping_method_sets_at_the_same_path_and_priority_conflict() {
    let registry = store();
    let upstream_id = Uuid::from_u128(0xA1);
    registry
        .insert_upstream(upstream(PARENT_TENANT, 0xA1, "payments"))
        .expect("upstream");

    // GET vs `GET|POST` on one path and priority: the sets overlap, so the
    // second rule would steal the same requests (DESIGN section 3.3).
    let existing = http_match_with(&["GET"], "/v1/pay");
    registry
        .insert_route(route_with_match(
            PARENT_TENANT,
            0xB1,
            upstream_id,
            existing,
            0,
        ))
        .expect("insert");

    let overlapping = http_match_with(&["POST", "GET"], "/v1/pay");
    let clash = registry.insert_route(route_with_match(
        PARENT_TENANT,
        0xB2,
        upstream_id,
        overlapping,
        0,
    ));
    assert!(matches!(clash, Err(OagwError::Conflict(..))), "{clash:?}");

    // The mirrored order conflicts too: the wider set first, the narrower one
    // second (a route never conflicts with its own replacement).
    assert!(registry.delete_route(PARENT_TENANT, Uuid::from_u128(0xB1)));
    registry
        .insert_route(route_with_match(
            PARENT_TENANT,
            0xB2,
            upstream_id,
            http_match_with(&["POST", "GET"], "/v1/pay"),
            0,
        ))
        .expect("the wider set is inserted first");
    let mirrored = registry.insert_route(route_with_match(
        PARENT_TENANT,
        0xB1,
        upstream_id,
        http_match_with(&["GET"], "/v1/pay"),
        0,
    ));
    assert!(
        matches!(mirrored, Err(OagwError::Conflict(..))),
        "{mirrored:?}"
    );
    let self_replace = registry.replace_route(route_with_match(
        PARENT_TENANT,
        0xB2,
        upstream_id,
        http_match_with(&["POST"], "/v1/pay"),
        0,
    ));
    assert!(
        self_replace.is_ok(),
        "a route may replace itself: {self_replace:?}"
    );

    // A different priority at the same path is a distinct rule.
    registry
        .insert_route(route_with_match(
            PARENT_TENANT,
            0xB3,
            upstream_id,
            http_match_with(&["GET"], "/v1/pay"),
            5,
        ))
        .expect("a different priority is a different rule");

    // A disjoint method set at the same path and priority is fine.
    registry
        .insert_route(route_with_match(
            PARENT_TENANT,
            0xB4,
            upstream_id,
            http_match_with(&["DELETE"], "/v1/pay"),
            0,
        ))
        .expect("disjoint methods do not conflict");

    // gRPC rules still compare on `(service, method)`.
    let grpc = RouteMatch {
        http: None,
        grpc: Some(crate::domain::model::GrpcMatch {
            service: "payments.v1.Pay".to_owned(),
            method: "Charge".to_owned(),
        }),
    };
    registry
        .insert_route(route_with_match(
            PARENT_TENANT,
            0xB5,
            upstream_id,
            grpc.clone(),
            0,
        ))
        .expect("insert");
    let clash = registry.insert_route(route_with_match(PARENT_TENANT, 0xB6, upstream_id, grpc, 0));
    assert!(matches!(clash, Err(OagwError::Conflict(..))), "{clash:?}");
}

#[tokio::test]
async fn concurrent_duplicate_alias_creates_yield_exactly_one_winner() {
    use std::sync::Arc as StdArc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let registry = StdArc::new(store());
    let created = Arc::new(AtomicUsize::new(0));
    let conflicted = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();
    for index in 0..32u64 {
        let registry = Arc::clone(&registry);
        let created = Arc::clone(&created);
        let conflicted = Arc::clone(&conflicted);
        handles.push(tokio::task::spawn_blocking(move || {
            let mut candidate = upstream(PARENT_TENANT, 0xA0 + index, "payments");
            candidate.id = Uuid::new_v4();
            match registry.insert_upstream(candidate) {
                Ok(_) => created.fetch_add(1, Ordering::SeqCst),
                Err(OagwError::Conflict(_)) => conflicted.fetch_add(1, Ordering::SeqCst),
                Err(other) => panic!("unexpected error: {other:?}"),
            }
        }));
    }
    for handle in handles {
        handle.await.expect("task");
    }

    assert_eq!(created.load(Ordering::SeqCst), 1, "exactly one 201");
    assert_eq!(
        conflicted.load(Ordering::SeqCst),
        31,
        "every other create is a 409"
    );
    assert_eq!(registry.list_upstreams(&[PARENT_TENANT]).len(), 1);
}

#[test]
fn deleting_an_upstream_removes_its_routes() {
    let registry = store();
    let upstream_id = Uuid::from_u128(0xA1);
    registry
        .insert_upstream(upstream(PARENT_TENANT, 0xA1, "payments"))
        .expect("upstream");
    registry
        .insert_route(route(PARENT_TENANT, 0xB1, upstream_id))
        .expect("route");

    assert_eq!(
        registry.route_ids_for_upstream(PARENT_TENANT, upstream_id),
        vec![Uuid::from_u128(0xB1)]
    );
    assert!(registry.delete_upstream(PARENT_TENANT, upstream_id));
    assert!(
        registry
            .route_ids_for_upstream(PARENT_TENANT, upstream_id)
            .is_empty()
    );
    assert!(
        registry
            .get_route(PARENT_TENANT, Uuid::from_u128(0xB1))
            .is_none()
    );
}

#[test]
fn list_routes_is_ordered_highest_priority_first() {
    let registry = store();
    let upstream_id = Uuid::from_u128(0xA1);
    registry
        .insert_upstream(upstream(PARENT_TENANT, 0xA1, "payments"))
        .expect("upstream");
    let high = route_with_match(PARENT_TENANT, 0xB1, upstream_id, http_match("/high"), 10);
    let mid = route_with_match(PARENT_TENANT, 0xB2, upstream_id, http_match("/mid"), 5);
    registry.insert_route(mid).expect("insert");
    registry.insert_route(high).expect("insert");

    let visible = registry.list_routes(&[PARENT_TENANT]);
    assert_eq!(visible.len(), 2);
    assert_eq!(visible[0].id, Uuid::from_u128(0xB1));
    assert_eq!(visible[1].id, Uuid::from_u128(0xB2));
}

#[test]
fn plugin_reference_lookups_cover_auth_and_both_chain_levels() {
    let registry = store();
    let plugin = Plugin {
        id: Uuid::from_u128(0xC1),
        tenant_id: PARENT_TENANT,
        plugin_type: "gts.cf.core.oagw.guard_plugin.v1".to_owned(),
        config: serde_json::json!({}),
        ..Plugin::default()
    };
    // The GTS instance-id spelling of a custom plugin, resolved through
    // `parse_plugin_id`, next to the bare-UUID spelling.
    let gts_reference = format_plugin_id(plugin.id);
    let bare_reference = plugin.id.to_string();

    let mut with_auth = upstream(PARENT_TENANT, 0xA1, "authed");
    with_auth.auth = Some(AuthConfig {
        auth_type: gts_reference.clone(),
        sharing: SharingMode::Inherit,
        config: serde_json::json!({}),
    });
    registry.insert_upstream(with_auth).expect("insert");

    let mut with_chain = upstream(PARENT_TENANT, 0xA2, "chained");
    with_chain.plugins.items = vec![PluginBinding::new(bare_reference, serde_json::json!({}))];
    registry.insert_upstream(with_chain).expect("insert");

    let clean = registry
        .insert_upstream(upstream(PARENT_TENANT, 0xA3, "clean"))
        .expect("insert");

    let referencing = registry.upstream_ids_referencing_plugin(&plugin);
    assert_eq!(referencing.len(), 2);
    assert!(referencing.contains(&Uuid::from_u128(0xA1)));
    assert!(referencing.contains(&Uuid::from_u128(0xA2)));
    assert!(!referencing.contains(&clean.id));

    let mut route_with_chain = route(PARENT_TENANT, 0xB1, clean.id);
    route_with_chain.plugins.items = vec![PluginBinding::new(gts_reference, serde_json::json!({}))];
    registry.insert_route(route_with_chain).expect("insert");
    assert_eq!(
        registry.route_ids_referencing_plugin(&plugin),
        vec![Uuid::from_u128(0xB1)]
    );

    // The tenant-scoped variants narrow the result to the named tenants and
    // still resolve both spellings.
    let own = registry
        .upstream_ids_referencing_plugin_in(&plugin, &[PARENT_TENANT])
        .len();
    assert_eq!(own, 2);
    assert!(
        registry
            .upstream_ids_referencing_plugin_in(&plugin, &[CHILD_TENANT])
            .is_empty()
    );
    assert!(
        registry
            .route_ids_referencing_plugin_in(&plugin, &[CHILD_TENANT])
            .is_empty()
    );
}

#[test]
fn plugins_round_trip_through_the_registry_and_cache() {
    let registry = store();
    let plugin_id = Uuid::from_u128(0xC1);
    let plugin = Plugin {
        id: plugin_id,
        tenant_id: PARENT_TENANT,
        plugin_type: "gts.cf.core.oagw.auth_plugin.v1".to_owned(),
        config: serde_json::json!({ "kind": "apikey" }),
        ..Plugin::default()
    };
    registry.insert_plugin(plugin).expect("inserted");

    assert!(registry.get_plugin(PARENT_TENANT, plugin_id).is_some());
    assert!(registry.get_plugin(CHILD_TENANT, plugin_id).is_none());
    assert_eq!(
        registry.list_plugins(&[PARENT_TENANT])[0].plugin_type,
        "gts.cf.core.oagw.auth_plugin.v1"
    );
    assert!(registry.lookup_plugin_cache(plugin_id).is_some());

    let removed = registry
        .delete_plugin(PARENT_TENANT, plugin_id)
        .expect("removed");
    assert_eq!(removed.config, serde_json::json!({ "kind": "apikey" }));
    assert!(registry.get_plugin(PARENT_TENANT, plugin_id).is_none());
    assert!(registry.lookup_plugin_cache(plugin_id).is_none());
}

#[test]
fn cache_keys_follow_the_adr_0005_spellings() {
    let tenant = PARENT_TENANT;
    let upstream_id = Uuid::from_u128(0xA1);
    assert_eq!(
        upstream_cache_key(tenant, "payments"),
        format!("upstream:{tenant}:payments")
    );
    assert_eq!(
        route_cache_key(upstream_id, "GET", "/v1"),
        format!("route:{upstream_id}:GET:/v1")
    );
    assert_eq!(
        plugin_cache_key(upstream_id),
        format!("plugin:{upstream_id}")
    );
    assert_eq!(
        dp_cache_key(tenant, "payments", "POST", "/v1/pay"),
        format!("dp:{tenant}:payments:POST:/v1/pay")
    );
}

#[test]
fn cache_hits_survive_until_an_authoritative_write_flushes_them() {
    let registry = store();
    let inserted = registry
        .insert_upstream(upstream(PARENT_TENANT, 0xA1, "payments"))
        .expect("insert");

    registry.store_upstream_cache(Arc::clone(&inserted));
    assert_eq!(registry.upstream_cache_len(), 1);
    let hit = registry
        .lookup_upstream_cache(PARENT_TENANT, "payments")
        .expect("hit");
    assert_eq!(hit.id, inserted.id);

    registry
        .replace_upstream(upstream(PARENT_TENANT, 0xA1, "payments"))
        .expect("replace");
    assert_eq!(registry.upstream_cache_len(), 0, "mutation must invalidate");
    assert!(
        registry
            .lookup_upstream_cache(PARENT_TENANT, "payments")
            .is_none()
    );
}

#[test]
fn route_and_dp_caches_flush_prefixes_not_everything() {
    let registry = store();
    let upstream_a = Uuid::from_u128(0xA1);
    let upstream_b = Uuid::from_u128(0xA2);
    registry
        .insert_upstream(upstream(PARENT_TENANT, 0xA1, "a"))
        .expect("upstream a");
    registry
        .insert_upstream(upstream(PARENT_TENANT, 0xA2, "b"))
        .expect("upstream b");
    let route_a = Arc::new(route_with_match(
        PARENT_TENANT,
        0xB1,
        upstream_a,
        http_match("/a"),
        0,
    ));
    let route_b = Arc::new(route_with_match(
        PARENT_TENANT,
        0xB2,
        upstream_b,
        http_match("/b"),
        0,
    ));
    registry
        .insert_route(route_with_match(
            PARENT_TENANT,
            0xB1,
            upstream_a,
            http_match("/a"),
            0,
        ))
        .expect("insert");
    registry
        .insert_route(route_with_match(
            PARENT_TENANT,
            0xB2,
            upstream_b,
            http_match("/b"),
            0,
        ))
        .expect("insert");

    registry.store_route_cache(upstream_a, "GET", "/v1", Arc::clone(&route_a));
    registry.store_route_cache(upstream_b, "GET", "/v1", Arc::clone(&route_b));
    assert_eq!(registry.route_cache_len(), 2);

    assert!(registry.delete_route(PARENT_TENANT, route_a.id));
    assert_eq!(
        registry.route_cache_len(),
        1,
        "only upstream A entries are flushed"
    );
    assert!(
        registry
            .lookup_route_cache(upstream_a, "GET", "/v1")
            .is_none()
    );
    assert!(
        registry
            .lookup_route_cache(upstream_b, "GET", "/v1")
            .is_some()
    );

    let target = ResolvedProxyTarget {
        upstream: Arc::new(upstream(PARENT_TENANT, 0xA1, "payments")),
        route: None,
    };
    registry.store_dp_cache(PARENT_TENANT, "payments", "GET", "/v1", Arc::new(target));
    assert_eq!(registry.dp_cache_len(), 1);
    assert!(
        registry
            .lookup_dp_cache(PARENT_TENANT, "payments", "GET", "/v1")
            .is_some()
    );
    registry.flush_all_caches();
    assert!(registry.caches_are_empty());
}

#[test]
fn caches_flush_on_full_instead_of_growing_unbounded() {
    let registry = RegistryStore::new(CacheLimits {
        upstream: 2,
        route: 2,
        plugin: 2,
        dp: 2,
    });
    for index in 0..5u64 {
        let inserted = registry
            .insert_upstream(upstream(
                PARENT_TENANT,
                0x1000 + index,
                &format!("alias-{index}"),
            ))
            .expect("insert");
        registry.store_upstream_cache(Arc::clone(&inserted));
    }
    assert!(
        registry.upstream_cache_len() <= 2,
        "cache must stay bounded"
    );

    for index in 0..5u64 {
        registry.store_plugin_cache(
            Uuid::from_u128(u128::from(0x2000 + index)),
            Arc::new(serde_json::json!({ "index": index })),
        );
    }
    assert!(registry.plugin_cache_len() <= 2);
}

#[test]
fn stored_errors_carry_the_gateway_source_header() {
    let error = OagwError::conflict("duplicate alias");
    let rendered = error.into_response();
    assert!(rendered.headers().contains_key(ERROR_SOURCE_HEADER));
}

#[test]
fn upstream_ids_format_with_the_gts_prefix() {
    let id = Uuid::from_u128(0xA1);
    assert!(format_upstream_id(id).starts_with("gts.cf.core.oagw.upstream.v1~"));
}

// -- H6: the route insert and the upstream delete share one critical section.

/// A delete cannot enter the critical section an in-flight insert holds: it
/// has to wait, so it can never land between the insert's ownership check and
/// its write.
#[test]
fn an_upstream_delete_waits_for_the_route_critical_section() {
    let registry = Arc::new(store());
    let upstream_id = Uuid::from_u128(0xA1);
    registry
        .insert_upstream(upstream(PARENT_TENANT, 0xA1, "payments"))
        .expect("upstream");

    // Hold the section exactly as `put_route` does while it checks and writes.
    let guard = registry.route_write.lock();
    let deleter = {
        let registry = Arc::clone(&registry);
        std::thread::spawn(move || registry.delete_upstream(PARENT_TENANT, upstream_id))
    };
    std::thread::sleep(Duration::from_millis(20));
    // Deterministic either way: an unstarted delete has not removed anything,
    // and a started one is parked on `route_write`.
    assert!(
        registry.get_upstream(PARENT_TENANT, upstream_id).is_some(),
        "the delete must not run inside another writer's critical section"
    );
    drop(guard);
    assert!(deleter.join().expect("delete thread"), "delete completes");
    assert!(
        registry.list_routes(&[PARENT_TENANT]).is_empty(),
        "the delete takes its routes with it"
    );
}

/// The interleaving the shared lock rules out: the delete lands before the
/// insert, and the insert is refused instead of resurrecting a route whose
/// upstream is gone.
#[test]
fn an_insert_after_an_upstream_delete_is_refused_without_leaving_a_route() {
    let registry = store();
    let upstream_id = Uuid::from_u128(0xA1);
    registry
        .insert_upstream(upstream(PARENT_TENANT, 0xA1, "payments"))
        .expect("upstream");
    assert!(registry.delete_upstream(PARENT_TENANT, upstream_id));

    let attempt = registry.insert_route(route(PARENT_TENANT, 0xB1, upstream_id));
    assert!(
        matches!(attempt, Err(OagwError::NotFound(..))),
        "{attempt:?}"
    );
    assert!(
        registry
            .get_route(PARENT_TENANT, Uuid::from_u128(0xB1))
            .is_none()
    );
    assert!(
        registry.list_routes(&[PARENT_TENANT]).is_empty(),
        "no dangling route survives its upstream"
    );
}

/// Under concurrent creates and a concurrent delete, no route that names a
/// gone upstream ever becomes visible: the shared critical section is the
/// whole point.
#[test]
fn concurrent_route_creates_and_an_upstream_delete_leave_no_dangling_route() {
    let registry = Arc::new(store());
    let upstream_id = Uuid::from_u128(0xA1);
    registry
        .insert_upstream(upstream(PARENT_TENANT, 0xA1, "payments"))
        .expect("upstream");

    let writers: Vec<_> = (0..8u64)
        .map(|index| {
            let registry = Arc::clone(&registry);
            std::thread::spawn(move || {
                for offset in 0..8u64 {
                    let id = 0xB00 + index * 16 + offset;
                    let r#match = http_match_with(&["GET"], &format!("/v1/pay/{id}"));
                    // Either outcome is legitimate: the create lands, or the
                    // upstream is already gone and the create is refused.
                    let _ = registry.insert_route(route_with_match(
                        PARENT_TENANT,
                        id,
                        upstream_id,
                        r#match,
                        0,
                    ));
                }
            })
        })
        .collect();
    let deleter = {
        let registry = Arc::clone(&registry);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(5));
            registry.delete_upstream(PARENT_TENANT, upstream_id)
        })
    };
    for writer in writers {
        writer.join().expect("writer thread");
    }
    assert!(deleter.join().expect("delete thread"));

    for survivor in registry.list_routes(&[PARENT_TENANT]) {
        assert!(
            registry
                .get_upstream(PARENT_TENANT, survivor.upstream_id)
                .is_some(),
            "route {} outlived its upstream",
            survivor.id
        );
    }
}
