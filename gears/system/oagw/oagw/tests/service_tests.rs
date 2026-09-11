//! Management service tests.
//!
//! Covers the six flows of the FEATURE §2 end to end against the store: the
//! 201 shape of a create, the derived and idempotent alias, the foreign
//! `upstream_id` and the `MatchConflict` refusal of a route create, the 404
//! that never distinguishes a foreign identifier from a missing one, the
//! bounded list, the alias immutability of a replacement, the omitted
//! `upstream_id` that conforms, the cascading upstream deletion, the route
//! deletion that leaves its upstream untouched, the enable/disable carry
//! forward, and the cache and deletion-seam ordering.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::missing_panics_doc)]

use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::{Value, json};
use uuid::Uuid;

use oagw::control_plane::cache::ControlPlaneCache;
use oagw::control_plane::cache::RateLimitCleanup;
use oagw::control_plane::service::ManagementService;
use oagw::control_plane::service::ServiceError;
use oagw::config::OagwConfig;
use oagw::domain::error::ErrorKind;
use oagw::store::{OagwStore, UpstreamRow};

const HTTP_PROTOCOL: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

fn tenant(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

/// A management service over its own empty store and cache.
fn service() -> (ManagementService, Arc<ControlPlaneCache>) {
    let cache = Arc::new(ControlPlaneCache::new());
    let service = ManagementService::new(
        Arc::new(OagwStore::new()),
        &OagwConfig::default(),
        Arc::clone(&cache),
    )
    .expect("the validators compile");
    (service, cache)
}

/// A minimal valid upstream body.
fn upstream_body(host: &str) -> Value {
    json!({
        "server": {
            "endpoints": [{ "scheme": "https", "host": host, "port": 443 }]
        },
        "protocol": HTTP_PROTOCOL,
        "tags": ["llm"]
    })
}

/// A route body addressing one upstream.
fn route_body(upstream_id: Uuid, path: &str, priority: i64) -> Value {
    json!({
        "upstream_id": upstream_id,
        "match": { "http": { "methods": ["GET"], "path": path } },
        "priority": priority,
        "tags": ["edge"]
    })
}

/// Removes one root property from a body.
fn without(body: &Value, key: &str) -> Value {
    let mut body = body.clone();
    body.as_object_mut()
        .expect("the body is an object")
        .remove(key);
    body
}

/// The catalogue row of a refused operation, asserting it is a domain failure.
fn domain_of(error: &ServiceError) -> &oagw::DomainError {
    assert!(!error.is_storage(), "the operation is a domain failure");
    error.domain()
}

/// A deletion-seam observer recording the notifications it receives.
#[derive(Debug, Default)]
struct Recording {
    upstreams: Mutex<Vec<(Uuid, Uuid)>>,
    routes: Mutex<Vec<(Uuid, Uuid)>>,
}

impl RateLimitCleanup for Recording {
    fn upstream_deleted(&self, tenant_id: Uuid, upstream_id: Uuid) {
        self.upstreams.lock().push((tenant_id, upstream_id));
    }

    fn route_deleted(&self, tenant_id: Uuid, route_id: Uuid) {
        self.routes.lock().push((tenant_id, route_id));
    }
}

/// An upstream of one tenant, returning the row the store wrote.
fn created_upstream(service: &ManagementService, owner: u128, host: &str) -> UpstreamRow {
    service
        .create_upstream(tenant(owner), &upstream_body(host))
        .expect("the upstream is created")
}

/// The identifier of one created upstream.
fn id_of(row: &UpstreamRow) -> Uuid {
    row.upstream.id
}

#[test]
fn a_created_upstream_carries_the_identifier_and_the_derived_alias() {
    let (service, _cache) = service();
    let row = service
        .create_upstream(tenant(1), &upstream_body("api.openai.com"))
        .expect("the upstream is created");

    assert_ne!(row.upstream.id, Uuid::nil(), "the store assigned one");
    assert_eq!(row.tenant_id, tenant(1));
    assert_eq!(row.upstream.alias.as_deref(), Some("api.openai.com"));
    assert_eq!(row.tags, vec![String::from("llm")], "the tags materialize");
    assert!(row.upstream.enabled, "a created row starts enabled");
}

#[test]
fn an_alias_matching_the_derivation_is_an_idempotent_no_op() {
    let (service, _cache) = service();
    let mut body = upstream_body("api.openai.com");
    body["alias"] = json!("api.openai.com");
    let row = service
        .create_upstream(tenant(1), &body)
        .expect("the explicit alias matches the derived one");
    assert_eq!(row.upstream.alias.as_deref(), Some("api.openai.com"));
}

#[test]
fn a_second_upstream_holding_the_derived_alias_is_refused() {
    let (service, _cache) = service();
    service
        .create_upstream(tenant(1), &upstream_body("api.openai.com"))
        .expect("the first upstream");

    let error = service
        .create_upstream(tenant(1), &upstream_body("api.openai.com"))
        .expect_err("the alias is already held");
    let refused = domain_of(&error);
    assert_eq!(refused.kind, ErrorKind::AliasConflict);
    assert_eq!(refused.http_status(), 409);

    let other = service
        .create_upstream(tenant(1), &upstream_body("eu.openai.com"))
        .expect("a different endpoint set derives a different alias");
    assert_ne!(other.upstream.alias, None);
}

#[test]
fn a_created_route_resolves_its_upstream_and_its_match_key() {
    let (service, _cache) = service();
    let upstream = created_upstream(&service, 1, "api.openai.com");
    let upstream_id = id_of(&upstream);

    let row = service
        .create_route(tenant(1), &route_body(upstream_id, "/v1/chat", 10))
        .expect("the route is created");
    assert_eq!(row.route.upstream_id, upstream_id);
    assert_eq!(row.tenant_id, tenant(1));
    assert_eq!(row.tags, vec![String::from("edge")]);
}

#[test]
fn a_route_naming_a_foreign_upstream_is_refused() {
    let (service, _cache) = service();
    let upstream = created_upstream(&service, 1, "api.openai.com");
    let upstream_id = id_of(&upstream);

    let error = service
        .create_route(tenant(2), &route_body(upstream_id, "/v1/chat", 10))
        .expect_err("another tenant owns the upstream");
    let refused = domain_of(&error);
    assert_eq!(refused.kind, ErrorKind::ValidationError);
    assert_eq!(refused.http_status(), 400);
    assert!(refused.detail.contains("upstream_id"), "{refused}");

    let error = service
        .create_route(tenant(1), &route_body(Uuid::new_v4(), "/v1/chat", 10))
        .expect_err("the upstream does not exist");
    assert_eq!(domain_of(&error).http_status(), 400);
}

#[test]
fn a_second_route_holding_the_match_key_is_refused() {
    let (service, _cache) = service();
    let upstream = created_upstream(&service, 1, "api.openai.com");
    let upstream_id = id_of(&upstream);
    service
        .create_route(tenant(1), &route_body(upstream_id, "/v1/chat", 10))
        .expect("the first route");

    let error = service
        .create_route(tenant(1), &route_body(upstream_id, "/v1/chat", 10))
        .expect_err("the key is already held");
    let refused = domain_of(&error);
    assert_eq!(refused.kind, ErrorKind::MatchConflict);
    assert_eq!(refused.http_status(), 409);
    assert!(refused.detail.contains("route"), "{refused}");

    // A different priority or method, or a disabled route, never collides.
    service
        .create_route(tenant(1), &route_body(upstream_id, "/v1/chat", 11))
        .expect("another priority");
    let mut other_method = route_body(upstream_id, "/v1/chat", 10);
    other_method["match"]["http"]["methods"] = json!(["POST"]);
    service
        .create_route(tenant(1), &other_method)
        .expect("another method");
}

#[test]
fn a_single_read_of_a_foreign_identifier_is_a_bare_404() {
    let (service, _cache) = service();
    let upstream = created_upstream(&service, 1, "api.openai.com");
    let upstream_id = id_of(&upstream);

    let unknown = service
        .read_upstream(tenant(1), Uuid::new_v4())
        .expect_err("the identifier is unknown");
    let missing = domain_of(&unknown);
    let elsewhere = service
        .read_upstream(tenant(2), upstream_id)
        .expect_err("the row belongs to another tenant");
    let foreign = domain_of(&elsewhere);
    assert_eq!(missing.http_status(), 404);
    assert_eq!(foreign.http_status(), 404);
    assert_eq!(missing.detail, foreign.detail, "the causes are indistinguishable");

    let resolved = service
        .read_upstream(tenant(1), upstream_id)
        .expect("the calling tenant's row");
    assert_eq!(resolved.upstream.id, upstream_id);
}

#[test]
fn a_list_answers_the_page_the_parameters_ask_for() {
    let (service, _cache) = service();
    let first = created_upstream(&service, 1, "api.openai.com");
    created_upstream(&service, 1, "eu.openai.com");
    created_upstream(&service, 2, "foreign.openai.com");

    let page = service
        .list_upstreams(tenant(1), "")
        .expect("the defaults are admitted");
    assert_eq!(page.items.len(), 2, "the foreign upstream stays out");
    assert_eq!(page.projection, Vec::<String>::new());

    let page = service
        .list_upstreams(tenant(1), "$filter=alias%20eq%20'api.openai.com'")
        .expect("the filter is admitted");
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].upstream.id, id_of(&first));

    let error = service
        .list_upstreams(tenant(1), "$count=true")
        .expect_err("the parameter is not exposed");
    assert_eq!(domain_of(&error).http_status(), 400);

    let routes = service
        .list_routes(tenant(1), "$top=101")
        .expect("the ceiling is a bound, not a refusal");
    assert!(routes.items.is_empty());
}

#[test]
fn an_upstream_replacement_replaces_in_full_and_keeps_the_alias() {
    let (service, _cache) = service();
    let upstream = created_upstream(&service, 1, "api.openai.com");
    let upstream_id = id_of(&upstream);

    // The tags are replaced in full and the alias is recomputed to the stored
    // value.
    let mut replacement = upstream_body("api.openai.com");
    replacement["tags"] = json!(["edge"]);
    let written = service
        .replace_upstream(tenant(1), upstream_id, &replacement)
        .expect("the replacement applies");
    assert_eq!(written.tags, vec![String::from("edge")]);
    assert_eq!(written.upstream.alias.as_deref(), Some("api.openai.com"));
    assert_eq!(written.upstream.id, upstream_id, "the identifier is immutable");

    // Clearing the optional families the body omits.
    let mut cleared = without(&upstream_body("api.openai.com"), "tags");
    cleared["protocol"] = json!(HTTP_PROTOCOL);
    let written = service
        .replace_upstream(tenant(1), upstream_id, &cleared)
        .expect("the replacement applies");
    assert!(written.tags.is_empty(), "the omitted family is cleared");

    // A body whose endpoints derive another alias is refused, and the stored
    // alias is left unchanged.
    let error = service
        .replace_upstream(tenant(1), upstream_id, &upstream_body("eu.openai.com"))
        .expect_err("the alias is immutable");
    let refused = domain_of(&error);
    assert_eq!(refused.kind, ErrorKind::AliasConflict);
    assert_eq!(refused.http_status(), 409);
    let stored = service
        .read_upstream(tenant(1), upstream_id)
        .expect("the row survives the refusal");
    assert_eq!(stored.upstream.alias.as_deref(), Some("api.openai.com"));

    // A body stating another identifier is refused.
    let mut renamed = upstream_body("api.openai.com");
    renamed["id"] = json!(Uuid::new_v4().to_string());
    let error = service
        .replace_upstream(tenant(1), upstream_id, &renamed)
        .expect_err("the identifier is immutable");
    assert_eq!(domain_of(&error).http_status(), 400);

    // A foreign identifier is a bare 404.
    let error = service
        .replace_upstream(tenant(2), upstream_id, &upstream_body("api.openai.com"))
        .expect_err("another tenant owns the row");
    assert_eq!(domain_of(&error).http_status(), 404);
}

#[test]
fn a_route_replacement_takes_the_upstream_reference_from_the_stored_row() {
    let (service, _cache) = service();
    let upstream = created_upstream(&service, 1, "api.openai.com");
    let upstream_id = id_of(&upstream);
    let route = service
        .create_route(tenant(1), &route_body(upstream_id, "/v1/chat", 10))
        .expect("the route");

    let conformance = without(&route_body(Uuid::nil(), "/v1/chat", 10), "upstream_id");
    let written = service
        .replace_route(tenant(1), route.route.id, &conformance)
        .expect("the omitted reference conforms to the stored row");
    assert_eq!(written.route.upstream_id, upstream_id);

    // The replacement schema narrows the required set and takes the upstream
    // reference from the stored row, so the body states neither.
    let narrowed = without(&without(&route_body(upstream_id, "/v1/chat", 10), "tags"), "upstream_id");
    let written = service
        .replace_route(tenant(1), route.route.id, &narrowed)
        .expect("the required set is narrowed for a replacement");
    assert!(written.tags.is_empty(), "the omitted family is cleared");

    let error = service
        .replace_route(tenant(1), Uuid::new_v4(), &narrowed)
        .expect_err("the identifier is unknown");
    assert_eq!(domain_of(&error).http_status(), 404);
}

#[test]
fn an_enable_flag_travels_on_the_replacement_body() {
    let (service, _cache) = service();
    let upstream = created_upstream(&service, 1, "api.openai.com");
    let upstream_id = id_of(&upstream);

    // An omitted `enabled` carries the stored value forward.
    let replacement = upstream_body("api.openai.com");
    let written = service
        .replace_upstream(tenant(1), upstream_id, &replacement)
        .expect("the replacement applies");
    assert!(written.upstream.enabled, "the stored value carries forward");

    // An explicit value controls.
    let mut disabled = upstream_body("api.openai.com");
    disabled["enabled"] = json!(false);
    let written = service
        .replace_upstream(tenant(1), upstream_id, &disabled)
        .expect("the replacement applies");
    assert!(!written.upstream.enabled, "an explicit value wins");

    let stored = service
        .read_upstream(tenant(1), upstream_id)
        .expect("the row");
    assert!(!stored.upstream.enabled, "the flag persisted");

    let mut reenabled = upstream_body("api.openai.com");
    reenabled["enabled"] = json!(true);
    let written = service
        .replace_upstream(tenant(1), upstream_id, &reenabled)
        .expect("the replacement applies");
    assert!(written.upstream.enabled, "the row returns to enabled");
}

#[test]
fn an_upstream_deletion_cascades_into_its_routes() {
    let (service, cache) = service();
    let observer = Arc::new(Recording::default());
    service.register_deletion_observer(Arc::clone(&observer) as Arc<_>);

    let upstream = created_upstream(&service, 1, "api.openai.com");
    let upstream_id = id_of(&upstream);
    let route = service
        .create_route(tenant(1), &route_body(upstream_id, "/v1/chat", 10))
        .expect("the route");

    let before = cache.generation();
    let deleted = service
        .delete_upstream(tenant(1), upstream_id)
        .expect("the deletion applies");
    assert!(deleted, "the row was there");
    assert!(cache.generation() > before, "the cache advanced");

    assert!(
        service.read_upstream(tenant(1), upstream_id).is_err(),
        "the upstream row is gone"
    );
    assert!(
        service.read_route(tenant(1), route.route.id).is_err(),
        "the route row cascaded away"
    );
    assert_eq!(
        observer.upstreams.lock().len(),
        1,
        "the deletion notified the cleanup"
    );
    assert_eq!(observer.upstreams.lock()[0], (tenant(1), upstream_id));
    assert_eq!(observer.routes.lock().len(), 0, "no route deletion was issued");

    let error = service
        .delete_upstream(tenant(1), upstream_id)
        .expect_err("the row is already gone");
    assert_eq!(domain_of(&error).http_status(), 404);
}

#[test]
fn a_route_deletion_leaves_its_upstream_untouched() {
    let (service, _cache) = service();
    let observer = Arc::new(Recording::default());
    service.register_deletion_observer(Arc::clone(&observer) as Arc<_>);

    let upstream = created_upstream(&service, 1, "api.openai.com");
    let upstream_id = id_of(&upstream);
    let first = service
        .create_route(tenant(1), &route_body(upstream_id, "/v1/chat", 10))
        .expect("the first route");
    let second = service
        .create_route(tenant(1), &route_body(upstream_id, "/v1/embed", 20))
        .expect("the second route");

    let deleted = service
        .delete_route(tenant(1), first.route.id)
        .expect("the deletion applies");
    assert!(deleted);

    assert!(
        service.read_upstream(tenant(1), upstream_id).is_ok(),
        "the upstream row survives the route deletion"
    );
    assert!(
        service.read_route(tenant(1), second.route.id).is_ok(),
        "no other route is disturbed"
    );
    assert_eq!(observer.routes.lock().len(), 1);
    assert_eq!(observer.routes.lock()[0], (tenant(1), first.route.id));
    assert_eq!(observer.upstreams.lock().len(), 0);
}

#[test]
fn the_cache_generation_advances_only_on_a_successful_write() {
    let (service, cache) = service();
    assert_eq!(cache.generation(), 0, "the cache starts at zero");

    let upstream = service
        .create_upstream(tenant(1), &upstream_body("api.openai.com"))
        .expect("the upstream is created");
    let after_create = cache.generation();
    assert_eq!(after_create, 1, "one successful write, one generation");

    service
        .read_upstream(tenant(1), upstream.upstream.id)
        .expect("the read resolves");
    service.list_upstreams(tenant(1), "").expect("the list resolves");
    assert_eq!(cache.generation(), after_create, "a read never flushes");

    service
        .create_upstream(tenant(1), &upstream_body("api.openai.com"))
        .expect_err("the alias conflicts");
    service
        .create_route(tenant(1), &route_body(Uuid::new_v4(), "/v1", 1))
        .expect_err("the referenced upstream is missing");
    assert_eq!(
        cache.generation(),
        after_create,
        "a failed write never flushes"
    );

    service
        .replace_upstream(tenant(1), upstream.upstream.id, &upstream_body("api.openai.com"))
        .expect("the replacement applies");
    assert_eq!(cache.generation(), after_create + 1);

    service
        .delete_upstream(tenant(1), upstream.upstream.id)
        .expect("the deletion applies");
    assert_eq!(cache.generation(), after_create + 2);
}

#[test]
fn a_failed_deletion_notifies_nothing() {
    let (service, _cache) = service();
    let observer = Arc::new(Recording::default());
    service.register_deletion_observer(Arc::clone(&observer) as Arc<_>);

    let upstream = created_upstream(&service, 1, "api.openai.com");
    let upstream_id = id_of(&upstream);

    // A 404 deletion reaches no observer.
    let error = service
        .delete_upstream(tenant(1), Uuid::new_v4())
        .expect_err("the identifier is unknown");
    assert_eq!(domain_of(&error).http_status(), 404);
    // A foreign deletion reaches no observer either.
    let error = service
        .delete_route(tenant(2), upstream_id)
        .expect_err("the row belongs to another tenant");
    assert_eq!(domain_of(&error).http_status(), 404);

    let route = service
        .create_route(tenant(1), &route_body(upstream_id, "/v1/chat", 10))
        .expect("the route");
    let error = service
        .delete_route(tenant(2), route.route.id)
        .expect_err("another tenant owns the route");
    assert_eq!(domain_of(&error).http_status(), 404);

    assert!(observer.upstreams.lock().is_empty(), "no notification");
    assert!(observer.routes.lock().is_empty(), "no notification");
}

#[test]
fn a_persistence_failure_is_never_a_catalogue_row() {
    let cache = Arc::new(ControlPlaneCache::new());
    let service = ManagementService::new(
        Arc::new(OagwStore::with_orphaned_match_index()),
        &OagwConfig::default(),
        Arc::clone(&cache),
    )
    .expect("the validators compile");

    let error = service
        .create_upstream(tenant(1), &upstream_body("api.openai.com"))
        .expect_err("the store cannot commit the batch");
    assert!(error.is_storage(), "the failure is a persistence failure");

    assert_eq!(cache.generation(), 0, "a failed write never flushes");

    // The 404 of a deletion still precedes any storage concern.
    let error = service
        .delete_upstream(tenant(1), Uuid::new_v4())
        .expect_err("the identifier is unknown");
    assert_eq!(domain_of(&error).http_status(), 404);
}
