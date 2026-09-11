//! Integration tests for the alias outcomes over the HTTP surface
//! (`cpt-cf-oagw-dod-upstream-management-alias-derivation`,
//! `cpt-cf-oagw-dod-upstream-management-alias-normalization`).
// @cpt-dod:cpt-cf-oagw-dod-upstream-management-alias-immutability:p1
// @cpt-dod:cpt-cf-oagw-dod-upstream-management-alias-normalization:p1

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use serde_json::{Value, json};
use uuid::Uuid;

use oagw::test_support::{FakeHierarchyTenantResolver, management_surface, security_context};

const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

fn pool(endpoints: &[Value]) -> Value {
    json!({ "protocol": PROTOCOL_HTTP, "server": { "endpoints": endpoints } })
}

fn https(host: &str) -> Value {
    json!({ "host": host })
}

/// Every row of the alias-derivation table over `POST`.
#[tokio::test]
async fn the_derivation_table_holds_over_the_http_surface() {
    let tenant = Uuid::new_v4();
    let surface = management_surface(
        None,
        std::sync::Arc::new(oagw::test_support::FakePolicyAuthZ::default()),
        Arc::new(FakeHierarchyTenantResolver::default()),
    )
    .await;

    // One hostname on the standard port derives the hostname.
    let (status, bytes) = surface.create(tenant, Uuid::new_v4(), pool(&[https("api.vendor.com")])).await;
    assert_eq!(status, 201, "{bytes:?}");
    let record: Value = serde_json::from_slice(&bytes).expect("record");
    assert_eq!(record["alias"], "api.vendor.com");

    // One hostname on a non-standard port derives `hostname:port`.
    let mut endpoint = https("api.vendor.com");
    endpoint["port"] = json!(8443);
    let (status, bytes) = surface.create(tenant, Uuid::new_v4(), pool(&[endpoint])).await;
    assert_eq!(status, 201, "{bytes:?}");
    let record: Value = serde_json::from_slice(&bytes).expect("record");
    assert_eq!(record["alias"], "api.vendor.com:8443");

    // Two hostnames derive the registrable common suffix.
    let (status, bytes) = surface
        .create(
            tenant,
            Uuid::new_v4(),
            pool(&[https("us.vendor.com"), https("eu.vendor.com")]),
        )
        .await;
    assert_eq!(status, 201, "{bytes:?}");
    let record: Value = serde_json::from_slice(&bytes).expect("record");
    assert_eq!(record["alias"], "vendor.com");

    // A two-label country suffix is a bare public suffix: non-derivable, so an
    // explicit alias is required.
    let (status, bytes) = surface
        .create(tenant, Uuid::new_v4(), pool(&[https("foo.co.uk"), https("bar.co.uk")]))
        .await;
    assert_eq!(status, 400, "{bytes:?}");
    let problem: Value = serde_json::from_slice(&bytes).expect("problem");
    assert!(problem["detail"].as_str().unwrap_or("").contains("alias"), "{problem}");
}

/// An IP-based pool without an alias is rejected; with one it is accepted.
#[tokio::test]
async fn an_ip_pool_requires_an_explicit_alias() {
    let tenant = Uuid::new_v4();
    let surface = management_surface(
        None,
        std::sync::Arc::new(oagw::test_support::FakePolicyAuthZ::default()),
        Arc::new(FakeHierarchyTenantResolver::default()),
    )
    .await;
    let ip = json!({ "host": "10.0.0.7" });

    let (status, bytes) = surface.create(tenant, Uuid::new_v4(), pool(&[ip.clone()])).await;
    assert_eq!(status, 400, "{bytes:?}");
    let problem: Value = serde_json::from_slice(&bytes).expect("problem");
    assert!(
        problem["detail"].as_str().unwrap_or("").contains("explicit alias"),
        "the rejection states the requirement: {problem}"
    );

    let mut explicit = pool(&[ip]);
    explicit["alias"] = json!("ip-gateway");
    let (status, bytes) = surface.create(tenant, Uuid::new_v4(), explicit).await;
    assert_eq!(status, 201, "{bytes:?}");
    let record: Value = serde_json::from_slice(&bytes).expect("record");
    assert_eq!(record["alias"], "ip-gateway");
}

/// A supplied alias equal to the derived value reconciles without a
/// validation error, and a differing one is rejected naming the derived value.
#[tokio::test]
async fn a_supplied_alias_is_reconciled_with_the_derived_value() {
    let tenant = Uuid::new_v4();
    let surface = management_surface(
        None,
        std::sync::Arc::new(oagw::test_support::FakePolicyAuthZ::default()),
        Arc::new(FakeHierarchyTenantResolver::default()),
    )
    .await;

    let mut exact = pool(&[https("api.vendor.com")]);
    exact["alias"] = json!("api.vendor.com");
    let (status, bytes) = surface.create(tenant, Uuid::new_v4(), exact).await;
    assert_eq!(status, 201, "{bytes:?}");

    let mut differing = pool(&[https("api.vendor.com")]);
    differing["alias"] = json!("somewhere.else");
    let (status, bytes) = surface.create(tenant, Uuid::new_v4(), differing).await;
    assert_eq!(status, 400, "{bytes:?}");
    let problem: Value = serde_json::from_slice(&bytes).expect("problem");
    assert!(
        problem["detail"].as_str().unwrap_or("").contains("api.vendor.com"),
        "the rejection names the derived value: {problem}"
    );
}

/// The alias is stored ASCII lowercase with the trailing dot stripped, and the
/// same `(tenant_id, alias)` is one record only.
#[tokio::test]
async fn the_alias_is_normalized_and_unique_per_tenant() {
    let tenant = Uuid::new_v4();
    let surface = management_surface(
        None,
        std::sync::Arc::new(oagw::test_support::FakePolicyAuthZ::default()),
        Arc::new(FakeHierarchyTenantResolver::default()),
    )
    .await;

    let mut noisy = pool(&[https("api.vendor.com")]);
    noisy["alias"] = json!("API.Vendor.com.");
    let (status, bytes) = surface.create(tenant, Uuid::new_v4(), noisy).await;
    assert_eq!(status, 201, "{bytes:?}");
    let record: Value = serde_json::from_slice(&bytes).expect("record");
    assert_eq!(record["alias"], "api.vendor.com", "lowercase, no trailing dot");

    let mut again = pool(&[https("api.vendor.com")]);
    again["alias"] = json!("api.vendor.com");
    let (status, bytes) = surface.create(tenant, Uuid::new_v4(), again).await;
    assert_eq!(status, 409, "the same `(tenant_id, alias)` is a conflict: {bytes:?}");
}

/// A create whose alias matches an ancestor's upstream is a bind: accepted
/// with `oagw:upstream:bind`, `403` through the canonical surface without it.
#[tokio::test]
async fn an_ancestor_alias_match_is_a_bind_gated_by_the_bind_permission() {
    let parent = Uuid::new_v4();
    let child = Uuid::new_v4();
    let chain = [child, parent];
    let authz = Arc::new(oagw::test_support::FakePolicyAuthZ::default());
    let surface = management_surface(None, authz.clone(), FakeHierarchyTenantResolver::over(&chain)).await;

    // The parent's record under the shared alias exists first.
    let (status, bytes) = surface
        .create(parent, Uuid::new_v4(), pool(&[https("shared.vendor.com")]))
        .await;
    assert_eq!(status, 201, "{bytes:?}");

    // The bind permission is granted: the descendant create is the bind case.
    let (status, bytes) = surface
        .create(child, Uuid::new_v4(), pool(&[https("shared.vendor.com")]))
        .await;
    assert_eq!(status, 201, "the bind is accepted with the permission: {bytes:?}");

    // The bind permission is absent: the same create is a `403`.
    authz.deny("oagw:upstream:bind");
    let (status, bytes) = surface
        .send(
            http::Method::DELETE,
            &format!("/oagw/v1/upstreams/{}", {
                let listed = surface
                    .send(
                        http::Method::GET,
                        "/oagw/v1/upstreams",
                        Some(security_context(child, Uuid::new_v4())),
                        None,
                    )
                    .await;
                let listed: Value = serde_json::from_slice(&listed.1).expect("list");
                listed["items"][0]["id"].as_str().unwrap_or_default().to_owned()
            }),
            Some(security_context(child, Uuid::new_v4())),
            None,
        )
        .await;
    assert_eq!(status, 204, "{bytes:?}");

    let (status, bytes) = surface
        .create(child, Uuid::new_v4(), pool(&[https("shared.vendor.com")]))
        .await;
    assert_eq!(status, 403, "{bytes:?}");
    let problem: Value = serde_json::from_slice(&bytes).expect("problem");
    assert!(
        problem["type"].as_str().is_some_and(|t| t.contains("permission_denied")),
        "the canonical permission-denied surface renders: {problem}"
    );
    assert!(
        problem["context"]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains(":bind")),
        "the evaluated permission is the bind permission: {problem}"
    );
}

/// An ancestor upstream carrying `sharing: private` blocks visibility, so the
/// alias stays available for a local create.
#[tokio::test]
async fn a_private_ancestor_record_leaves_the_alias_available() {
    let parent = Uuid::new_v4();
    let child = Uuid::new_v4();
    let chain = [child, parent];

    let surface = management_surface(
        None,
        std::sync::Arc::new(oagw::test_support::FakePolicyAuthZ::default()),
        FakeHierarchyTenantResolver::over(&chain),
    )
    .await;
    let mut ancestor = pool(&[https("shared.vendor.com")]);
    ancestor["plugins"] = json!({ "sharing": "private", "items": [] });
    let (status, _) = surface.create(parent, Uuid::new_v4(), ancestor).await;
    assert_eq!(status, 201);

    // The ancestor record is invisible to the descendant's list.
    let (status, listed) = surface
        .send(
            http::Method::GET,
            "/oagw/v1/upstreams",
            Some(security_context(child, Uuid::new_v4())),
            None,
        )
        .await;
    assert_eq!(status, 200, "{listed:?}");
    let listed: Value = serde_json::from_slice(&listed).expect("list");
    assert_eq!(listed["count"], 0, "an ancestor record is never listed for a descendant");

    // ... and the alias stays available for a local create.
    let (status, bytes) = surface
        .create(child, Uuid::new_v4(), pool(&[https("shared.vendor.com")]))
        .await;
    assert_eq!(status, 201, "the alias stays available: {bytes:?}");
}

/// The full replacement keeps the alias when the endpoint pool is unchanged
/// with an exact-match alias, and rejects an endpoint change that would alter
/// it with the delete-and-re-create remediation.
#[tokio::test]
async fn the_alias_immutability_matrix_holds_on_the_http_surface() {
    let tenant = Uuid::new_v4();
    let surface = management_surface(
        None,
        std::sync::Arc::new(oagw::test_support::FakePolicyAuthZ::default()),
        Arc::new(FakeHierarchyTenantResolver::default()),
    )
    .await;
    let (status, bytes) = surface
        .create(tenant, Uuid::new_v4(), pool(&[https("api.vendor.com")]))
        .await;
    assert_eq!(status, 201, "{bytes:?}");
    let record: Value = serde_json::from_slice(&bytes).expect("record");
    let id: Uuid = serde_json::from_value(record["id"].clone()).expect("identifier");

    // Unchanged pool with an omitted alias is accepted.
    let (status, bytes) = surface
        .send(
            http::Method::PUT,
            &format!("/oagw/v1/upstreams/{id}"),
            Some(security_context(tenant, Uuid::new_v4())),
            Some(pool(&[https("api.vendor.com")])),
        )
        .await;
    assert_eq!(status, 200, "{bytes:?}");

    // Unchanged pool with the exact stored alias is accepted as a no-op.
    let mut same = pool(&[https("api.vendor.com")]);
    same["alias"] = json!("api.vendor.com");
    let (status, bytes) = surface
        .send(
            http::Method::PUT,
            &format!("/oagw/v1/upstreams/{id}"),
            Some(security_context(tenant, Uuid::new_v4())),
            Some(same),
        )
        .await;
    assert_eq!(status, 200, "{bytes:?}");

    // A differing alias on an unchanged pool is rejected.
    let mut override_alias = pool(&[https("api.vendor.com")]);
    override_alias["alias"] = json!("somewhere.else");
    let (status, bytes) = surface
        .send(
            http::Method::PUT,
            &format!("/oagw/v1/upstreams/{id}"),
            Some(security_context(tenant, Uuid::new_v4())),
            Some(override_alias),
        )
        .await;
    assert_eq!(status, 400, "{bytes:?}");
    let problem: Value = serde_json::from_slice(&bytes).expect("problem");
    assert!(
        problem["detail"].as_str().unwrap_or("").contains("delete and re-create"),
        "the remediation is named: {problem}"
    );

    // A derivable pool that recomputes a different alias is rejected.
    let mut different_host = pool(&[https("other.vendor.com")]);
    different_host["alias"] = json!("api.vendor.com");
    let (status, bytes) = surface
        .send(
            http::Method::PUT,
            &format!("/oagw/v1/upstreams/{id}"),
            Some(security_context(tenant, Uuid::new_v4())),
            Some(different_host),
        )
        .await;
    assert_eq!(status, 400, "{bytes:?}");
}

/// After a replacement, the record served for the alias carries the new
/// endpoint and not the old one — the observable effect of the write's Control
/// Plane L1 invalidation and Data Plane flush ordering. The pool keeps its
/// registrable suffix, so the derived alias the proxy resolves is unchanged.
#[tokio::test]
async fn the_record_served_after_a_replacement_carries_the_new_endpoint() {
    let tenant = Uuid::new_v4();
    let surface = management_surface(
        None,
        Arc::new(oagw::test_support::FakePolicyAuthZ::default()),
        Arc::new(FakeHierarchyTenantResolver::default()),
    )
    .await;
    let (status, bytes) = surface
        .create(
            tenant,
            Uuid::new_v4(),
            pool(&[https("us.vendor.com"), https("eu.vendor.com")]),
        )
        .await;
    assert_eq!(status, 201, "{bytes:?}");
    let record: Value = serde_json::from_slice(&bytes).expect("record");
    assert_eq!(record["alias"], "vendor.com");
    let id: Uuid = serde_json::from_value(record["id"].clone()).expect("identifier");

    let (status, bytes) = surface
        .send(
            http::Method::PUT,
            &format!("/oagw/v1/upstreams/{id}"),
            Some(security_context(tenant, Uuid::new_v4())),
            Some(pool(&[https("ap.vendor.com"), https("eu.vendor.com")])),
        )
        .await;
    assert_eq!(status, 200, "{bytes:?}");

    let (status, bytes) = surface.get(tenant, Uuid::new_v4(), id).await;
    assert_eq!(status, 200, "{bytes:?}");
    let served: Value = serde_json::from_slice(&bytes).expect("record");
    let hosts: Vec<&str> = served["server"]["endpoints"]
        .as_array()
        .expect("endpoints")
        .iter()
        .filter_map(|endpoint| endpoint["host"].as_str())
        .collect();
    assert!(hosts.contains(&"ap.vendor.com"), "the new endpoint is served: {served}");
    assert!(!hosts.contains(&"us.vendor.com"), "the old endpoint is gone: {served}");
}
