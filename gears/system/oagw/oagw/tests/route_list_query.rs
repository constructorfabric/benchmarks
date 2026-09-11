//! Integration tests for the OData list query over `/oagw/v1/routes`
//! (`cpt-cf-oagw-algo-route-management-lq`, `inst-rm-list-4` .. `-7`).
//!
//! The parser is the one entry 2.2 delivered; only the field set differs.
// @cpt-dod:cpt-cf-oagw-dod-route-management-list-query:p1

#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{Value, json};
use uuid::Uuid;

use oagw::test_support::permissive_surface;

const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

fn upstream_body(host: &str) -> Value {
    json!({ "protocol": PROTOCOL_HTTP, "server": { "endpoints": [ { "host": host } ] } })
}

fn http_match(path: &str, priority: i64) -> Value {
    json!({ "priority": priority, "match": { "http": { "path": path, "methods": ["GET"] } } })
}

/// Seed one upstream and three routes with distinct paths and priorities.
async fn seed(surface: &oagw::test_support::ManagementSurface, tenant: Uuid) -> Uuid {
    let (status, bytes) = surface.create(tenant, Uuid::new_v4(), upstream_body("api.vendor.com")).await;
    assert_eq!(status, 201, "{bytes:?}");
    let parent: Value = serde_json::from_slice(&bytes).expect("created");
    let upstream_id: Uuid = serde_json::from_value(parent["id"].clone()).expect("id");

    for (path, priority) in [("/v1/orders", 3), ("/v1/billing", 1), ("/v2/reports", 2)] {
        let mut body = http_match(path, priority);
        body["upstream_id"] = json!(upstream_id.to_string());
        let (status, bytes) = surface.create_route(tenant, Uuid::new_v4(), body).await;
        assert_eq!(status, 201, "{bytes:?}");
    }
    upstream_id
}

/// `$filter`, `$orderby`, `$top` and `$skip` apply in that sequence, and the
/// count is the count of the records actually returned.
#[tokio::test]
async fn the_list_applies_filter_ordering_top_and_skip() {
    let tenant = Uuid::new_v4();
    let surface = permissive_surface(None).await;
    let upstream_id = seed(&surface, tenant).await;

    let filter = format!("upstream_id%20eq%20%27{upstream_id}%27");
    let (status, bytes) = surface
        .list_routes(tenant, Uuid::new_v4(), &format!("$filter={filter}&$orderby=priority%20desc&$top=2&$skip=1"))
        .await;
    assert_eq!(status, 200, "{bytes:?}");
    let listed: Value = serde_json::from_slice(&bytes).expect("list");
    assert_eq!(listed["count"], 2, "the count of the records actually returned: {listed}");
    let paths: Vec<&str> = listed["items"]
        .as_array()
        .expect("items")
        .iter()
        .filter_map(|item| item["match"]["http"]["path"].as_str())
        .collect();
    assert_eq!(paths, vec!["/v2/reports", "/v1/billing"], "highest priority first: {listed}");
    for item in listed["items"].as_array().expect("items") {
        assert_eq!(item["upstream_id"], json!(upstream_id.to_string()), "the filter selected");
    }
}

/// `$select` projects the fields it names and nothing else.
#[tokio::test]
async fn the_select_projection_carries_only_the_selected_fields() {
    let tenant = Uuid::new_v4();
    let surface = permissive_surface(None).await;
    seed(&surface, tenant).await;

    let (status, bytes) = surface
        .list_routes(tenant, Uuid::new_v4(), "$select=id,enabled,priority")
        .await;
    assert_eq!(status, 200, "{bytes:?}");
    let listed: Value = serde_json::from_slice(&bytes).expect("list");
    let items = listed["items"].as_array().expect("items");
    assert_eq!(items.len(), 3);
    for item in items {
        assert!(item.get("match").is_none(), "an unselected block is projected away: {item}");
        assert!(item.get("upstream_id").is_none(), "an unselected field is projected away: {item}");
        assert!(item.get("priority").is_some_and(|value| value.is_i64()), "a selected field is carried: {item}");
        assert!(item["enabled"].is_boolean(), "a selected field is carried: {item}");
    }
    assert_eq!(listed["count"], 3, "the count is unaffected by the projection");
}

/// A bare list returns every own route with its overrides, in the order the
/// query produced.
#[tokio::test]
async fn a_bare_list_returns_every_route_with_its_overrides() {
    let tenant = Uuid::new_v4();
    let surface = permissive_surface(None).await;
    let upstream_id = seed(&surface, tenant).await;
    let (status, bytes) = surface.create_route(
        tenant,
        Uuid::new_v4(),
        json!({ "upstream_id": upstream_id.to_string(), "tags": ["beta"], "rate_limit": { "sustained": { "rate": 30 } }, "match": { "http": { "path": "/v3/cache", "methods": ["GET", "POST"] } } }),
    ).await;
    assert_eq!(status, 201, "{bytes:?}");

    let (status, bytes) = surface.list_routes(tenant, Uuid::new_v4(), "").await;
    assert_eq!(status, 200, "{bytes:?}");
    let listed: Value = serde_json::from_slice(&bytes).expect("list");
    assert_eq!(listed["count"], 4, "every own route is returned");
    let tagged = listed["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|item| item["match"]["http"]["path"] == json!("/v3/cache"))
        .expect("the route with overrides");
    assert_eq!(tagged["tags"], json!(["beta"]), "the stored tags are carried");
    assert!(tagged["rate_limit"]["sustained"]["rate"] == json!(30), "the stored override is carried");
    assert_eq!(tagged["match"]["http"]["methods"], json!(["GET", "POST"]), "the match block is carried");
}

/// An unsupported system query option, a foreign field name and an
/// out-of-range `$top` are rejections naming the parameter.
#[tokio::test]
async fn an_unsupported_or_out_of_range_parameter_is_rejected() {
    let tenant = Uuid::new_v4();
    let surface = permissive_surface(None).await;
    seed(&surface, tenant).await;

    for (query, parameter) in [
        ("$count=1", "$count"),
        ("$orderby=alias", "$orderby"),
        ("$top=1000", "$top"),
        ("$skip=-1", "$skip"),
        ("$filter=alias%20eq%20%27x%27", "$filter"),
    ] {
        let (status, bytes) = surface.list_routes(tenant, Uuid::new_v4(), query).await;
        assert_eq!(status, 400, "{query} {bytes:?}");
        let problem: Value = serde_json::from_slice(&bytes).expect("problem+json");
        assert!(
            problem["detail"].as_str().is_some_and(|detail| detail.contains(parameter)),
            "the rejection names the parameter `{parameter}`: {problem}"
        );
    }
}

/// An empty result is a `200` with `count` `0` and an empty list.
#[tokio::test]
async fn an_empty_result_is_count_zero_with_an_empty_list() {
    let tenant = Uuid::new_v4();
    let surface = permissive_surface(None).await;
    let (status, bytes) = surface.list_routes(tenant, Uuid::new_v4(), "").await;
    assert_eq!(status, 200, "{bytes:?}");
    let listed: Value = serde_json::from_slice(&bytes).expect("list");
    assert_eq!(listed["count"], 0);
    assert_eq!(listed["items"], json!([]));
}
