//! Integration tests for the upstream reference resolution of a route write
//! (`cpt-cf-oagw-dod-route-management-upstream-reference`).
//!
//! `upstream_id` must belong to the calling tenant: a foreign-tenant or
//! missing reference is `404`, indistinguishable, and discloses nothing.
// @cpt-dod:cpt-cf-oagw-dod-route-management-upstream-reference:p1

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use serde_json::{Value, json};
use uuid::Uuid;

use oagw::test_support::{
    FakeHierarchyTenantResolver, FakePolicyAuthZ, management_surface, permissive_surface,
    route_body,
};

const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

fn upstream_body(host: &str) -> Value {
    json!({ "protocol": PROTOCOL_HTTP, "server": { "endpoints": [ { "host": host } ] } })
}

/// A route under an upstream the calling tenant owns is created and addressable.
#[tokio::test]
async fn a_route_under_an_own_upstream_is_created() {
    let tenant = Uuid::new_v4();
    let surface = permissive_surface(None).await;
    let (status, bytes) = surface.create(tenant, Uuid::new_v4(), upstream_body("api.vendor.com")).await;
    assert_eq!(status, 201, "{bytes:?}");
    let parent: Value = serde_json::from_slice(&bytes).expect("created");
    let upstream_id: Uuid = serde_json::from_value(parent["id"].clone()).expect("id");

    let (status, bytes) = surface.create_route(tenant, Uuid::new_v4(), route_body(upstream_id, "/v1/orders")).await;
    assert_eq!(status, 201, "{bytes:?}");
    let created: Value = serde_json::from_slice(&bytes).expect("created");
    assert_eq!(created["upstream_id"], json!(upstream_id.to_string()));
}

/// A route whose `upstream_id` is missing, foreign or an ancestor's is `404`,
/// and every rejection is indistinguishable from the others.
#[tokio::test]
async fn a_foreign_or_missing_upstream_reference_is_not_found() {
    let tenant = Uuid::new_v4();
    let other = Uuid::new_v4();
    let surface = permissive_surface(None).await;
    let (status, bytes) = surface.create(tenant, Uuid::new_v4(), upstream_body("api.vendor.com")).await;
    assert_eq!(status, 201, "{bytes:?}");

    let foreign = surface.create(other, Uuid::new_v4(), upstream_body("foreign.vendor.com")).await;
    assert_eq!(foreign.0, 201, "{:?}", foreign.1);
    let foreign_parent: Value = serde_json::from_slice(&foreign.1).expect("created");
    let foreign_id: Uuid = serde_json::from_value(foreign_parent["id"].clone()).expect("id");

    let mut details = Vec::new();
    for upstream_id in [Uuid::new_v4(), foreign_id, Uuid::nil()] {
        let (status, bytes) = surface
            .create_route(
                tenant,
                Uuid::new_v4(),
                json!({ "upstream_id": upstream_id.to_string(), "match": { "http": { "path": "/v1", "methods": ["GET"] } } }),
            )
            .await;
        assert_eq!(status, 404, "{upstream_id} {bytes:?}");
        let problem: Value = serde_json::from_slice(&bytes).expect("problem+json");
        details.push(problem["detail"].clone());
        assert!(problem["detail"].as_str().is_some(), "the shared problem+json surface: {problem}");
    }
    assert_eq!(details[0], details[1], "a missing and a foreign reference are indistinguishable");
    assert_eq!(details[1], details[2], "a foreign and a nil reference are indistinguishable");
    assert!(
        !details[1].as_str().unwrap_or("").contains(&foreign_id.to_string()),
        "no reference value is echoed: {}",
        details[1]
    );
    let counts = surface.gear.storage().expect("storage").row_counts();
    assert_eq!(counts["oagw_route"], 0, "no route was stored for an unresolvable reference");
}

/// An ancestor tenant's upstream is not directly addressable through the
/// management API, so a route cannot be attached to it.
#[tokio::test]
async fn an_ancestor_upstream_reference_is_not_found() {
    let parent = Uuid::new_v4();
    let child = Uuid::new_v4();
    let chain = [child, parent];
    let surface = management_surface(
        None,
        Arc::new(FakePolicyAuthZ::default()),
        FakeHierarchyTenantResolver::over(&chain),
    )
    .await;
    let (status, bytes) = surface.create(parent, Uuid::new_v4(), upstream_body("ancestor.vendor.com")).await;
    assert_eq!(status, 201, "{bytes:?}");
    let ancestor: Value = serde_json::from_slice(&bytes).expect("created");
    let ancestor_id: Uuid = serde_json::from_value(ancestor["id"].clone()).expect("id");

    let (status, bytes) = surface
        .create_route(
            child,
            Uuid::new_v4(),
            json!({ "upstream_id": ancestor_id.to_string(), "match": { "http": { "path": "/v1", "methods": ["GET"] } } }),
        )
        .await;
    assert_eq!(status, 404, "{bytes:?}");
    let counts = surface.gear.storage().expect("storage").row_counts();
    assert_eq!(counts["oagw_route"], 0, "no route is attached to an ancestor upstream");
}

/// A foreign upstream reference is not a `403`: the tenant scoping of the
/// reference is a lookup, not an authorization decision.
#[tokio::test]
async fn a_foreign_upstream_reference_is_not_a_permission_denial() {
    let tenant = Uuid::new_v4();
    let surface = permissive_surface(None).await;
    let (status, bytes) = surface.create_route(
        tenant,
        Uuid::new_v4(),
        json!({ "upstream_id": Uuid::new_v4().to_string(), "match": { "http": { "path": "/v1", "methods": ["GET"] } } }),
    ).await;
    assert_eq!(status, 404, "{bytes:?}");
    let problem: Value = serde_json::from_slice(&bytes).expect("problem+json");
    assert!(
        problem["type"].as_str().is_some_and(|problem_type| problem_type.contains("not_found")),
        "the not-found surface renders, never a permission denial: {problem}"
    );
}
