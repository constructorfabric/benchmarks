//! Integration tests for the enable and disable semantics over the HTTP
//! surface (`cpt-cf-oagw-dod-upstream-management-enable-disable`,
//! `cpt-cf-oagw-algo-upstream-management-enabled-inheritance`).
//!
//! `enabled` is settable only through a full replacement, there is no dedicated
//! enable or disable endpoint, and an ancestor disablement presents to every
//! descendant and cannot be lifted from below.
// @cpt-dod:cpt-cf-oagw-dod-upstream-management-enable-disable:p1

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

/// A full-replacement body carrying an `enabled` value.
fn pool_with_enabled(host: &str, enabled: bool) -> Value {
    let mut body = pool(host);
    body["enabled"] = json!(enabled);
    body
}

async fn create(surface: &oagw::test_support::ManagementSurface, tenant: Uuid, host: &str) -> Uuid {
    let (status, bytes) = surface.create(tenant, Uuid::new_v4(), pool(host)).await;
    assert_eq!(status, 201, "{bytes:?}");
    let record: Value = serde_json::from_slice(&bytes).expect("record");
    serde_json::from_value(record["id"].clone()).expect("identifier")
}

/// The owning tenant disables through a full replacement, the stored state and
/// the effective presentation agree, and re-enabling restores traffic.
#[tokio::test]
async fn the_owning_tenant_disables_and_re_enables_through_a_replacement() {
    let tenant = Uuid::new_v4();
    let surface = management_surface(
        None,
        Arc::new(FakePolicyAuthZ::default()),
        Arc::new(FakeHierarchyTenantResolver::default()),
    )
    .await;
    let id = create(&surface, tenant, "api.vendor.com").await;

    let (status, bytes) = surface
        .send(
            http::Method::PUT,
            &format!("/oagw/v1/upstreams/{id}"),
            Some(security_context(tenant, Uuid::new_v4())),
            Some(pool_with_enabled("api.vendor.com", false)),
        )
        .await;
    assert_eq!(status, 200, "{bytes:?}");
    let replaced: Value = serde_json::from_slice(&bytes).expect("record");
    assert_eq!(replaced["enabled"], json!(false), "the stored state is disabled");
    assert_eq!(
        replaced["effective_enablement"]["enabled"],
        json!(false),
        "the record presents as disabled"
    );
    assert_eq!(
        replaced["effective_enablement"]["disabling_tenant_id"],
        json!(tenant.to_string()),
        "the record's own disablement names its own tenant and no ancestor: {replaced}"
    );

    let (status, bytes) = surface.get(tenant, Uuid::new_v4(), id).await;
    assert_eq!(status, 200, "{bytes:?}");
    let served: Value = serde_json::from_slice(&bytes).expect("record");
    assert_eq!(served["enabled"], json!(false), "the disabled state is stored");

    // A replacement that omits `enabled` stores the schema default `true`.
    let (status, bytes) = surface
        .send(
            http::Method::PUT,
            &format!("/oagw/v1/upstreams/{id}"),
            Some(security_context(tenant, Uuid::new_v4())),
            Some(pool("api.vendor.com")),
        )
        .await;
    assert_eq!(status, 200, "{bytes:?}");
    let replaced: Value = serde_json::from_slice(&bytes).expect("record");
    assert_eq!(replaced["enabled"], json!(true), "an omitted `enabled` defaults to true");
    assert_eq!(replaced["effective_enablement"]["enabled"], json!(true));
}

/// An ancestor disablement presents as disabled to a descendant, no descendant
/// write lifts it, and it clears when the ancestor re-enables or removes its
/// record.
#[tokio::test]
async fn an_ancestor_disablement_is_inherited_and_cannot_be_lifted_from_below() {
    let parent = Uuid::new_v4();
    let child = Uuid::new_v4();
    let chain = [child, parent];
    let surface = management_surface(
        None,
        Arc::new(FakePolicyAuthZ::default()),
        FakeHierarchyTenantResolver::over(&chain),
    )
    .await;
    let parent_id = create(&surface, parent, "shared.vendor.com").await;
    // The descendant's record under the same alias is the bind case.
    let child_id = create(&surface, child, "shared.vendor.com").await;

    // Before the disablement the descendant's record presents its own state.
    let (status, bytes) = surface.get(child, Uuid::new_v4(), child_id).await;
    assert_eq!(status, 200, "{bytes:?}");
    let served: Value = serde_json::from_slice(&bytes).expect("record");
    assert_eq!(served["effective_enablement"]["enabled"], json!(true));

    // The ancestor disables its own record.
    let (status, _) = surface
        .send(
            http::Method::PUT,
            &format!("/oagw/v1/upstreams/{parent_id}"),
            Some(security_context(parent, Uuid::new_v4())),
            Some(pool_with_enabled("shared.vendor.com", false)),
        )
        .await;
    assert_eq!(status, 200);

    // The descendant now presents `disabled-by-ancestor`, naming the ancestor.
    let (status, bytes) = surface.get(child, Uuid::new_v4(), child_id).await;
    assert_eq!(status, 200, "{bytes:?}");
    let served: Value = serde_json::from_slice(&bytes).expect("record");
    assert_eq!(
        served["effective_enablement"]["enabled"],
        json!(false),
        "the ancestor disablement is inherited: {served}"
    );
    assert_eq!(
        served["effective_enablement"]["disabling_tenant_id"],
        json!(parent.to_string()),
        "the disabling ancestor is named: {served}"
    );

    // A descendant write of its own record does not lift the disablement.
    let (status, bytes) = surface
        .send(
            http::Method::PUT,
            &format!("/oagw/v1/upstreams/{child_id}"),
            Some(security_context(child, Uuid::new_v4())),
            Some(pool_with_enabled("shared.vendor.com", true)),
        )
        .await;
    assert_eq!(status, 200, "{bytes:?}");
    let replaced: Value = serde_json::from_slice(&bytes).expect("record");
    assert_eq!(
        replaced["effective_enablement"]["enabled"],
        json!(false),
        "the ancestor disablement still governs: {replaced}"
    );

    // Addressing the ancestor's record from below is not-found and the
    // disablement stands.
    let (status, bytes) = surface
        .send(
            http::Method::PUT,
            &format!("/oagw/v1/upstreams/{parent_id}"),
            Some(security_context(child, Uuid::new_v4())),
            Some(pool_with_enabled("shared.vendor.com", true)),
        )
        .await;
    assert_eq!(status, 404, "an ancestor record is not-found below: {bytes:?}");
    let (status, bytes) = surface.get(child, Uuid::new_v4(), child_id).await;
    assert_eq!(status, 200, "{bytes:?}");
    let served: Value = serde_json::from_slice(&bytes).expect("record");
    assert_eq!(served["effective_enablement"]["enabled"], json!(false), "nothing was lifted");

    // Re-enabling the ancestor's record clears the derived state.
    let (status, _) = surface
        .send(
            http::Method::PUT,
            &format!("/oagw/v1/upstreams/{parent_id}"),
            Some(security_context(parent, Uuid::new_v4())),
            Some(pool_with_enabled("shared.vendor.com", true)),
        )
        .await;
    assert_eq!(status, 200);
    let (status, bytes) = surface.get(child, Uuid::new_v4(), child_id).await;
    assert_eq!(status, 200, "{bytes:?}");
    let served: Value = serde_json::from_slice(&bytes).expect("record");
    assert_eq!(
        served["effective_enablement"],
        json!({ "enabled": true }),
        "the descendant returns to its own stored state: {served}"
    );

    // Removing the ancestor's record clears it as well.
    let (status, _) = surface
        .send(
            http::Method::PUT,
            &format!("/oagw/v1/upstreams/{parent_id}"),
            Some(security_context(parent, Uuid::new_v4())),
            Some(pool_with_enabled("shared.vendor.com", false)),
        )
        .await;
    assert_eq!(status, 200);
    let (status, _) = surface
        .send(
            http::Method::DELETE,
            &format!("/oagw/v1/upstreams/{parent_id}"),
            Some(security_context(parent, Uuid::new_v4())),
            None,
        )
        .await;
    assert_eq!(status, 204);
    let (status, bytes) = surface.get(child, Uuid::new_v4(), child_id).await;
    assert_eq!(status, 200, "{bytes:?}");
    let served: Value = serde_json::from_slice(&bytes).expect("record");
    assert_eq!(
        served["effective_enablement"],
        json!({ "enabled": true }),
        "the descendant keeps its own stored state: {served}"
    );
}

/// A replacement that disables and also violates a shape is rejected, and the
/// stored record is left unchanged.
#[tokio::test]
async fn a_rejected_replacement_leaves_the_stored_state_unchanged() {
    let tenant = Uuid::new_v4();
    let surface = management_surface(
        Some(json!({ "allow_http_upstream": true })),
        Arc::new(FakePolicyAuthZ::default()),
        Arc::new(FakeHierarchyTenantResolver::default()),
    )
    .await;
    let id = create(&surface, tenant, "api.vendor.com").await;

    let mut mixed = pool_with_enabled("api.vendor.com", false);
    mixed["server"]["endpoints"] =
        json!([{ "host": "api.vendor.com" }, { "scheme": "http", "host": "b.vendor.com", "port": 443 }]);
    let (status, bytes) = surface
        .send(
            http::Method::PUT,
            &format!("/oagw/v1/upstreams/{id}"),
            Some(security_context(tenant, Uuid::new_v4())),
            Some(mixed),
        )
        .await;
    assert_eq!(status, 400, "the disablement does not skip the shape validation: {bytes:?}");

    let (status, bytes) = surface.get(tenant, Uuid::new_v4(), id).await;
    assert_eq!(status, 200, "{bytes:?}");
    let served: Value = serde_json::from_slice(&bytes).expect("record");
    assert_eq!(served["enabled"], json!(true), "the stored state is unchanged: {served}");
    assert_eq!(
        served["effective_enablement"]["enabled"],
        json!(true),
        "the record still presents as enabled"
    );
}

/// No dedicated enable or disable endpoint exists: `enabled` is reachable only
/// through the full replacement.
#[tokio::test]
async fn there_is_no_dedicated_enable_or_disable_endpoint() {
    let tenant = Uuid::new_v4();
    let surface = management_surface(
        None,
        Arc::new(FakePolicyAuthZ::default()),
        Arc::new(FakeHierarchyTenantResolver::default()),
    )
    .await;
    let id = create(&surface, tenant, "api.vendor.com").await;

    for path in [
        format!("/oagw/v1/upstreams/{id}/enable"),
        format!("/oagw/v1/upstreams/{id}/disable"),
    ] {
        for method in [http::Method::POST, http::Method::PUT, http::Method::PATCH] {
            let (status, bytes) = surface
                .send(method.clone(), &path, Some(security_context(tenant, Uuid::new_v4())), Some(json!({})))
                .await;
            assert!(
                status == http::StatusCode::NOT_FOUND || status == http::StatusCode::METHOD_NOT_ALLOWED,
                "{method} {path} is not a route on the tree: {status} {bytes:?}"
            );
        }
    }
    let counts = surface.gear.storage().expect("storage").row_counts();
    assert_eq!(counts["oagw_upstream"], 1, "no stray endpoint stored anything");
}
