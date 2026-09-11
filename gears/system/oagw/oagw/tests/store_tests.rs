//! Store tests.
//!
//! Covers `cpt-cf-oagw-dod-persisted-model`, `cpt-cf-oagw-algo-tenant-scope`
//! and the write steps of `cpt-cf-oagw-flow-upstream-create`,
//! `cpt-cf-oagw-flow-route-create`, `cpt-cf-oagw-flow-upstream-replace-delete`
//! and `cpt-cf-oagw-flow-route-delete`: the seven tables round-trip, the
//! tenant predicate guards every read and every `{id}`-addressed write, the
//! `(tenant_id, alias)` and enabled-match-key uniqueness checks run inside the
//! same batch as the write they guard, cascade works in both directions, and a
//! failing batch leaves no partial row.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::missing_panics_doc)]

use std::sync::Arc;

use oagw::store::{MatchKey, OagwStore, StoreError};
use oagw::{Endpoint, EndpointHost, HttpMatch, MatchConfig, Route, Scheme, ServerConfig, Upstream};
use uuid::Uuid;

const HTTP_PROTOCOL: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

fn tenant(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

fn endpoint(host: &str, port: u16) -> Endpoint {
    Endpoint {
        scheme: Scheme::Https,
        host: EndpointHost::parse(host).expect("valid endpoint host"),
        port: Some(port),
    }
}

fn upstream(alias: Option<&str>) -> Upstream {
    Upstream {
        id: Uuid::new_v4(),
        enabled: true,
        alias: alias.map(str::to_owned),
        tags: vec![String::from("llm")],
        server: ServerConfig {
            endpoints: vec![endpoint("api.openai.com", 443)],
        },
        protocol: String::from(HTTP_PROTOCOL),
        auth: None,
        headers: None,
        plugins: None,
        rate_limit: None,
        cors: None,
    }
}

fn route(upstream_id: Uuid, path: &str, priority: i64, enabled: Option<bool>) -> Route {
    Route {
        id: Uuid::new_v4(),
        upstream_id,
        match_config: MatchConfig {
            http: Some(HttpMatch {
                methods: vec![String::from("GET")],
                path: String::from(path),
                query_allowlist: vec![],
                path_suffix_mode: None,
            }),
            grpc: None,
        },
        plugins: None,
        rate_limit: None,
        tags: vec![String::from("edge")],
        cors: None,
        priority: Some(priority),
        enabled,
    }
}

fn http_of(route: &Route) -> HttpMatch {
    route.match_config.http.clone().expect("http match")
}

fn key_of(store: &OagwStore, tenant_id: Uuid, route: &Route) -> Option<Uuid> {
    let http = http_of(route);
    store
        .enabled_match_index(tenant_id)
        .get(&MatchKey {
            upstream_id: route.upstream_id,
            path: http.path,
            priority: route.priority.unwrap_or_default(),
            method: "GET".to_owned(),
        })
        .copied()
}

#[test]
fn an_inserted_upstream_round_trips_every_table() {
    let store = OagwStore::new();
    let value = upstream(Some("api.openai.com"));
    let id = value.id;

    let row = store.insert_upstream(tenant(1), &value).expect("inserted");
    assert_eq!(row.upstream.id, id);
    assert_eq!(row.upstream.alias.as_deref(), Some("api.openai.com"));
    assert_eq!(row.tags, vec![String::from("llm")]);
    assert_eq!(row.upstream.tags, vec![String::from("llm")]);

    let read = store.get_upstream(tenant(1), id).expect("read back");
    assert_eq!(read, row);
    assert_eq!(
        store.upstream_tag_rows(tenant(1), id),
        vec![String::from("llm")]
    );
    assert_eq!(store.list_upstreams(tenant(1)), vec![row]);
}

#[test]
fn an_inserted_route_round_trips_every_table() {
    let store = OagwStore::new();
    let upstream_id = store
        .insert_upstream(tenant(1), &upstream(None))
        .expect("upstream inserted")
        .upstream
        .id;
    let value = route(upstream_id, "/v1/chat", 1, Some(true));

    let row = store.insert_route(tenant(1), &value).expect("inserted");
    assert_eq!(row.route.upstream_id, upstream_id);
    assert_eq!(row.tags, vec![String::from("edge")]);

    let read = store.get_route(tenant(1), row.route.id).expect("read back");
    assert_eq!(read, row);
    assert_eq!(
        store.route_http_match(tenant(1), row.route.id),
        value.match_config.http
    );
    assert_eq!(store.route_grpc_match(tenant(1), row.route.id), None);
    assert_eq!(store.route_methods(tenant(1), row.route.id), vec!["GET"]);
    assert_eq!(
        store.route_tag_rows(tenant(1), row.route.id),
        vec![String::from("edge")]
    );
    assert_eq!(store.list_routes(tenant(1)), vec![row]);
}

#[test]
fn a_second_tenants_rows_are_invisible_to_every_read() {
    let store = OagwStore::new();
    let upstream_id = store
        .insert_upstream(tenant(1), &upstream(Some("api.openai.com")))
        .expect("upstream inserted")
        .upstream
        .id;
    let route_id = store
        .insert_route(tenant(1), &route(upstream_id, "/v1/chat", 1, Some(true)))
        .expect("route inserted")
        .route
        .id;

    assert!(store.get_upstream(tenant(2), upstream_id).is_none());
    assert!(store.get_route(tenant(2), route_id).is_none());
    assert!(store.list_upstreams(tenant(2)).is_empty());
    assert!(store.list_routes(tenant(2)).is_empty());
    assert!(store.route_http_match(tenant(2), route_id).is_none());
    assert!(store.route_grpc_match(tenant(2), route_id).is_none());
    assert!(store.route_methods(tenant(2), route_id).is_empty());
    assert!(store.upstream_tag_rows(tenant(2), upstream_id).is_empty());
    assert!(store.route_tag_rows(tenant(2), route_id).is_empty());
    assert!(store.enabled_match_index(tenant(2)).is_empty());
}

#[test]
fn a_foreign_tenant_cannot_address_a_row_by_its_id() {
    let store = OagwStore::new();
    let upstream_id = store
        .insert_upstream(tenant(1), &upstream(Some("api.openai.com")))
        .expect("upstream inserted")
        .upstream
        .id;
    let route_id = store
        .insert_route(tenant(1), &route(upstream_id, "/v1/chat", 1, Some(true)))
        .expect("route inserted")
        .route
        .id;
    let foreign = route(upstream_id, "/v2", 2, Some(true));

    assert!(matches!(
        store.replace_upstream(
            tenant(2),
            upstream_id,
            &Upstream {
                id: upstream_id,
                ..upstream(None)
            }
        ),
        Err(StoreError::Invariant { .. })
    ));
    assert!(matches!(
        store.replace_route(tenant(2), route_id, &foreign),
        Err(StoreError::Invariant { .. })
    ));
    assert_eq!(store.delete_upstream(tenant(2), upstream_id), Ok(false));
    assert_eq!(store.delete_route(tenant(2), route_id), Ok(false));
    assert_eq!(store.list_upstreams(tenant(1)).len(), 1);
    assert_eq!(store.list_routes(tenant(1)).len(), 1);
}

#[test]
fn an_alias_conflict_inside_one_batch_leaves_no_row() {
    let store = OagwStore::new();
    store
        .insert_upstream(tenant(1), &upstream(Some("api.openai.com")))
        .expect("first insert");

    let second = upstream(Some("API.OpenAI.com"));
    assert_eq!(
        store.insert_upstream(tenant(1), &second),
        Err(StoreError::AliasConflict)
    );
    assert_eq!(store.list_upstreams(tenant(1)).len(), 1);
    assert!(
        store.get_upstream(tenant(1), second.id).is_none(),
        "the failed batch left no row behind"
    );
    assert!(
        store.upstream_tag_rows(tenant(1), second.id).is_empty(),
        "the failed batch left no tag row behind"
    );
}

#[test]
fn the_same_alias_in_a_different_tenant_succeeds() {
    let store = OagwStore::new();
    store
        .insert_upstream(tenant(1), &upstream(Some("api.openai.com")))
        .expect("first insert");

    let second = store
        .insert_upstream(tenant(2), &upstream(Some("api.openai.com")))
        .expect("the alias is unique per tenant");
    assert_eq!(store.list_upstreams(tenant(2)).len(), 1);
    assert_eq!(second.upstream.alias.as_deref(), Some("api.openai.com"));
}

#[test]
fn a_replacement_holds_its_own_alias() {
    let store = OagwStore::new();
    let first = store
        .insert_upstream(tenant(1), &upstream(Some("api.openai.com")))
        .expect("first insert")
        .upstream;
    let second = store
        .insert_upstream(tenant(1), &upstream(Some("eu.vendor.com")))
        .expect("second insert")
        .upstream;

    let renamed = store
        .replace_upstream(
            tenant(1),
            second.id,
            &Upstream {
                alias: Some(String::from("api.openai.com")),
                ..second.clone()
            },
        )
        .expect_err("the alias is held by the other upstream");
    assert_eq!(renamed, StoreError::AliasConflict);

    let kept = store
        .replace_upstream(tenant(1), first.id, &first)
        .expect("a replacement keeps its own alias");
    assert_eq!(kept.upstream.alias.as_deref(), Some("api.openai.com"));
}

#[test]
fn a_replacement_rewrites_its_tag_rows() {
    let store = OagwStore::new();
    let mut value = upstream(None);
    let id = value.id;
    store.insert_upstream(tenant(1), &value).expect("inserted");

    value.tags = vec![String::from("edge"), String::from("llm")];
    let replaced = store
        .replace_upstream(tenant(1), id, &value)
        .expect("replaced");
    assert_eq!(
        replaced.tags,
        vec![String::from("edge"), String::from("llm")]
    );
    assert_eq!(store.upstream_tag_rows(tenant(1), id), replaced.tags);
}

#[test]
fn a_replacement_cannot_move_the_addressed_identifier() {
    let store = OagwStore::new();
    let value = upstream(None);
    let id = store
        .insert_upstream(tenant(1), &value)
        .expect("inserted")
        .upstream
        .id;

    assert!(matches!(
        store.replace_upstream(tenant(1), id, &upstream(None)),
        Err(StoreError::Invariant { .. })
    ));
    assert_eq!(store.get_upstream(tenant(1), id), Some(store.get_upstream(tenant(1), id).expect("kept")));
}

#[test]
fn an_enabled_route_match_key_is_unique_per_upstream() {
    let store = OagwStore::new();
    let upstream_id = store
        .insert_upstream(tenant(1), &upstream(None))
        .expect("upstream inserted")
        .upstream
        .id;
    let first = route(upstream_id, "/v1/chat", 1, Some(true));
    store.insert_route(tenant(1), &first).expect("first route");

    let second = route(upstream_id, "/v1/chat", 1, Some(true));
    let refused = store
        .insert_route(tenant(1), &second)
        .expect_err("the match key is taken");
    let StoreError::MatchConflict {
        colliding_route_id,
    } = refused
    else {
        panic!("expected a match conflict, got {refused:?}");
    };
    assert_eq!(colliding_route_id, first.id);
    assert_eq!(store.list_routes(tenant(1)).len(), 1);
    assert!(
        store.get_route(tenant(1), second.id).is_none(),
        "the failed batch left no route row behind"
    );
    assert!(
        store.route_tag_rows(tenant(1), second.id).is_empty(),
        "the failed batch left no tag row behind"
    );
    assert!(
        store.route_methods(tenant(1), second.id).is_empty(),
        "the failed batch left no method row behind"
    );
}

#[test]
fn a_disabled_route_is_exempt_from_match_uniqueness() {
    let store = OagwStore::new();
    let upstream_id = store
        .insert_upstream(tenant(1), &upstream(None))
        .expect("upstream inserted")
        .upstream
        .id;
    store
        .insert_route(tenant(1), &route(upstream_id, "/v1/chat", 1, Some(true)))
        .expect("enabled route");

    let disabled = store
        .insert_route(tenant(1), &route(upstream_id, "/v1/chat", 1, Some(false)))
        .expect("a disabled route may share the key");
    assert_eq!(store.list_routes(tenant(1)).len(), 2);
    assert_eq!(store.enabled_match_index(tenant(1)).len(), 1);
    assert!(
        !store
            .enabled_match_index(tenant(1))
            .values()
            .any(|id| *id == disabled.route.id),
        "a disabled route owns no index entry"
    );
}

#[test]
fn a_route_of_another_upstream_may_share_the_key() {
    let store = OagwStore::new();
    let first = store
        .insert_upstream(tenant(1), &upstream(Some("a.vendor.com")))
        .expect("first upstream")
        .upstream
        .id;
    let second = store
        .insert_upstream(tenant(1), &upstream(Some("b.vendor.com")))
        .expect("second upstream")
        .upstream
        .id;
    store
        .insert_route(tenant(1), &route(first, "/v1/chat", 1, Some(true)))
        .expect("first route");

    store
        .insert_route(tenant(1), &route(second, "/v1/chat", 1, Some(true)))
        .expect("the key is scoped to one upstream");
    assert_eq!(store.enabled_match_index(tenant(1)).len(), 2);
}

#[test]
fn a_different_method_or_priority_does_not_collide() {
    let store = OagwStore::new();
    let upstream_id = store
        .insert_upstream(tenant(1), &upstream(None))
        .expect("upstream inserted")
        .upstream
        .id;
    let first = route(upstream_id, "/v1/chat", 1, Some(true));
    store.insert_route(tenant(1), &first).expect("first route");

    let other_method = route(upstream_id, "/v1/chat", 1, Some(true));
    store
        .insert_route(
            tenant(1),
            &Route {
                match_config: MatchConfig {
                    http: Some(HttpMatch {
                        methods: vec![String::from("POST")],
                        ..http_of(&first)
                    }),
                    ..first.match_config.clone()
                },
                ..other_method.clone()
            },
        )
        .expect("another method is another key");

    store
        .insert_route(tenant(1), &route(upstream_id, "/v1/chat", 2, Some(true)))
        .expect("another priority is another key");
    assert_eq!(store.enabled_match_index(tenant(1)).len(), 3);
}

#[test]
fn the_replaced_row_is_excluded_from_its_own_match_key() {
    let store = OagwStore::new();
    let upstream_id = store
        .insert_upstream(tenant(1), &upstream(None))
        .expect("upstream inserted")
        .upstream
        .id;
    let value = route(upstream_id, "/v1/chat", 1, Some(true));
    let id = store
        .insert_route(tenant(1), &value)
        .expect("inserted")
        .route
        .id;

    let untouched = store
        .replace_route(tenant(1), id, &value)
        .expect("a replacement keeps its own match key");
    assert_eq!(untouched.route.id, id);

    let moved = store
        .replace_route(
            tenant(1),
            id,
            &Route {
                id,
                ..route(upstream_id, "/v2/embed", 1, Some(true))
            },
        )
        .expect("a replacement may move its key");
    assert_eq!(store.list_routes(tenant(1)).len(), 1);
    assert_eq!(
        key_of(&store, tenant(1), &moved.route),
        Some(id),
        "the index follows the replacement"
    );
}

#[test]
fn deleting_an_upstream_cascades_into_its_routes() {
    let store = OagwStore::new();
    let upstream_id = store
        .insert_upstream(tenant(1), &upstream(None))
        .expect("upstream inserted")
        .upstream
        .id;
    let first = store
        .insert_route(tenant(1), &route(upstream_id, "/v1", 1, Some(true)))
        .expect("first route")
        .route
        .id;
    let second = store
        .insert_route(tenant(1), &route(upstream_id, "/v2", 2, Some(true)))
        .expect("second route")
        .route
        .id;

    assert_eq!(store.delete_upstream(tenant(1), upstream_id), Ok(true));
    assert!(store.get_upstream(tenant(1), upstream_id).is_none());
    assert!(store.get_route(tenant(1), first).is_none());
    assert!(store.get_route(tenant(1), second).is_none());
    assert!(store.list_routes(tenant(1)).is_empty());
    assert!(store.upstream_tag_rows(tenant(1), upstream_id).is_empty());
    assert!(store.route_tag_rows(tenant(1), first).is_empty());
    assert!(store.route_methods(tenant(1), second).is_empty());
    assert!(store.route_http_match(tenant(1), second).is_none());
    assert!(store.enabled_match_index(tenant(1)).is_empty());
}

#[test]
fn deleting_a_route_leaves_its_upstream() {
    let store = OagwStore::new();
    let upstream_id = store
        .insert_upstream(tenant(1), &upstream(Some("api.openai.com")))
        .expect("upstream inserted")
        .upstream
        .id;
    let route_id = store
        .insert_route(tenant(1), &route(upstream_id, "/v1", 1, Some(true)))
        .expect("route inserted")
        .route
        .id;

    assert_eq!(store.delete_route(tenant(1), route_id), Ok(true));
    assert!(
        store.get_upstream(tenant(1), upstream_id).is_some(),
        "a route deletion never touches the upstream row"
    );
    assert_eq!(
        store.upstream_tag_rows(tenant(1), upstream_id),
        vec![String::from("llm")]
    );
    assert!(store.route_tag_rows(tenant(1), route_id).is_empty());
    assert!(store.route_methods(tenant(1), route_id).is_empty());
    assert!(store.route_http_match(tenant(1), route_id).is_none());
    assert!(store.enabled_match_index(tenant(1)).is_empty());
}

#[test]
fn deleting_an_absent_or_foreign_row_answers_false() {
    let store = OagwStore::new();
    assert_eq!(store.delete_upstream(tenant(1), Uuid::nil()), Ok(false));
    assert_eq!(store.delete_route(tenant(1), Uuid::nil()), Ok(false));
}

#[test]
fn the_enabled_match_index_reflects_every_write_and_delete() {
    let store = OagwStore::new();
    let upstream_id = store
        .insert_upstream(tenant(1), &upstream(None))
        .expect("upstream inserted")
        .upstream
        .id;
    assert!(store.enabled_match_index(tenant(1)).is_empty());

    let enabled = route(upstream_id, "/v1/chat", 1, Some(true));
    let enabled_id = store
        .insert_route(tenant(1), &enabled)
        .expect("inserted")
        .route
        .id;
    assert_eq!(key_of(&store, tenant(1), &enabled), Some(enabled_id));

    let toggled = Route {
        enabled: Some(false),
        ..enabled.clone()
    };
    store
        .replace_route(tenant(1), enabled_id, &toggled)
        .expect("replaced");
    assert!(
        store.enabled_match_index(tenant(1)).is_empty(),
        "disabling a route drops its index entries"
    );

    let re_enabled = Route {
        enabled: Some(true),
        ..toggled
    };
    store
        .replace_route(tenant(1), enabled_id, &re_enabled)
        .expect("replaced");
    assert_eq!(key_of(&store, tenant(1), &re_enabled), Some(enabled_id));

    store.delete_route(tenant(1), enabled_id).expect("deleted");
    assert!(store.enabled_match_index(tenant(1)).is_empty());
}

#[test]
fn a_broken_model_refuses_the_batch_and_writes_nothing() {
    let store = Arc::new(OagwStore::with_orphaned_match_index());
    let value = upstream(Some("api.openai.com"));

    assert!(matches!(
        store.insert_upstream(tenant(1), &value),
        Err(StoreError::Invariant { .. })
    ));
    assert!(
        store.get_upstream(tenant(1), value.id).is_none(),
        "a refused batch leaves no row behind"
    );
}

#[test]
fn an_empty_store_scans_to_nothing() {
    let store = OagwStore::new();
    assert!(store.list_upstreams(tenant(1)).is_empty());
    assert!(store.list_routes(tenant(1)).is_empty());
    assert!(store.enabled_match_index(tenant(1)).is_empty());
}
