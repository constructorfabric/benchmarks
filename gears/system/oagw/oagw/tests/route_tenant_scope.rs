//! Integration tests for the tenant scoping of the route surface
//! (`cpt-cf-oagw-dod-route-management-tenant-scope`).
//!
//! A route identifier owned by another tenant — including an ancestor — is
//! `404`, indistinguishable from a missing record, in every operation.
// @cpt-dod:cpt-cf-oagw-dod-route-management-tenant-scoping:p1

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use serde_json::{Value, json};
use uuid::Uuid;

use oagw::test_support::{
    FakeHierarchyTenantResolver, FakePolicyAuthZ, management_surface, permissive_surface,
    route_body, security_context,
};

const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

fn upstream_body(host: &str) -> Value {
    json!({ "protocol": PROTOCOL_HTTP, "server": { "endpoints": [ { "host": host } ] } })
}

/// Seed one upstream and one route for `tenant`, returning the route id.
async fn seed_route(surface: &oagw::test_support::ManagementSurface, tenant: Uuid, host: &str) -> Uuid {
    let (status, bytes) = surface.create(tenant, Uuid::new_v4(), upstream_body(host)).await;
    assert_eq!(status, 201, "{bytes:?}");
    let parent: Value = serde_json::from_slice(&bytes).expect("created");
    let upstream_id: Uuid = serde_json::from_value(parent["id"].clone()).expect("id");
    let (status, bytes) = surface.create_route(tenant, Uuid::new_v4(), route_body(upstream_id, "/v1/orders")).await;
    assert_eq!(status, 201, "{bytes:?}");
    let created: Value = serde_json::from_slice(&bytes).expect("created");
    serde_json::from_value(created["id"].clone()).expect("the route identifier")
}

/// A foreign tenant's route is not readable, not replaceable, not deletable and
/// never listed, and the response is the same not-found a missing record gets.
#[tokio::test]
async fn a_foreign_route_is_not_found_in_every_operation() {
    let tenant = Uuid::new_v4();
    let other = Uuid::new_v4();
    let surface = permissive_surface(None).await;
    let own = seed_route(&surface, tenant, "api.vendor.com").await;
    let foreign = seed_route(&surface, other, "foreign.vendor.com").await;

    let (status, own_bytes) = surface.get_route(tenant, Uuid::new_v4(), Uuid::new_v4()).await;
    assert_eq!(status, 404);
    let (status, bytes) = surface.get_route(tenant, Uuid::new_v4(), foreign).await;
    assert_eq!(status, 404, "{bytes:?}");
    // The two bodies are identical apart from `instance`, which is the path the
    // caller addressed and is therefore different by construction, and apart
    // from `trace_id`, the per-request correlation identifier the
    // trace-propagation entry mints for a request that carried no
    // `X-Request-ID`, which differs per request for the same reason; no member
    // names the tenant the record belongs to or whether it exists.
    let strip = |bytes: &[u8]| -> Value {
        let mut document: Value = serde_json::from_slice(bytes).expect("problem+json");
        let object = document.as_object_mut().expect("an object");
        object.remove("instance");
        object.remove("trace_id");
        document
    };
    assert_eq!(
        strip(&bytes),
        strip(&own_bytes),
        "a foreign identifier is indistinguishable from a missing one"
    );
    assert_eq!(surface.get_route(other, Uuid::new_v4(), own).await.0, 404, "the reverse direction too");

    let body = json!({ "match": { "http": { "path": "/v1/orders", "methods": ["GET"] } } });
    assert_eq!(surface.replace_route(tenant, Uuid::new_v4(), foreign, body.clone()).await.0, 404);
    assert_eq!(surface.delete_route(tenant, Uuid::new_v4(), foreign).await.0, 404);

    // The foreign record is untouched by the attempts above.
    let counts = surface.gear.storage().expect("storage").row_counts();
    assert_eq!(counts["oagw_route"], 2, "neither record was written");
    let (status, bytes) = surface.get_route(other, Uuid::new_v4(), foreign).await;
    assert_eq!(status, 200, "{bytes:?}");
    let read: Value = serde_json::from_slice(&bytes).expect("read");
    assert_eq!(read["match"]["http"]["path"], "/v1/orders", "the foreign record survives");
}

/// A list carries only the caller's own routes, and an ancestor's routes are
/// never visible through this API.
#[tokio::test]
async fn the_list_is_scoped_to_the_calling_tenant() {
    let parent = Uuid::new_v4();
    let child = Uuid::new_v4();
    let chain = [child, parent];
    let surface = management_surface(
        None,
        Arc::new(FakePolicyAuthZ::default()),
        FakeHierarchyTenantResolver::over(&chain),
    )
    .await;
    seed_route(&surface, parent, "parent.vendor.com").await;
    seed_route(&surface, child, "child.vendor.com").await;

    let (status, bytes) = surface.list_routes(child, Uuid::new_v4(), "").await;
    assert_eq!(status, 200, "{bytes:?}");
    let listed: Value = serde_json::from_slice(&bytes).expect("list");
    assert_eq!(listed["count"], 1, "an ancestor's routes are not visible: {listed}");
    let (status, bytes) = surface.list_routes(parent, Uuid::new_v4(), "").await;
    assert_eq!(status, 200, "{bytes:?}");
    let listed: Value = serde_json::from_slice(&bytes).expect("list");
    assert_eq!(listed["count"], 1, "a descendant's routes are not visible either");
}

/// A tenant's route is addressable through a security context that names the
/// same tenant, whatever principal carries it.
#[tokio::test]
async fn a_route_is_addressable_by_any_principal_of_its_tenant() {
    let tenant = Uuid::new_v4();
    let surface = permissive_surface(None).await;
    let id = seed_route(&surface, tenant, "api.vendor.com").await;
    for principal in [Uuid::new_v4(), Uuid::nil()] {
        let (status, bytes) = surface
            .send(
                http::Method::GET,
                &format!("/oagw/v1/routes/{id}"),
                Some(security_context(tenant, principal)),
                None,
            )
            .await;
        assert_eq!(status, 200, "{bytes:?}");
    }
}
