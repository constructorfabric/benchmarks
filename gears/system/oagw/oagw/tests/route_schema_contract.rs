//! Integration tests for the route payload contract
//! (`cpt-cf-oagw-dod-route-management-schema-conformance`,
//! `cpt-cf-oagw-dod-route-management-route-overrides`,
//! `cpt-cf-oagw-dod-route-management-enable-disable`).
//!
//! The set of schema-external API fields is closed, `upstream_id` is immutable,
//! every `plugins.items[]` entry is resolved at binding time, and the
//! materialized defaults apply.
// @cpt-dod:cpt-cf-oagw-dod-route-management-match-block:p1
// @cpt-dod:cpt-cf-oagw-dod-route-management-path-and-query:p1
// @cpt-dod:cpt-cf-oagw-dod-route-management-schema-conformance:p1
// @cpt-dod:cpt-cf-oagw-dod-route-management-unit-tests:p1

#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{Value, json};
use uuid::Uuid;

use oagw::test_support::{permissive_surface, route_body};

const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
const NOOP_AUTH_PLUGIN: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1";
const BASIC_AUTH_PLUGIN: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1";
const CORS_GUARD_PLUGIN: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1";
const TIMEOUT_GUARD_PLUGIN: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1";

fn upstream_body(host: &str) -> Value {
    json!({ "protocol": PROTOCOL_HTTP, "server": { "endpoints": [ { "host": host } ] } })
}

fn http_match(path: &str, methods: &[&str]) -> Value {
    json!({ "match": { "http": { "path": path, "methods": methods } } })
}

/// Seed one upstream for `tenant`, returning its identifier.
async fn seed_upstream(surface: &oagw::test_support::ManagementSurface, tenant: Uuid) -> Uuid {
    let (status, bytes) = surface
        .create(tenant, Uuid::new_v4(), upstream_body("api.vendor.com"))
        .await;
    assert_eq!(status, 201, "{bytes:?}");
    let parent: Value = serde_json::from_slice(&bytes).expect("created");
    serde_json::from_value(parent["id"].clone()).expect("the upstream identifier")
}

/// An unknown property — including the server-assigned and immutable fields —
/// is a `400` naming it.
#[tokio::test]
async fn an_unknown_property_is_rejected() {
    let tenant = Uuid::new_v4();
    let surface = permissive_surface(None).await;
    let upstream_id = seed_upstream(&surface, tenant).await;

    for field in [
        "id",
        "tenant_id",
        "match_type",
        "aliases",
        "credential_ref",
        "cred_ref",
    ] {
        let mut body = route_body(upstream_id, "/v1/orders");
        let value = match field {
            "match_type" => json!("http"),
            "aliases" | "credential_ref" | "cred_ref" => json!(["x"]),
            other => json!(json!(other).to_string()),
        };
        body[field] = value;
        let (status, bytes) = surface.create_route(tenant, Uuid::new_v4(), body).await;
        assert_eq!(status, 400, "{field} {bytes:?}");
        let problem: Value = serde_json::from_slice(&bytes).expect("problem+json");
        assert!(
            problem["detail"].as_str().is_some_and(|detail| detail.contains(field)),
            "the rejection names the offending field `{field}`: {problem}"
        );
    }
}

/// A match block with no alternative, with both, or with a shape violation is
/// a `400`.
#[tokio::test]
async fn the_match_block_contract_is_enforced() {
    let tenant = Uuid::new_v4();
    let surface = permissive_surface(None).await;
    let upstream_id = seed_upstream(&surface, tenant).await;

    for (name, body) in [
        ("no alternative", json!({ "upstream_id": upstream_id.to_string(), "match": {} })),
        (
            "both alternatives",
            json!({ "upstream_id": upstream_id.to_string(), "match": { "http": { "path": "/v1", "methods": ["GET"] }, "grpc": { "service": "s.v1.Service", "method": "M" } } }),
        ),
        (
            "an http block without methods",
            json!({ "upstream_id": upstream_id.to_string(), "match": { "http": { "path": "/v1" } } }),
        ),
        (
            "an http block with an empty method list",
            json!({ "upstream_id": upstream_id.to_string(), "match": { "http": { "path": "/v1", "methods": [] } } }),
        ),
        (
            "an http block with an unknown method",
            json!({ "upstream_id": upstream_id.to_string(), "match": { "http": { "path": "/v1", "methods": ["HEAD"] } } }),
        ),
        (
            "an http block without a path",
            json!({ "upstream_id": upstream_id.to_string(), "match": { "http": { "methods": ["GET"] } } }),
        ),
        (
            "a grpc block without a method",
            json!({ "upstream_id": upstream_id.to_string(), "match": { "grpc": { "service": "s.v1.Service" } } }),
        ),
        (
            "a method outside the declared set",
            json!({ "upstream_id": upstream_id.to_string(), "match": { "http": { "path": "/v1", "methods": ["GET"] }, "query_allowlist": ["x"] } }),
        ),
    ] {
        let (status, bytes) = surface.create_route(tenant, Uuid::new_v4(), body).await;
        assert_eq!(status, 400, "{name}: {bytes:?}");
    }
    let counts = surface.gear.storage().expect("storage").row_counts();
    assert_eq!(counts["oagw_route"], 0, "no route was stored for a rejected match block");
}

/// `priority` and `enabled` materialize their declared defaults, and the
/// declared `path_suffix_mode` default is carried.
#[tokio::test]
async fn the_declared_defaults_are_materialized() {
    let tenant = Uuid::new_v4();
    let surface = permissive_surface(None).await;
    let upstream_id = seed_upstream(&surface, tenant).await;

    let (status, bytes) = surface.create_route(tenant, Uuid::new_v4(), route_body(upstream_id, "/v1/orders")).await;
    assert_eq!(status, 201, "{bytes:?}");
    let created: Value = serde_json::from_slice(&bytes).expect("created");
    assert_eq!(created["priority"], 0);
    assert_eq!(created["enabled"], json!(true));

    // A supplied `priority` and `enabled` are stored as given.
    let (status, bytes) = surface
        .create_route(
            tenant,
            Uuid::new_v4(),
            json!({ "upstream_id": upstream_id.to_string(), "priority": 12, "enabled": false, "match": { "http": { "path": "/v2/billing", "methods": ["GET"], "path_suffix_mode": "disabled" } } }),
        )
        .await;
    assert_eq!(status, 201, "{bytes:?}");
    let stored: Value = serde_json::from_slice(&bytes).expect("created");
    assert_eq!(stored["priority"], 12);
    assert_eq!(stored["enabled"], json!(false));
    assert_eq!(stored["match"]["http"]["path_suffix_mode"], json!("disabled"));
}

/// A replacement body that supplies `upstream_id` is rejected as an
/// immutable-field violation, whatever value it carries.
#[tokio::test]
async fn a_replacement_supplying_the_upstream_reference_is_rejected() {
    let tenant = Uuid::new_v4();
    let surface = permissive_surface(None).await;
    let upstream_id = seed_upstream(&surface, tenant).await;
    let (status, bytes) = surface.create_route(tenant, Uuid::new_v4(), route_body(upstream_id, "/v1/orders")).await;
    let created: Value = serde_json::from_slice(&bytes).expect("created");
    let id: Uuid = serde_json::from_value(created["id"].clone()).expect("id");
    assert_eq!(status, 201);

    for value in [upstream_id.to_string(), Uuid::new_v4().to_string()] {
        let mut body = http_match("/v1/orders", &["GET"]);
        body["upstream_id"] = json!(value);
        let (status, bytes) = surface.replace_route(tenant, Uuid::new_v4(), id, body).await;
        assert_eq!(status, 400, "{bytes:?}");
        let problem: Value = serde_json::from_slice(&bytes).expect("problem+json");
        assert!(
            problem["detail"].as_str().is_some_and(|detail| detail.contains("upstream_id")),
            "the rejection names the immutable field: {problem}"
        );
    }

    // The stored record is untouched and the reference is unchanged.
    let (status, bytes) = surface.get_route(tenant, Uuid::new_v4(), id).await;
    assert_eq!(status, 200, "{bytes:?}");
    let read: Value = serde_json::from_slice(&bytes).expect("read");
    assert_eq!(read["upstream_id"], json!(upstream_id.to_string()));
}

/// Every `plugins.items[]` entry is resolved at binding time: a built-in
/// identifier resolves, a catalog-only one is a `400`, and no interim
/// unresolved state is ever stored.
#[tokio::test]
async fn the_plugin_references_are_resolved_at_binding_time() {
    let tenant = Uuid::new_v4();
    let surface = permissive_surface(None).await;
    let upstream_id = seed_upstream(&surface, tenant).await;

    let mut body = route_body(upstream_id, "/v1/orders");
    body["plugins"] = json!({ "sharing": "inherit", "items": [NOOP_AUTH_PLUGIN] });
    let (status, bytes) = surface.create_route(tenant, Uuid::new_v4(), body).await;
    assert_eq!(status, 201, "{bytes:?}");
    let created: Value = serde_json::from_slice(&bytes).expect("created");
    assert_eq!(created["plugins"]["items"], json!([NOOP_AUTH_PLUGIN]), "the stored references");

    for reference in [BASIC_AUTH_PLUGIN, CORS_GUARD_PLUGIN, TIMEOUT_GUARD_PLUGIN, "not-an-identifier"] {
        let mut body = route_body(upstream_id, "/v1/resolved");
        body["plugins"] = json!({ "sharing": "inherit", "items": [NOOP_AUTH_PLUGIN, reference] });
        let (status, bytes) = surface.create_route(tenant, Uuid::new_v4(), body).await;
        assert_eq!(status, 400, "{reference} {bytes:?}");
        let problem: Value = serde_json::from_slice(&bytes).expect("problem+json");
        assert!(
            problem["detail"].as_str().is_some_and(|detail| detail.contains("plugins.items")),
            "the binding-time rejection names the offending entry: {problem}"
        );
    }

    let counts = surface.gear.storage().expect("storage").row_counts();
    assert_eq!(counts["oagw_route"], 1, "no interim unresolved-binding state is stored");
    assert_eq!(counts["oagw_route_plugin"], 1, "exactly one resolved binding row");
}

/// The route payload carries no credential reference: the credential boundary
/// of the upstream surface is not reachable from a route write.
#[tokio::test]
async fn a_route_payload_carries_no_credential_reference() {
    let tenant = Uuid::new_v4();
    let surface = permissive_surface(None).await;
    let upstream_id = seed_upstream(&surface, tenant).await;

    let mut body = route_body(upstream_id, "/v1/orders");
    body["auth"] = json!({ "sharing": "private", "auth_type": NOOP_AUTH_PLUGIN, "config": { "api_key_ref": "cred://t/k" } });
    let (status, bytes) = surface.create_route(tenant, Uuid::new_v4(), body).await;
    assert_eq!(status, 400, "{bytes:?}");

    let mut body = route_body(upstream_id, "/v1/orders");
    body["plugins"] = json!({ "sharing": "private", "items": [BASIC_AUTH_PLUGIN] });
    let (status, bytes) = surface.create_route(tenant, Uuid::new_v4(), body).await;
    assert_eq!(status, 400, "a catalog-only auth reference is not bindable: {bytes:?}");
    let counts = surface.gear.storage().expect("storage").row_counts();
    assert_eq!(counts["oagw_route"], 0, "neither rejected body stored a route");
    assert_eq!(counts["oagw_route_plugin"], 0, "no binding row was created");
}

/// Omitting `enabled` on a replacement re-enables a disabled route, and the
/// omitted override blocks are cleared.
#[tokio::test]
async fn the_replacement_defaults_and_clears_the_overrides() {
    let tenant = Uuid::new_v4();
    let surface = permissive_surface(None).await;
    let upstream_id = seed_upstream(&surface, tenant).await;

    let mut body = route_body(upstream_id, "/v1/orders");
    body["enabled"] = json!(false);
    body["tags"] = json!(["beta"]);
    body["rate_limit"] = json!({ "sustained": { "rate": 30 } });
    let (status, bytes) = surface.create_route(tenant, Uuid::new_v4(), body).await;
    assert_eq!(status, 201, "{bytes:?}");
    let created: Value = serde_json::from_slice(&bytes).expect("created");
    let id: Uuid = serde_json::from_value(created["id"].clone()).expect("id");
    assert_eq!(created["enabled"], json!(false));

    let (status, bytes) = surface
        .replace_route(
            tenant,
            Uuid::new_v4(),
            id,
            json!({ "priority": 5, "match": { "http": { "path": "/v1/orders", "methods": ["GET"] } } }),
        )
        .await;
    assert_eq!(status, 200, "{bytes:?}");
    let replaced: Value = serde_json::from_slice(&bytes).expect("replaced");
    assert_eq!(replaced["enabled"], json!(true), "the omitted enablement re-enables");
    assert_eq!(replaced["priority"], 5, "the supplied priority is stored");
    assert_eq!(replaced["tags"], json!([]), "the omitted tag list is cleared");
    assert!(replaced.get("rate_limit").is_none(), "the omitted override is cleared");
}
