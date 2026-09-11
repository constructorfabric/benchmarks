//! Integration tests for the match-rule uniqueness of one upstream's routes
//! (`cpt-cf-oagw-algo-route-management-uniq`).
//!
//! A `(path, method, priority)` triple for `http` — and a `(service, method)`
//! pair for `grpc` — is unique over the *enabled* routes of one upstream.
// @cpt-dod:cpt-cf-oagw-dod-route-management-uniqueness:p1

#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{Value, json};
use uuid::Uuid;

use oagw::test_support::{permissive_surface, route_body};

const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

fn upstream_body(host: &str) -> Value {
    json!({ "protocol": PROTOCOL_HTTP, "server": { "endpoints": [ { "host": host } ] } })
}

fn http_match(path: &str, methods: &[&str], priority: i64) -> Value {
    json!({ "priority": priority, "match": { "http": { "path": path, "methods": methods } } })
}

/// A second enabled route with the same method, path prefix and priority under
/// the same upstream is a `409`.
#[tokio::test]
async fn a_second_enabled_route_with_the_same_match_keys_conflicts() {
    let tenant = Uuid::new_v4();
    let surface = permissive_surface(None).await;
    let (status, bytes) = surface.create(tenant, Uuid::new_v4(), upstream_body("api.vendor.com")).await;
    assert_eq!(status, 201, "{bytes:?}");
    let parent: Value = serde_json::from_slice(&bytes).expect("created");
    let upstream_id: Uuid = serde_json::from_value(parent["id"].clone()).expect("id");

    for body in [
        http_match("/v1/orders", &["GET"], 0),
        http_match("/v1/orders", &["POST", "DELETE"], 0),
        http_match("/v1/orders", &["GET"], 4),
    ] {
        let mut body = body;
        body["upstream_id"] = json!(upstream_id.to_string());
        let (status, bytes) = surface.create_route(tenant, Uuid::new_v4(), body).await;
        assert_eq!(status, 201, "{bytes:?}");
    }

    let mut collision = http_match("/v1/orders", &["GET"], 0);
    collision["upstream_id"] = json!(upstream_id.to_string());
    let (status, bytes) = surface.create_route(tenant, Uuid::new_v4(), collision).await;
    assert_eq!(status, 409, "{bytes:?}");
    let problem: Value = serde_json::from_slice(&bytes).expect("problem+json");
    assert!(
        problem["title"] == json!("Conflict") && problem["status"] == json!(409),
        "the conflict surface renders: {problem}"
    );
    let counts = surface.gear.storage().expect("storage").row_counts();
    assert_eq!(counts["oagw_route"], 3, "the colliding route was not stored");
}

/// A different method, a different priority or a different upstream is not a
/// collision.
#[tokio::test]
async fn a_difference_in_method_priority_or_upstream_is_not_a_collision() {
    let tenant = Uuid::new_v4();
    let surface = permissive_surface(None).await;
    for alias in ["api.vendor.com", "other.vendor.com"] {
        let (status, bytes) = surface.create(tenant, Uuid::new_v4(), upstream_body(alias)).await;
        assert_eq!(status, 201, "{bytes:?}");
    }
    let (status, bytes) = surface
        .send(
            http::Method::GET,
            "/oagw/v1/upstreams",
            Some(oagw::test_support::security_context(tenant, Uuid::new_v4())),
            None,
        )
        .await;
    assert_eq!(status, 200, "{bytes:?}");
    let listed: Value = serde_json::from_slice(&bytes).expect("list");
    let ids: Vec<Uuid> = listed["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|item| serde_json::from_value(item["id"].clone()).expect("id"))
        .collect();
    assert_eq!(ids.len(), 2, "two upstreams to route over");

    let mut first = http_match("/v1/orders", &["GET"], 0);
    first["upstream_id"] = json!(ids[0].to_string());
    let (status, bytes) = surface.create_route(tenant, Uuid::new_v4(), first).await;
    assert_eq!(status, 201, "{bytes:?}");

    for variant in [
        {
            let mut v = http_match("/v1/orders", &["POST"], 0);
            v["upstream_id"] = json!(ids[0].to_string());
            v
        },
        {
            let mut v = http_match("/v1/orders", &["GET"], 9);
            v["upstream_id"] = json!(ids[0].to_string());
            v
        },
        {
            let mut v = http_match("/v1/orders", &["GET"], 0);
            v["upstream_id"] = json!(ids[1].to_string());
            v
        },
    ] {
        let (status, bytes) = surface.create_route(tenant, Uuid::new_v4(), variant).await;
        assert_eq!(status, 201, "{bytes:?}");
    }
    let counts = surface.gear.storage().expect("storage").row_counts();
    assert_eq!(counts["oagw_route"], 4, "all four routes are stored");
}

/// A replacement that collides with a sibling is a `409`; replacing a route
/// with its own keys is not.
#[tokio::test]
async fn a_replacement_colliding_with_a_sibling_conflicts() {
    let tenant = Uuid::new_v4();
    let surface = permissive_surface(None).await;
    let (status, bytes) = surface.create(tenant, Uuid::new_v4(), upstream_body("api.vendor.com")).await;
    assert_eq!(status, 201, "{bytes:?}");
    let parent: Value = serde_json::from_slice(&bytes).expect("created");
    let upstream_id: Uuid = serde_json::from_value(parent["id"].clone()).expect("id");

    for body in [http_match("/v1/orders", &["GET"], 0), http_match("/v1/billing", &["GET"], 0)] {
        let mut body = body;
        body["upstream_id"] = json!(upstream_id.to_string());
        let (status, bytes) = surface.create_route(tenant, Uuid::new_v4(), body).await;
        assert_eq!(status, 201, "{bytes:?}");
    }
    let (_, bytes) = surface.list_routes(tenant, Uuid::new_v4(), "").await;
    let listed: Value = serde_json::from_slice(&bytes).expect("list");
    let id_of = |path: &str| -> Uuid {
        listed["items"]
            .as_array()
            .expect("items")
            .iter()
            .find(|item| item["match"]["http"]["path"] == json!(path))
            .map(|item| serde_json::from_value(item["id"].clone()).expect("id"))
            .expect("the route of the path")
    };
    assert_eq!(listed["count"], 2);

    // Replacing one route with its own keys is not a collision.
    let orders = id_of("/v1/orders");
    let (status, _) = surface
        .replace_route(tenant, Uuid::new_v4(), orders, http_match("/v1/orders", &["GET"], 0))
        .await;
    assert_eq!(status, 200, "the replacement keeps its own match keys");

    // Taking the sibling's keys is.
    let (status, bytes) = surface
        .replace_route(tenant, Uuid::new_v4(), orders, http_match("/v1/billing", &["GET"], 0))
        .await;
    assert_eq!(status, 409, "{bytes:?}");
}

/// A `grpc` match is unique on `(service, method)` under one upstream, and the
/// two alternatives do not collide with each other.
#[tokio::test]
async fn a_grpc_match_is_unique_on_service_and_method() {
    let tenant = Uuid::new_v4();
    let surface = permissive_surface(None).await;
    let (status, bytes) = surface.create(tenant, Uuid::new_v4(), upstream_body("api.vendor.com")).await;
    assert_eq!(status, 201, "{bytes:?}");
    let parent: Value = serde_json::from_slice(&bytes).expect("created");
    let upstream_id: Uuid = serde_json::from_value(parent["id"].clone()).expect("id");

    let grpc = |service: &str, method: &str| {
        json!({
            "upstream_id": upstream_id.to_string(),
            "match": { "grpc": { "service": service, "method": method } }
        })
    };
    for body in [
        grpc("cf.inventory.v1.Inventory", "Reserve"),
        grpc("cf.inventory.v1.Inventory", "Release"),
        grpc("cf.billing.v1.Billing", "Reserve"),
    ] {
        let (status, bytes) = surface.create_route(tenant, Uuid::new_v4(), body).await;
        assert_eq!(status, 201, "{bytes:?}");
    }
    let (status, bytes) = surface
        .create_route(tenant, Uuid::new_v4(), grpc("cf.inventory.v1.Inventory", "Reserve"))
        .await;
    assert_eq!(status, 409, "{bytes:?}");

    // The alternatives are independent: an `http` route over the same path
    // text is not a `grpc` collision.
    let (status, bytes) = surface
        .create_route(tenant, Uuid::new_v4(), route_body(upstream_id, "/cf.inventory.v1.Inventory"))
        .await;
    assert_eq!(status, 201, "{bytes:?}");
}

/// The comparison runs over the *enabled* routes, so a disabled route's keys
/// are free and a re-enable that collides is a `409`.
#[tokio::test]
async fn the_comparison_ignores_disabled_routes() {
    let tenant = Uuid::new_v4();
    let surface = permissive_surface(None).await;
    let (status, bytes) = surface.create(tenant, Uuid::new_v4(), upstream_body("api.vendor.com")).await;
    assert_eq!(status, 201, "{bytes:?}");
    let parent: Value = serde_json::from_slice(&bytes).expect("created");
    let upstream_id: Uuid = serde_json::from_value(parent["id"].clone()).expect("id");

    // The enabled route holds the keys.
    let (status, bytes) = surface
        .create_route(tenant, Uuid::new_v4(), json!({ "upstream_id": upstream_id.to_string(), "enabled": true, "match": { "http": { "path": "/v1/orders", "methods": ["GET"] } } }))
        .await;
    assert_eq!(status, 201, "{bytes:?}");
    let holder: Value = serde_json::from_slice(&bytes).expect("created");
    let holder_id: Uuid = serde_json::from_value(holder["id"].clone()).expect("id");

    // The disabled route is out of the comparison.
    let (status, bytes) = surface
        .create_route(tenant, Uuid::new_v4(), json!({ "upstream_id": upstream_id.to_string(), "enabled": false, "match": { "http": { "path": "/v1/orders", "methods": ["GET"] } } }))
        .await;
    assert_eq!(status, 201, "a disabled route is not part of the comparison: {bytes:?}");
    let disabled: Value = serde_json::from_slice(&bytes).expect("created");
    let disabled_id: Uuid = serde_json::from_value(disabled["id"].clone()).expect("id");

    // Re-enabling it collides with the enabled holder.
    let (status, bytes) = surface
        .replace_route(tenant, Uuid::new_v4(), disabled_id, http_match("/v1/orders", &["GET"], 0))
        .await;
    assert_eq!(status, 409, "{bytes:?}");

    // Disabling the holder frees the keys again.
    let (status, bytes) = surface
        .replace_route(
            tenant,
            Uuid::new_v4(),
            holder_id,
            json!({ "enabled": false, "match": { "http": { "path": "/v1/orders", "methods": ["GET"] } } }),
        )
        .await;
    assert_eq!(status, 200, "{bytes:?}");
    let (status, bytes) = surface
        .replace_route(tenant, Uuid::new_v4(), disabled_id, http_match("/v1/orders", &["GET"], 0))
        .await;
    assert_eq!(status, 200, "the keys are free once the holder is disabled: {bytes:?}");
}
