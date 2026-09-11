//! Integration tests for the strict tenant scoping of the management surface
//! (`cpt-cf-oagw-dod-upstream-management-tenant-scoping`).
//!
//! A record owned by another tenant - a sibling, a descendant or an ancestor -
//! is not-found on every operation, and the not-found response discloses
//! nothing about the foreign record.
// @cpt-dod:cpt-cf-oagw-dod-upstream-management-tenant-scoping:p1

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use serde_json::{Value, json};
use uuid::Uuid;

use oagw::test_support::{
    FakePolicyAuthZ, FakeHierarchyTenantResolver, management_surface, security_context,
};

const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

fn pool(host: &str) -> Value {
    json!({
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [ { "host": host } ] }
    })
}

/// A record owned by one tenant is not-found to a sibling tenant on every
/// operation, and the response is indistinguishable from the response for an
/// identifier that does not exist at all.
#[tokio::test]
async fn a_foreign_record_is_not_found_and_discloses_nothing() {
    let owner = Uuid::new_v4();
    let other = Uuid::new_v4();
    let surface = management_surface(
        None,
        Arc::new(FakePolicyAuthZ::default()),
        Arc::new(FakeHierarchyTenantResolver::default()),
    )
    .await;
    let (status, bytes) = surface.create(owner, Uuid::new_v4(), pool("api.vendor.com")).await;
    assert_eq!(status, 201, "{bytes:?}");
    let record: Value = serde_json::from_slice(&bytes).expect("record");
    let id: Uuid = serde_json::from_value(record["id"].clone()).expect("identifier");

    let (missing_status, missing) = surface.get(other, Uuid::new_v4(), Uuid::new_v4()).await;
    assert_eq!(missing_status, 404, "{missing:?}");

    let (status, bytes) = surface.get(other, Uuid::new_v4(), id).await;
    assert_eq!(status, 404, "a foreign record is not found: {bytes:?}");
    // Identical apart from `instance`, which is the path the caller addressed,
    // and from `trace_id`, the per-request correlation identifier the
    // trace-propagation entry mints for a request that carried no
    // `X-Request-ID`; no member names the tenant the record belongs to or
    // whether it exists.
    let strip = |bytes: &[u8]| -> Value {
        let mut document: Value = serde_json::from_slice(bytes).expect("problem+json");
        let object = document.as_object_mut().expect("an object");
        object.remove("instance");
        object.remove("trace_id");
        document
    };
    assert_eq!(
        strip(&bytes),
        strip(&missing),
        "the response discloses nothing about the foreign record"
    );
    assert!(
        !serde_json::from_slice::<Value>(&bytes)
            .expect("problem")
            .to_string()
            .contains(&owner.to_string()),
        "the owner tenant is never named: {missing:?}"
    );

    for (method, request_body) in [
        (http::Method::PUT, Some(pool("other.vendor.com"))),
        (http::Method::DELETE, None),
    ] {
        let (status, bytes) = surface
            .send(
                method.clone(),
                &format!("/oagw/v1/upstreams/{id}"),
                Some(security_context(other, Uuid::new_v4())),
                request_body,
            )
            .await;
        assert_eq!(status, 404, "{method}: {bytes:?}");
    }
    // The owner's record is untouched, and the caller's list holds nothing.
    let (status, bytes) = surface.get(owner, Uuid::new_v4(), id).await;
    assert_eq!(status, 200, "the owner still reads its own record: {bytes:?}");
    let (status, listed) = surface
        .send(
            http::Method::GET,
            "/oagw/v1/upstreams",
            Some(security_context(other, Uuid::new_v4())),
            None,
        )
        .await;
    assert_eq!(status, 200, "{listed:?}");
    let listed: Value = serde_json::from_slice(&listed).expect("list");
    assert_eq!(listed["count"], 0, "a foreign record is never listed: {listed}");
}

/// An ancestor's record is not-found to a descendant on every operation, and
/// the descendant's write changes nothing in the ancestor's record.
#[tokio::test]
async fn an_ancestor_record_is_not_addressable_from_below() {
    let parent = Uuid::new_v4();
    let child = Uuid::new_v4();
    let chain = [child, parent];
    let surface = management_surface(
        None,
        Arc::new(FakePolicyAuthZ::default()),
        FakeHierarchyTenantResolver::over(&chain),
    )
    .await;
    let (status, bytes) = surface.create(parent, Uuid::new_v4(), pool("api.vendor.com")).await;
    assert_eq!(status, 201, "{bytes:?}");
    let record: Value = serde_json::from_slice(&bytes).expect("record");
    let id: Uuid = serde_json::from_value(record["id"].clone()).expect("identifier");

    let (status, bytes) = surface.get(child, Uuid::new_v4(), id).await;
    assert_eq!(status, 404, "an ancestor record is not-found below: {bytes:?}");

    let (status, bytes) = surface
        .send(
            http::Method::PUT,
            &format!("/oagw/v1/upstreams/{id}"),
            Some(security_context(child, Uuid::new_v4())),
            Some(pool("changed.vendor.com")),
        )
        .await;
    assert_eq!(status, 404, "a descendant cannot replace an ancestor record: {bytes:?}");

    let (status, bytes) = surface
        .send(
            http::Method::DELETE,
            &format!("/oagw/v1/upstreams/{id}"),
            Some(security_context(child, Uuid::new_v4())),
            None,
        )
        .await;
    assert_eq!(status, 404, "a descendant cannot delete an ancestor record: {bytes:?}");

    // The ancestor's record is unchanged and still its own.
    let (status, served) = surface.get(parent, Uuid::new_v4(), id).await;
    assert_eq!(status, 200, "{served:?}");
    let served: Value = serde_json::from_slice(&served).expect("record");
    assert_eq!(served["alias"], "api.vendor.com", "nothing was written");
}

/// A descendant's record is equally foreign to the ancestor: scoping is
/// symmetric and actor-agnostic.
#[tokio::test]
async fn a_descendant_record_is_not_addressable_from_above() {
    let parent = Uuid::new_v4();
    let child = Uuid::new_v4();
    let chain = [child, parent];
    let surface = management_surface(
        None,
        Arc::new(FakePolicyAuthZ::default()),
        FakeHierarchyTenantResolver::over(&chain),
    )
    .await;
    let (status, bytes) = surface.create(child, Uuid::new_v4(), pool("api.vendor.com")).await;
    assert_eq!(status, 201, "{bytes:?}");
    let record: Value = serde_json::from_slice(&bytes).expect("record");
    let id: Uuid = serde_json::from_value(record["id"].clone()).expect("identifier");

    let (status, bytes) = surface.get(parent, Uuid::new_v4(), id).await;
    assert_eq!(status, 404, "{bytes:?}");
    let (status, listed) = surface
        .send(
            http::Method::GET,
            "/oagw/v1/upstreams",
            Some(security_context(parent, Uuid::new_v4())),
            None,
        )
        .await;
    assert_eq!(status, 200, "{listed:?}");
    let listed: Value = serde_json::from_slice(&listed).expect("list");
    assert_eq!(listed["count"], 0, "a descendant record is never listed above: {listed}");
}

/// A tenant-scoped rejection leaves the store exactly as it was.
#[tokio::test]
async fn a_foreign_access_stores_nothing() {
    let owner = Uuid::new_v4();
    let other = Uuid::new_v4();
    let surface = management_surface(
        None,
        Arc::new(FakePolicyAuthZ::default()),
        Arc::new(FakeHierarchyTenantResolver::default()),
    )
    .await;
    let (status, bytes) = surface.create(owner, Uuid::new_v4(), pool("api.vendor.com")).await;
    assert_eq!(status, 201, "{bytes:?}");
    let record: Value = serde_json::from_slice(&bytes).expect("record");
    let id: Uuid = serde_json::from_value(record["id"].clone()).expect("identifier");

    let (status, _) = surface
        .send(
            http::Method::PUT,
            &format!("/oagw/v1/upstreams/{id}"),
            Some(security_context(other, Uuid::new_v4())),
            Some(pool("other.vendor.com")),
        )
        .await;
    assert_eq!(status, 404);
    let (status, _) = surface.create(other, Uuid::new_v4(), pool("other.vendor.com")).await;
    assert_eq!(status, 201, "the other tenant creates its own record");

    let counts = surface.gear.storage().expect("storage").row_counts();
    assert_eq!(counts["oagw_upstream"], 2, "one record per tenant, nothing else");

    let (status, bytes) = surface
        .send(
            http::Method::DELETE,
            &format!("/oagw/v1/upstreams/{id}"),
            Some(security_context(other, Uuid::new_v4())),
            None,
        )
        .await;
    assert_eq!(status, 404, "{bytes:?}");
    let counts = surface.gear.storage().expect("storage").row_counts();
    assert_eq!(counts["oagw_upstream"], 2, "a foreign delete removes nothing");
    let (status, _) = surface.get(owner, Uuid::new_v4(), id).await;
    assert_eq!(status, 200, "the owner's record survives the foreign delete");
}
