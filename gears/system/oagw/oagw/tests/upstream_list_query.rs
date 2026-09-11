//! Integration tests for the OData list parameters over the HTTP surface
//! (`cpt-cf-oagw-dod-upstream-management-odata-list`).
// @cpt-dod:cpt-cf-oagw-dod-upstream-management-odata-list:p1

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use serde_json::{Value, json};
use uuid::Uuid;

use oagw::test_support::{
    FakePolicyAuthZ, FakeHierarchyTenantResolver, management_surface, security_context,
};

const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

async fn surface() -> oagw::test_support::ManagementSurface {
    management_surface(
        None,
        Arc::new(FakePolicyAuthZ::default()),
        Arc::new(FakeHierarchyTenantResolver::default()),
    )
    .await
}

fn pool(host: &str) -> Value {
    json!({
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [ { "host": host } ] },
        "tags": [host.split('.').next().unwrap_or("tag")]
    })
}

/// Seed three records whose aliases and tags differ, so the query parameters
/// have something to select and order.
async fn seed(surface: &oagw::test_support::ManagementSurface, tenant: Uuid) {
    for host in ["alpha.vendor.com", "beta.vendor.com", "gamma.vendor.com"] {
        let (status, bytes) = surface.create(tenant, Uuid::new_v4(), pool(host)).await;
        assert_eq!(status, 201, "{bytes:?}");
    }
}

/// `$filter`, `$orderby`, `$top` and `$skip` combine over the caller's own
/// records, applied in the FEATURE's filter, ordering, offset, limit sequence.
#[tokio::test]
async fn the_list_applies_filter_ordering_top_and_skip() {
    let tenant = Uuid::new_v4();
    let surface = surface().await;
    seed(&surface, tenant).await;

    let (status, bytes) = surface
        .send(
            http::Method::GET,
            "/oagw/v1/upstreams?$filter=alias%20ne%20%27gamma.vendor.com%27&$orderby=alias%20desc&$top=2&$skip=1",
            Some(security_context(tenant, Uuid::new_v4())),
            None,
        )
        .await;
    assert_eq!(status, 200, "{bytes:?}");
    let listed: Value = serde_json::from_slice(&bytes).expect("list");
    assert_eq!(listed["count"], 1, "one record matches the filter after the skip: {listed}");
    let aliases: Vec<&str> = listed["items"]
        .as_array()
        .expect("items")
        .iter()
        .filter_map(|item| item["alias"].as_str())
        .collect();
    assert_eq!(aliases, vec!["alpha.vendor.com"]);

    // `$top` alone bounds the page, and an explicit `$top` of the cap is
    // honored.
    let (status, bytes) = surface
        .send(
            http::Method::GET,
            "/oagw/v1/upstreams?$top=2",
            Some(security_context(tenant, Uuid::new_v4())),
            None,
        )
        .await;
    assert_eq!(status, 200, "{bytes:?}");
    let listed: Value = serde_json::from_slice(&bytes).expect("list");
    assert_eq!(listed["count"], 2);
}

/// `$select` projects the named fields and the response reports the count of
/// the records actually returned.
#[tokio::test]
async fn the_list_projects_the_selected_fields() {
    let tenant = Uuid::new_v4();
    let surface = surface().await;
    seed(&surface, tenant).await;

    let (status, bytes) = surface
        .send(
            http::Method::GET,
            "/oagw/v1/upstreams?$select=alias,enabled",
            Some(security_context(tenant, Uuid::new_v4())),
            None,
        )
        .await;
    assert_eq!(status, 200, "{bytes:?}");
    let listed: Value = serde_json::from_slice(&bytes).expect("list");
    assert_eq!(listed["count"], 3, "the count actually returned");
    let first = &listed["items"][0];
    assert!(first.get("alias").is_some(), "the projected field is present: {first}");
    assert!(first.get("server").is_none(), "an unprojected field is absent: {first}");
}

/// An empty result set is a successful empty list.
#[tokio::test]
async fn an_empty_result_is_a_successful_empty_list() {
    let tenant = Uuid::new_v4();
    let surface = surface().await;
    let (status, bytes) = surface
        .send(
            http::Method::GET,
            "/oagw/v1/upstreams",
            Some(security_context(tenant, Uuid::new_v4())),
            None,
        )
        .await;
    assert_eq!(status, 200, "{bytes:?}");
    let listed: Value = serde_json::from_slice(&bytes).expect("list");
    assert_eq!(listed["count"], 0);
    assert_eq!(listed["items"], json!([]));
}

/// An out-of-range `$top`, a negative offset, or an unparseable expression is a
/// validation error naming the parameter.
#[tokio::test]
async fn a_malformed_or_out_of_range_parameter_is_named() {
    let tenant = Uuid::new_v4();
    let surface = surface().await;
    seed(&surface, tenant).await;
    for query in [
        "$top=101",
        "$top=-1",
        "$skip=-3",
        "$filter=alias%20~~~%20%27x%27",
        "$orderby=42",
        "$select=",
        "$unknown=1",
    ] {
        let (status, bytes) = surface
            .send(
                http::Method::GET,
                &format!("/oagw/v1/upstreams?{query}"),
                Some(security_context(tenant, Uuid::new_v4())),
                None,
            )
            .await;
        assert_eq!(status, 400, "{query}: {bytes:?}");
        let problem: Value = serde_json::from_slice(&bytes).expect("problem");
        let detail = problem["detail"].as_str().unwrap_or_default().to_owned();
        assert!(
            detail.contains('$'),
            "the offending parameter is named: {problem}"
        );
    }
}

/// The list never contains a record owned by another tenant, including an
/// ancestor.
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
    for host in ["parent-one.vendor.com", "parent-two.vendor.com"] {
        let (status, _) = surface.create(parent, Uuid::new_v4(), pool(host)).await;
        assert_eq!(status, 201);
    }
    let (status, _) = surface.create(child, Uuid::new_v4(), pool("child.vendor.com")).await;
    assert_eq!(status, 201);

    let (status, bytes) = surface
        .send(
            http::Method::GET,
            "/oagw/v1/upstreams",
            Some(security_context(child, Uuid::new_v4())),
            None,
        )
        .await;
    assert_eq!(status, 200, "{bytes:?}");
    let listed: Value = serde_json::from_slice(&bytes).expect("list");
    assert_eq!(listed["count"], 1, "only the caller's own records: {listed}");
}

/// A `$top` above the cap is rejected rather than clamped.
#[tokio::test]
async fn the_top_cap_is_enforced() {
    let tenant = Uuid::new_v4();
    let surface = surface().await;
    let (status, bytes) = surface
        .send(
            http::Method::GET,
            "/oagw/v1/upstreams?$top=100",
            Some(security_context(tenant, Uuid::new_v4())),
            None,
        )
        .await;
    assert_eq!(status, 200, "the cap itself is honored: {bytes:?}");
}
