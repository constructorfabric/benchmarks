//! Integration tests for the five route operations over the gear-relative
//! routes (`cpt-cf-oagw-dod-route-management-route-crud`).
//!
//! Every test drives the real `OagwGear` through `Gear::init` and
//! `RestApiCapability::register_rest`, then issues HTTP requests through the
//! registered router.
// @cpt-dod:cpt-cf-oagw-dod-route-management-integration-tests:p1
// @cpt-dod:cpt-cf-oagw-dod-route-management-route-crud:p1

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use serde_json::{Value, json};
use uuid::Uuid;

use oagw::test_support::{
    FakeHierarchyTenantResolver, FakePolicyAuthZ, RecordingConfigWriteHook, management_surface,
    permissive_surface, route_body, security_context,
};

const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

fn upstream_body(host: &str) -> Value {
    json!({ "protocol": PROTOCOL_HTTP, "server": { "endpoints": [ { "host": host } ] } })
}

/// Create the parent upstream a route addresses and return its identifier.
async fn seed_upstream(
    surface: &oagw::test_support::ManagementSurface,
    tenant: Uuid,
    alias: &str,
) -> Uuid {
    let (status, bytes) = surface.create(tenant, Uuid::new_v4(), upstream_body(alias)).await;
    assert_eq!(status, 201, "{bytes:?}");
    let record: Value = serde_json::from_slice(&bytes).expect("created upstream");
    serde_json::from_value(record["id"].clone()).expect("the upstream identifier")
}

/// A created route is addressed by the identifier the create returned, carries
/// the materialized defaults, and is returned by `GET`.
#[tokio::test]
async fn a_created_route_is_addressable_by_its_identifier() {
    let tenant = Uuid::new_v4();
    let surface = permissive_surface(None).await;
    let upstream_id = seed_upstream(&surface, tenant, "api.vendor.com").await;

    let (status, bytes) = surface.create_route(tenant, Uuid::new_v4(), route_body(upstream_id, "/v1/orders")).await;
    assert_eq!(status, 201, "{bytes:?}");
    let created: Value = serde_json::from_slice(&bytes).expect("the created route");
    let id: Uuid = serde_json::from_value(created["id"].clone()).expect("the route identifier");
    assert_ne!(id, Uuid::nil(), "the identifier is server-generated");
    assert_eq!(created["upstream_id"], json!(upstream_id.to_string()));
    assert_eq!(created["priority"], 0, "the declared default priority");
    assert_eq!(created["enabled"], json!(true), "the declared default enablement");
    assert!(created["match"]["http"]["path_suffix_mode"] == json!("append"), "the match default");
    assert!(created.get("tenant_id").is_none(), "the owner tenant is never on the wire");
    assert!(created.get("match_type").is_none(), "the derived match type is never on the wire");
    assert!(created.get("rate_limit").is_none(), "no implicit override block");
    assert!(created.get("cors").is_none());
    assert!(created.get("plugins").is_none());

    let (status, bytes) = surface.get_route(tenant, Uuid::new_v4(), id).await;
    assert_eq!(status, 200, "{bytes:?}");
    let read: Value = serde_json::from_slice(&bytes).expect("the stored route");
    assert_eq!(read["id"], created["id"]);
    assert_eq!(read["match"]["http"]["path"], "/v1/orders");
}

/// The create response is addressed by the `Location` header.
#[tokio::test]
async fn the_create_response_is_addressed_by_its_location_header() {
    let tenant = Uuid::new_v4();
    let surface = permissive_surface(None).await;
    let upstream_id = seed_upstream(&surface, tenant, "api.vendor.com").await;

    let mut request = http::Request::builder()
        .method(http::Method::POST)
        .uri("/oagw/v1/routes")
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(route_body(upstream_id, "/v1/orders").to_string()))
        .expect("request");
    request.extensions_mut().insert(security_context(tenant, Uuid::new_v4()));
    let response = tower::ServiceExt::oneshot(surface.router.clone(), request)
        .await
        .expect("the router answers");
    assert_eq!(response.status(), 201);
    let location = response
        .headers()
        .get(http::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .expect("the Location header")
        .to_owned();
    assert_eq!(location, format!("/oagw/v1/routes/{id}", id = location.rsplit('/').next().unwrap_or("")));
    let body = axum::body::to_bytes(response.into_body(), 1 << 20).await.expect("body");
    let created: Value = serde_json::from_slice(&body).expect("the created route");
    let id: Uuid = serde_json::from_value(created["id"].clone()).expect("id");
    assert!(location.ends_with(&id.to_string()), "`{location}` addresses the created route");
}

/// `PUT` is the full replacement: omitted optional blocks are cleared, `id`
/// and `upstream_id` survive, and the materialized defaults apply.
#[tokio::test]
async fn a_replacement_overwrites_every_field_and_clears_the_omitted_ones() {
    let tenant = Uuid::new_v4();
    let surface = permissive_surface(None).await;
    let upstream_id = seed_upstream(&surface, tenant, "api.vendor.com").await;

    let mut body = route_body(upstream_id, "/v1/orders");
    body["priority"] = json!(7);
    body["tags"] = json!(["orders"]);
    body["rate_limit"] = json!({ "sustained": { "rate": 60 } });
    let (status, bytes) = surface.create_route(tenant, Uuid::new_v4(), body).await;
    assert_eq!(status, 201, "{bytes:?}");
    let created: Value = serde_json::from_slice(&bytes).expect("created");
    let id: Uuid = serde_json::from_value(created["id"].clone()).expect("id");

    let (status, bytes) = surface
        .replace_route(
            tenant,
            Uuid::new_v4(),
            id,
            json!({ "match": { "http": { "path": "/v1/orders", "methods": ["GET", "POST"] } } }),
        )
        .await;
    assert_eq!(status, 200, "{bytes:?}");
    let replaced: Value = serde_json::from_slice(&bytes).expect("replaced");
    assert_eq!(replaced["id"], created["id"], "the identifier survives");
    assert_eq!(replaced["upstream_id"], created["upstream_id"], "the upstream reference survives");
    assert_eq!(replaced["priority"], 0, "the omitted priority materializes the declared default");
    assert_eq!(replaced["enabled"], json!(true), "the omitted enablement re-enables");
    assert_eq!(replaced["tags"], json!([]), "the omitted tag list is cleared");
    assert!(replaced.get("rate_limit").is_none(), "the omitted override is cleared");
    assert_eq!(replaced["match"]["http"]["methods"], json!(["GET", "POST"]), "the match block is replaced");
}

/// `DELETE` answers `204 No Content` with an empty body and leaves no
/// queryable trace.
#[tokio::test]
async fn a_deleted_route_answers_204_and_leaves_no_trace() {
    let tenant = Uuid::new_v4();
    let surface = permissive_surface(None).await;
    let upstream_id = seed_upstream(&surface, tenant, "api.vendor.com").await;

    let (status, bytes) = surface.create_route(tenant, Uuid::new_v4(), route_body(upstream_id, "/v1/orders")).await;
    let created: Value = serde_json::from_slice(&bytes).expect("created");
    let id: Uuid = serde_json::from_value(created["id"].clone()).expect("id");
    assert_eq!(status, 201);

    let (status, bytes) = surface.delete_route(tenant, Uuid::new_v4(), id).await;
    assert_eq!(status, 204, "{bytes:?}");
    assert!(bytes.is_empty(), "the deletion confirmation carries no body: {bytes:?}");
    assert_eq!(surface.get_route(tenant, Uuid::new_v4(), id).await.0, 404, "no trace is left");
    let (status, bytes) = surface.list_routes(tenant, Uuid::new_v4(), "").await;
    assert_eq!(status, 200);
    let listed: Value = serde_json::from_slice(&bytes).expect("list");
    assert_eq!(listed["count"], 0, "the route is no longer queryable");
}

/// A delete of a missing, foreign or already-deleted identifier is `404`.
#[tokio::test]
async fn a_delete_of_a_missing_route_is_not_found() {
    let tenant = Uuid::new_v4();
    let surface = permissive_surface(None).await;
    for id in [Uuid::new_v4(), Uuid::nil()] {
        let (status, bytes) = surface.delete_route(tenant, Uuid::new_v4(), id).await;
        assert_eq!(status, 404, "{bytes:?}");
        let problem: Value = serde_json::from_slice(&bytes).expect("problem+json");
        assert!(problem["type"].as_str().is_some(), "the shared problem+json surface: {problem}");
    }
}

/// A path identifier that is neither a UUID nor the anonymous GTS resource
/// instance form is a `400` naming the field.
#[tokio::test]
async fn a_malformed_route_identifier_is_a_validation_failure() {
    let tenant = Uuid::new_v4();
    let surface = permissive_surface(None).await;
    for (method, path) in [
        (http::Method::GET, "/oagw/v1/routes/not-an-identifier"),
        (http::Method::PUT, "/oagw/v1/routes/not-an-identifier"),
        (http::Method::DELETE, "/oagw/v1/routes/not-an-identifier"),
    ] {
        let body = if method == http::Method::PUT {
            Some(json!({ "match": { "http": { "path": "/v1", "methods": ["GET"] } } }))
        } else {
            None
        };
        let (status, bytes) = surface.send(method.clone(), path, Some(security_context(tenant, Uuid::new_v4())), body).await;
        assert_eq!(status, 400, "{method} {bytes:?}");
        let problem: Value = serde_json::from_slice(&bytes).expect("problem+json");
        assert!(
            problem["detail"].as_str().is_some_and(|detail| detail.contains("id")),
            "the rejection names the field: {problem}"
        );
    }
}

/// The anonymous GTS resource instance identifier addresses the same record.
#[tokio::test]
async fn the_anonymous_gts_resource_identifier_addresses_the_route() {
    let tenant = Uuid::new_v4();
    let surface = permissive_surface(None).await;
    let upstream_id = seed_upstream(&surface, tenant, "api.vendor.com").await;
    let (status, bytes) = surface.create_route(tenant, Uuid::new_v4(), route_body(upstream_id, "/v1/orders")).await;
    let created: Value = serde_json::from_slice(&bytes).expect("created");
    let id: Uuid = serde_json::from_value(created["id"].clone()).expect("id");
    assert_eq!(status, 201);

    let resource = oagw::domain::gts_helpers::route_resource_id(id);
    let (status, bytes) = surface
        .send(
            http::Method::GET,
            &format!("/oagw/v1/routes/{resource}"),
            Some(security_context(tenant, Uuid::new_v4())),
            None,
        )
        .await;
    assert_eq!(status, 200, "{bytes:?}");
    let read: Value = serde_json::from_slice(&bytes).expect("the stored route");
    assert_eq!(read["id"], created["id"], "both spellings name the same record");
}

/// Every operation evaluates its own route permission, and a deny is the
/// shared canonical permission-denied surface with no store write.
#[tokio::test]
async fn a_denied_permission_is_the_canonical_permission_denied_surface() {
    let tenant = Uuid::new_v4();
    let authz = Arc::new(FakePolicyAuthZ::default());
    let surface = management_surface(None, authz.clone(), Arc::new(FakeHierarchyTenantResolver::default())).await;
    let upstream_id = seed_upstream(&surface, tenant, "api.vendor.com").await;
    let (status, bytes) = surface.create_route(tenant, Uuid::new_v4(), route_body(upstream_id, "/v1/orders")).await;
    assert_eq!(status, 201, "{bytes:?}");
    let created: Value = serde_json::from_slice(&bytes).expect("created");
    let id: Uuid = serde_json::from_value(created["id"].clone()).expect("id");
    let body = json!({ "match": { "http": { "path": "/v1/orders", "methods": ["GET"] } } });

    for (permission, method, path, request_body) in [
        ("gts.cf.core.oagw.route.v1~:create", http::Method::POST, "/oagw/v1/routes".to_owned(), Some(route_body(upstream_id, "/v2/billing"))),
        ("gts.cf.core.oagw.route.v1~:read", http::Method::GET, "/oagw/v1/routes".to_owned(), None),
        ("gts.cf.core.oagw.route.v1~:read", http::Method::GET, format!("/oagw/v1/routes/{id}"), None),
        ("gts.cf.core.oagw.route.v1~:override", http::Method::PUT, format!("/oagw/v1/routes/{id}"), Some(body)),
        ("gts.cf.core.oagw.route.v1~:delete", http::Method::DELETE, format!("/oagw/v1/routes/{id}"), None),
    ] {
        authz.deny(permission);
        let (status, bytes) = surface
            .send(method.clone(), &path, Some(security_context(tenant, Uuid::new_v4())), request_body)
            .await;
        assert_eq!(status, 403, "{permission} {method} {bytes:?}");
        let problem: Value = serde_json::from_slice(&bytes).expect("problem+json");
        assert!(
            problem["type"].as_str().is_some_and(|problem_type| problem_type.contains("permission_denied")),
            "the canonical permission-denied surface renders: {problem}"
        );
        assert!(
            problem["context"]["resource_type"]
                .as_str()
                .is_some_and(|resource| resource == "gts.cf.core.oagw.route.v1~"),
            "the evaluated resource is the route base type: {problem}"
        );
        assert!(
            problem["context"]["reason"].as_str().is_some_and(|reason| reason.contains(permission)),
            "the evaluated permission is the reason: {problem}"
        );
    }

    // A deny stores nothing: the gate precedes the store.
    authz.deny("gts.cf.core.oagw.route.v1~:create");
    let (status, bytes) = surface.create_route(tenant, Uuid::new_v4(), route_body(upstream_id, "/v2/billing")).await;
    assert_eq!(status, 403, "{bytes:?}");
    let counts = surface.gear.storage().expect("storage").row_counts();
    assert_eq!(counts["oagw_route"], 1, "the denied create wrote nothing");
}

/// A request with no security context is the `401` authentication surface,
/// raised before any payload validation.
#[tokio::test]
async fn a_request_without_a_security_context_is_unauthenticated() {
    let surface = permissive_surface(None).await;
    for (method, path, body) in [
        (http::Method::POST, "/oagw/v1/routes".to_owned(), Some(json!({}))),
        (http::Method::GET, "/oagw/v1/routes".to_owned(), None),
        (http::Method::GET, format!("/oagw/v1/routes/{}", Uuid::new_v4()), None),
        (http::Method::DELETE, format!("/oagw/v1/routes/{}", Uuid::new_v4()), None),
    ] {
        let (status, bytes) = surface.send(method.clone(), &path, None, body).await;
        assert_eq!(status, 401, "{method} {bytes:?}");
        let problem: Value = serde_json::from_slice(&bytes).expect("problem+json");
        assert!(
            problem["type"]
                .as_str()
                .is_some_and(|problem_type| problem_type.contains("cf.oagw.auth.failed.v1")),
            "the OAGW authentication surface: {problem}"
        );
    }
}

/// An accepted route write emits the structured audit event of DESIGN §4.3
/// with the caller's attribution, after the store write.
#[tokio::test]
async fn an_accepted_write_emits_the_structured_audit_event() {
    let tenant = Uuid::new_v4();
    let principal = Uuid::new_v4();
    let surface = permissive_surface(None).await;
    let upstream_id = seed_upstream(&surface, tenant, "api.vendor.com").await;
    let hook = RecordingConfigWriteHook::new();
    surface
        .gear
        .routes()
        .expect("the route aggregate is published")
        .set_config_write_hook(hook.clone());

    let (status, bytes) = surface.create_route(tenant, principal, route_body(upstream_id, "/v1/orders")).await;
    assert_eq!(status, 201, "{bytes:?}");
    let created: Value = serde_json::from_slice(&bytes).expect("created");
    let id: Uuid = serde_json::from_value(created["id"].clone()).expect("id");

    let (status, _) = surface
        .replace_route(
            tenant,
            principal,
            id,
            json!({ "match": { "http": { "path": "/v1/orders", "methods": ["POST"] } } }),
        )
        .await;
    assert_eq!(status, 200, "the replacement is accepted");
    let (status, bytes) = surface.delete_route(tenant, principal, id).await;
    assert_eq!(status, 204, "{bytes:?}");

    let events = hook.notifications();
    assert_eq!(events.len(), 3, "one event per accepted write: {events:?}");
    let resource = created["id"].clone();
    let id: Uuid = serde_json::from_value(resource.clone()).expect("the identifier");
    for event in ["route.create", "route.replace", "route.delete"] {
        let found = events
            .iter()
            .find(|notification| notification.event == event)
            .unwrap_or_else(|| panic!("the {event} event is emitted"));
        assert_eq!(found.tenant_id, tenant, "the owner tenant is attributed");
        assert_eq!(found.principal_id, principal, "the caller is attributed");
        assert_eq!(
            found.resource_id,
            oagw::domain::gts_helpers::route_resource_id(id),
            "the route resource identifier is the anonymous GTS form"
        );
        assert_eq!(found.upstream_id, Some(upstream_id), "the owning upstream is carried");
        assert_eq!(found.outcome, "accepted", "the outcome is carried");
    }
}

/// A rejected route write emits no audit event: the hook runs after the store
/// write, so a failed write leaves the configuration untouched and unannounced.
#[tokio::test]
async fn a_rejected_write_emits_no_audit_event() {
    let tenant = Uuid::new_v4();
    let surface = permissive_surface(None).await;
    let upstream_id = seed_upstream(&surface, tenant, "api.vendor.com").await;
    let hook = RecordingConfigWriteHook::new();
    surface.gear.routes().expect("routes").set_config_write_hook(hook.clone());

    let (status, bytes) = surface
        .create_route(
            tenant,
            Uuid::new_v4(),
            json!({ "upstream_id": upstream_id.to_string(), "match": {} }),
        )
        .await;
    assert_eq!(status, 400, "{bytes:?}");
    let (status, bytes) = surface
        .create_route(
            tenant,
            Uuid::new_v4(),
            json!({ "upstream_id": Uuid::new_v4().to_string(), "match": { "http": { "path": "/v1", "methods": ["GET"] } } }),
        )
        .await;
    assert_eq!(status, 404, "{bytes:?}");
    assert!(hook.notifications().is_empty(), "no event for a rejected write: {:?}", hook.notifications());
}
