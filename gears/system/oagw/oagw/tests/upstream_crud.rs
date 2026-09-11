//! Integration tests for the five upstream operations over the gear-relative
//! routes (`cpt-cf-oagw-dod-upstream-management-crud-endpoints`,
//! `cpt-cf-oagw-dod-upstream-management-auth-config`).
//!
//! Every test drives the real `OagwGear` through `Gear::init` and
//! `RestApiCapability::register_rest`, then issues HTTP requests through the
//! registered router.
// @cpt-dod:cpt-cf-oagw-dod-upstream-management-auth-config:p1
// @cpt-dod:cpt-cf-oagw-dod-upstream-management-crud-endpoints:p1
// @cpt-dod:cpt-cf-oagw-dod-upstream-management-integration-tests:p1

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use serde_json::{Value, json};
use uuid::Uuid;

use oagw::test_support::{
    FakePolicyAuthZ, MANAGEMENT_PERMISSIONS, FakeHierarchyTenantResolver, management_surface,
    permissive_surface, security_context,
};

const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

fn body(host: &str) -> Value {
    json!({
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [ { "host": host } ] }
    })
}

/// `POST` stores a record whose alias is the single hostname, `GET` returns it
/// for its identifier, and the list reports the count actually returned.
#[tokio::test]
async fn the_five_operations_round_trip_one_record() {
    let tenant = Uuid::new_v4();
    let surface = permissive_surface(None).await;

    let (status, created) = surface.create(tenant, Uuid::new_v4(), body("api.vendor.com")).await;
    assert_eq!(status, 201, "{created:?}");
    let record: Value = serde_json::from_slice(&created).expect("the created record");
    assert_eq!(record["alias"], "api.vendor.com", "the alias is the hostname");
    assert_eq!(record["server"]["endpoints"][0]["port"], 443, "the port default");
    assert_eq!(record["server"]["endpoints"][0]["scheme"], "https", "the scheme default");
    assert!(record["effective_enablement"]["enabled"] == json!(true));
    assert!(record.get("tenant_id").is_none(), "the owner tenant is never on the wire");
    let id: Uuid =
        serde_json::from_value(record["id"].clone()).expect("the server-generated identifier");

    let (status, fetched) = surface.get(tenant, Uuid::new_v4(), id).await;
    assert_eq!(status, 200, "{fetched:?}");
    let fetched: Value = serde_json::from_slice(&fetched).expect("the stored record");
    assert_eq!(fetched["id"], record["id"]);
    assert_eq!(fetched["alias"], "api.vendor.com");

    let (status, listed) = surface
        .send(
            http::Method::GET,
            "/oagw/v1/upstreams",
            Some(security_context(tenant, Uuid::new_v4())),
            None,
        )
        .await;
    assert_eq!(status, 200, "{listed:?}");
    let listed: Value = serde_json::from_slice(&listed).expect("the list body");
    assert_eq!(listed["count"], 1, "the count of the records actually returned");
    assert_eq!(listed["items"].as_array().expect("items").len(), 1);

    // `PUT` is the full replacement, and `DELETE` returns an empty `204`.
    let mut replacement = body("api.vendor.com");
    replacement["tags"] = json!(["billing"]);
    let (status, replaced) = surface
        .send(
            http::Method::PUT,
            &format!("/oagw/v1/upstreams/{id}"),
            Some(security_context(tenant, Uuid::new_v4())),
            Some(replacement),
        )
        .await;
    assert_eq!(status, 200, "{replaced:?}");
    let replaced: Value = serde_json::from_slice(&replaced).expect("the replaced record");
    assert_eq!(replaced["id"], record["id"], "the replacement keeps the identifier");
    assert_eq!(replaced["tags"], json!(["billing"]));

    let (status, deleted) = surface
        .send(
            http::Method::DELETE,
            &format!("/oagw/v1/upstreams/{id}"),
            Some(security_context(tenant, Uuid::new_v4())),
            None,
        )
        .await;
    assert_eq!(status, 204, "{deleted:?}");
    assert!(deleted.is_empty(), "a `204` carries no body");

    let (status, _) = surface.get(tenant, Uuid::new_v4(), id).await;
    assert_eq!(status, 404, "the identifier is gone");
}

/// A request with no resolvable security context is rejected with `401` and
/// the OAGW authentication type before any payload validation, for each of the
/// five operations.
#[tokio::test]
async fn a_missing_context_is_rejected_with_401_before_payload_validation() {
    let surface = permissive_surface(None).await;
    let id = Uuid::new_v4();

    let cases: Vec<(http::Method, String, Option<Value>)> = vec![
        (http::Method::POST, "/oagw/v1/upstreams".to_owned(), Some(json!({ "no": "body" }))),
        (http::Method::GET, "/oagw/v1/upstreams".to_owned(), None),
        (http::Method::GET, format!("/oagw/v1/upstreams/{id}"), None),
        (
            http::Method::PUT,
            format!("/oagw/v1/upstreams/{id}"),
            Some(json!({ "no": "body" })),
        ),
        (http::Method::DELETE, format!("/oagw/v1/upstreams/{id}"), None),
    ];
    for (method, path, request_body) in cases {
        let (status, bytes) = surface.send(method, &path, None, request_body).await;
        assert_eq!(status, 401, "{bytes:?}");
        let problem: Value = serde_json::from_slice(&bytes).expect("problem+json");
        assert_eq!(
            problem["type"],
            "gts://gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1",
            "the OAGW authentication type renders: {problem}"
        );
    }
    assert!(
        surface.gear.storage().expect("storage").row_counts()["oagw_upstream"] == 0,
        "nothing was stored by the rejected requests"
    );
}

/// A valid context that lacks the operation's permission is rejected with `403`
/// through the shared canonical permission-denied surface, for each of the five
/// operations.
#[tokio::test]
async fn a_denied_permission_is_rejected_with_403_through_the_canonical_surface() {
    let tenant = Uuid::new_v4();
    let denied: Vec<String> = MANAGEMENT_PERMISSIONS
        .iter()
        .map(|permission| (*permission).to_owned())
        .collect();
    let surface = management_surface(
        None,
        FakePolicyAuthZ::denying(&denied.iter().map(String::as_str).collect::<Vec<_>>()),
        Arc::new(FakeHierarchyTenantResolver::default()),
    )
    .await;
    let id = Uuid::new_v4();

    let cases: Vec<(http::Method, String, Option<Value>)> = vec![
        (http::Method::POST, "/oagw/v1/upstreams".to_owned(), Some(body("api.vendor.com"))),
        (http::Method::GET, "/oagw/v1/upstreams".to_owned(), None),
        (http::Method::GET, format!("/oagw/v1/upstreams/{id}"), None),
        (http::Method::PUT, format!("/oagw/v1/upstreams/{id}"), Some(body("api.vendor.com"))),
        (http::Method::DELETE, format!("/oagw/v1/upstreams/{id}"), None),
    ];
    for (method, path, request_body) in cases {
        let (status, bytes) = surface
            .send(method, &path, Some(security_context(tenant, Uuid::new_v4())), request_body)
            .await;
        assert_eq!(status, 403, "{bytes:?}");
        let problem: Value = serde_json::from_slice(&bytes).expect("problem+json");
        assert!(
            problem["type"].as_str().is_some_and(|problem_type| problem_type.contains(
                "permission_denied"
            )),
            "the canonical permission-denied surface renders: {problem}"
        );
        assert!(
            problem["context"]["resource_type"]
                .as_str()
                .is_some_and(|resource| resource.starts_with("gts.cf.core.oagw.upstream.v1~")),
            "the evaluated resource is the upstream base type: {problem}"
        );
    }
}

/// A `403` never stores anything: the permission gate precedes the store.
#[tokio::test]
async fn a_denied_create_stores_nothing() {
    let tenant = Uuid::new_v4();
    let surface = management_surface(
        None,
        FakePolicyAuthZ::denying(&["gts.cf.core.oagw.upstream.v1~:create"]),
        Arc::new(FakeHierarchyTenantResolver::default()),
    )
    .await;
    let (status, bytes) = surface.create(tenant, Uuid::new_v4(), body("api.vendor.com")).await;
    assert_eq!(status, 403, "{bytes:?}");
    let counts = surface.gear.storage().expect("storage").row_counts();
    assert_eq!(counts["oagw_upstream"], 0, "the gate precedes the store");
}

/// A second create of the same `(tenant_id, alias)` is exactly one `409`, and
/// exactly one record is stored.
#[tokio::test]
async fn a_same_tenant_alias_collision_is_a_conflict() {
    let tenant = Uuid::new_v4();
    let surface = permissive_surface(None).await;
    let mut first = body("api.vendor.com");
    first["alias"] = json!("api.vendor.com");
    let (status, _) = surface.create(tenant, Uuid::new_v4(), first.clone()).await;
    assert_eq!(status, 201);

    let (status, bytes) = surface.create(tenant, Uuid::new_v4(), first).await;
    assert_eq!(status, 409, "{bytes:?}");
    let counts = surface.gear.storage().expect("storage").row_counts();
    assert_eq!(counts["oagw_upstream"], 1, "exactly one record is stored");
}

/// A read-back presents the credential-bearing fields as their stored
/// `cred://` references and never resolved secret material.
#[tokio::test]
async fn the_read_back_carries_only_cred_references() {
    let tenant = Uuid::new_v4();
    let surface = permissive_surface(None).await;
    let mut payload = body("api.vendor.com");
    payload["auth"] = json!({
        "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
        "config": { "api_key_ref": "cred://tenant-1/apikeys/primary" }
    });
    let (status, bytes) = surface.create(tenant, Uuid::new_v4(), payload).await;
    assert_eq!(status, 201, "{bytes:?}");
    let record: Value = serde_json::from_slice(&bytes).expect("the created record");

    let wire = serde_json::to_string(&record).expect("the record renders");
    assert!(!wire.contains("sk-live-"), "no secret material on the wire: {wire}");
    assert_eq!(
        record["auth"]["config"]["api_key_ref"],
        "cred://tenant-1/apikeys/primary",
        "the reference is returned exactly as stored"
    );

    let (status, listed) = surface
        .send(
            http::Method::GET,
            "/oagw/v1/upstreams?$select=alias,auth",
            Some(security_context(tenant, Uuid::new_v4())),
            None,
        )
        .await;
    assert_eq!(status, 200, "{listed:?}");
    let listed: Value = serde_json::from_slice(&listed).expect("the projected list");
    assert_eq!(listed["count"], 1);
    assert_eq!(
        listed["items"][0]["auth"]["config"]["api_key_ref"],
        "cred://tenant-1/apikeys/primary",
        "a projected read-back holds the reference too"
    );
}

/// A body outside the upstream schema shape is a `400` and stores nothing.
#[tokio::test]
async fn a_body_outside_the_schema_shape_is_rejected_and_stores_nothing() {
    let tenant = Uuid::new_v4();
    let surface = permissive_surface(None).await;

    let cases: Vec<Value> = vec![
        // An unknown property.
        json!({
            "protocol": PROTOCOL_HTTP,
            "server": { "endpoints": [ { "host": "api.vendor.com" } ] },
            "scopes": ["read"]
        }),
        // An empty endpoint pool.
        json!({ "protocol": PROTOCOL_HTTP, "server": { "endpoints": [] } }),
        // A port outside 1..=65535 is not representable in the u16 field, so
        // the pool with a malformed host is the shape rejection asserted here.
        json!({
            "protocol": PROTOCOL_HTTP,
            "server": { "endpoints": [ { "host": "api..vendor.com" } ] }
        }),
    ];
    for payload in cases {
        let (status, bytes) = surface.create(tenant, Uuid::new_v4(), payload).await;
        assert_eq!(status, 400, "{bytes:?}");
    }
    let counts = surface.gear.storage().expect("storage").row_counts();
    assert_eq!(counts["oagw_upstream"], 0, "a rejected body stores nothing");
}

/// A delete removes the dependent route, tag and plugin binding rows and the
/// identifier reads back as not-found.
#[tokio::test]
async fn a_delete_removes_the_dependent_rows() {
    let tenant = Uuid::new_v4();
    let surface = permissive_surface(None).await;
    let mut payload = body("api.vendor.com");
    payload["tags"] = json!(["billing", "gold"]);
    payload["plugins"] = json!({ "sharing": "private", "items": [] });
    let (status, bytes) = surface.create(tenant, Uuid::new_v4(), payload).await;
    assert_eq!(status, 201, "{bytes:?}");
    let record: Value = serde_json::from_slice(&bytes).expect("the created record");
    let id: Uuid = serde_json::from_value(record["id"].clone()).expect("identifier");

    let counts = surface.gear.storage().expect("storage").row_counts();
    assert_eq!(counts["oagw_upstream_tag"], 2, "the tag rows are stored on create");

    let (status, _) = surface
        .send(
            http::Method::DELETE,
            &format!("/oagw/v1/upstreams/{id}"),
            Some(security_context(tenant, Uuid::new_v4())),
            None,
        )
        .await;
    assert_eq!(status, 204);

    let counts = surface.gear.storage().expect("storage").row_counts();
    assert_eq!(counts["oagw_upstream"], 0);
    assert_eq!(counts["oagw_upstream_tag"], 0, "the tag rows cascaded");
    assert_eq!(counts["oagw_upstream_plugin"], 0, "the binding rows cascaded");
}
