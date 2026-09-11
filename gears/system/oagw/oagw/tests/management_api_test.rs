//! Management API integration tests: the CRUD surface a tenant administrator
//! drives, over the gear's own router.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use axum::http::{StatusCode, header};
use common::Harness;
use httpmock::prelude::MockServer;
use serde_json::{Value, json};

/// Seeds an upstream whose alias is explicitly chosen.
async fn seed_named_upstream(harness: &Harness, alias: &str) -> Value {
    let (status, body) = harness
        .json(
            "POST",
            "/oagw/v1/upstreams",
            Some(json!({
                "alias": alias,
                "protocol": oagw::gts_helpers::PROTOCOL_HTTP,
                "server": {"endpoints": [
                    {"scheme": "http", "host": "10.0.0.9", "port": 9000}
                ]}
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "seed: {body}");
    body
}

#[tokio::test]
async fn create_returns_created_with_location_and_derived_alias() {
    let server = MockServer::start();
    let harness = Harness::new(&server);

    let (status, body, headers) = harness
        .json_with_headers(
            "POST",
            "/oagw/v1/upstreams",
            Some(json!({
                "protocol": oagw::gts_helpers::PROTOCOL_HTTP,
                "server": {"endpoints": [
                    {"scheme": "http", "host": "api.example.com", "port": 8080}
                ]}
            })),
        )
        .await;

    assert_eq!(status, StatusCode::CREATED);
    let location = headers
        .get(header::LOCATION)
        .expect("a create advertises where the resource lives")
        .to_str()
        .expect("an ASCII location");
    assert_eq!(
        location,
        format!("/oagw/v1/upstreams/{}", body["id"].as_str().unwrap())
    );
    assert_eq!(body["alias"], json!("api.example.com:8080"));
    assert_eq!(body["enabled"], json!(true));
    assert_eq!(
        body["tenant_id"],
        json!("00000000-0000-0000-0000-000000000000")
    );
    assert_eq!(body["protocol"], json!(oagw::gts_helpers::PROTOCOL_HTTP));
}

#[tokio::test]
async fn create_accepts_a_matching_explicit_alias() {
    let server = MockServer::start();
    let harness = Harness::new(&server);

    let (_, body) = harness
        .json(
            "POST",
            "/oagw/v1/upstreams",
            Some(json!({
                "alias": "api.example.com:8080",
                "protocol": oagw::gts_helpers::PROTOCOL_HTTP,
                "server": {"endpoints": [
                    {"scheme": "http", "host": "api.example.com", "port": 8080}
                ]}
            })),
        )
        .await;
    assert_eq!(body["alias"], json!("api.example.com:8080"));
}

#[tokio::test]
async fn create_rejects_an_alias_that_contradicts_the_pool() {
    let server = MockServer::start();
    let harness = Harness::new(&server);

    let (status, problem) = harness
        .json(
            "POST",
            "/oagw/v1/upstreams",
            Some(json!({
                "alias": "somewhere-else",
                "protocol": oagw::gts_helpers::PROTOCOL_HTTP,
                "server": {"endpoints": [
                    {"scheme": "http", "host": "api.example.com", "port": 8080}
                ]}
            })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    assert_eq!(
        problem["type"],
        json!(oagw::gts_helpers::error_type_id("validation.error"))
    );
}

#[tokio::test]
async fn create_requires_an_alias_for_an_ip_pool() {
    let server = MockServer::start();
    let harness = Harness::new(&server);

    let (status, problem) = harness
        .json(
            "POST",
            "/oagw/v1/upstreams",
            Some(json!({
                "protocol": oagw::gts_helpers::PROTOCOL_HTTP,
                "server": {"endpoints": [{"scheme": "http", "host": "10.1.2.3", "port": 80}]}
            })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    let detail = problem["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("explicit alias"),
        "the reason names the missing alias: {detail}"
    );
}

#[tokio::test]
async fn create_accepts_an_explicit_alias_for_an_ip_pool() {
    let server = MockServer::start();
    let harness = Harness::new(&server);
    let body = seed_named_upstream(&harness, "payments.internal").await;
    assert_eq!(body["alias"], json!("payments.internal"));
    assert_eq!(body["server"]["endpoints"][0]["host"], json!("10.0.0.9"));
}

#[tokio::test]
async fn create_derives_the_common_suffix_of_a_multi_host_pool() {
    let server = MockServer::start();
    let harness = Harness::new(&server);

    let (status, body) = harness
        .json(
            "POST",
            "/oagw/v1/upstreams",
            Some(json!({
                "protocol": oagw::gts_helpers::PROTOCOL_HTTP,
                "server": {"endpoints": [
                    {"scheme": "https", "host": "eu.api.example.com", "port": 443},
                    {"scheme": "https", "host": "us.api.example.com", "port": 443}
                ]}
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["alias"], json!("api.example.com"));
}

#[tokio::test]
async fn create_rejects_a_pool_without_a_registrable_common_suffix() {
    let server = MockServer::start();
    let harness = Harness::new(&server);

    let (status, problem) = harness
        .json(
            "POST",
            "/oagw/v1/upstreams",
            Some(json!({
                "protocol": oagw::gts_helpers::PROTOCOL_HTTP,
                "server": {"endpoints": [
                    {"scheme": "http", "host": "a.example.com", "port": 443},
                    {"scheme": "http", "host": "b.other.org", "port": 443}
                ]}
            })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
}

#[tokio::test]
async fn create_rejects_a_bare_public_suffix_pool() {
    let server = MockServer::start();
    let harness = Harness::new(&server);

    let (status, problem) = harness
        .json(
            "POST",
            "/oagw/v1/upstreams",
            Some(json!({
                "protocol": oagw::gts_helpers::PROTOCOL_HTTP,
                "server": {"endpoints": [
                    {"scheme": "http", "host": "a.co.uk", "port": 443},
                    {"scheme": "http", "host": "b.co.uk", "port": 443}
                ]}
            })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
}

#[tokio::test]
async fn create_rejects_an_unknown_field() {
    let server = MockServer::start();
    let harness = Harness::new(&server);

    let (status, problem) = harness
        .json(
            "POST",
            "/oagw/v1/upstreams",
            Some(json!({
                "protocol": oagw::gts_helpers::PROTOCOL_HTTP,
                "nonsense": true,
                "server": {"endpoints": [{"scheme": "http", "host": "api.example.com", "port": 443}]}
            })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
}

#[tokio::test]
async fn list_and_read_round_trip() {
    let server = MockServer::start();
    let harness = Harness::new(&server);
    let created = seed_named_upstream(&harness, "payments.internal").await;

    let (status, page) = harness.json("GET", "/oagw/v1/upstreams", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["count"], json!(1));
    assert_eq!(page["items"][0]["id"], created["id"].clone());
    assert_eq!(page["total"], json!(1));

    let id = created["id"].as_str().unwrap();
    let (status, found) = harness
        .json("GET", &format!("/oagw/v1/upstreams/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(found["alias"], json!("payments.internal"));
    assert!(found["created_at"].is_string());
    assert!(found["updated_at"].is_string());
}

#[tokio::test]
async fn read_of_an_unknown_upstream_is_a_problem_document() {
    let server = MockServer::start();
    let harness = Harness::new(&server);

    let (status, problem) = harness
        .json("GET", "/oagw/v1/upstreams/upstream-absent", None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{problem}");
    assert_eq!(
        problem["type"],
        json!(oagw::gts_helpers::error_type_id("route.not_found"))
    );
    assert_eq!(problem["title"], json!("Route Not Found"));
    assert_eq!(problem["status"], json!(404));
    assert_eq!(
        problem["instance"],
        json!("/oagw/v1/upstreams/upstream-absent")
    );
}

#[tokio::test]
async fn replace_updates_fields_and_keeps_the_identity_stable() {
    let server = MockServer::start();
    let harness = Harness::new(&server);
    let created = seed_named_upstream(&harness, "payments.internal").await;
    let id = created["id"].as_str().unwrap().to_owned();

    let (status, body) = harness
        .json(
            "PUT",
            &format!("/oagw/v1/upstreams/{id}"),
            Some(json!({
                "enabled": false,
                "protocol": oagw::gts_helpers::PROTOCOL_HTTP,
                "server": {"endpoints": [{"scheme": "http", "host": "10.0.0.9", "port": 9000}]},
                "tags": ["payments"]
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], json!(id));
    assert_eq!(body["alias"], json!("payments.internal"));
    assert_eq!(body["enabled"], json!(false));
    assert_eq!(body["tags"], json!(["payments"]));

    // A second create must not inherit the disabled state.
    let (_, reread) = harness
        .json("GET", &format!("/oagw/v1/upstreams/{id}"), None)
        .await;
    assert_eq!(reread["enabled"], json!(false));
}

#[tokio::test]
async fn replace_rejects_an_alias_change() {
    let server = MockServer::start();
    let harness = Harness::new(&server);
    let created = seed_named_upstream(&harness, "payments.internal").await;
    let id = created["id"].as_str().unwrap();

    let (status, problem) = harness
        .json(
            "PUT",
            &format!("/oagw/v1/upstreams/{id}"),
            Some(json!({
                "alias": "renamed.internal",
                "protocol": oagw::gts_helpers::PROTOCOL_HTTP,
                "server": {"endpoints": [{"scheme": "http", "host": "10.0.0.9", "port": 9000}]}
            })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    let detail = problem["detail"].as_str().unwrap_or_default();
    assert!(detail.contains("immutable"), "{detail}");
}

#[tokio::test]
async fn replace_of_an_unknown_upstream_is_404() {
    let server = MockServer::start();
    let harness = Harness::new(&server);

    let (status, problem) = harness
        .json(
            "PUT",
            "/oagw/v1/upstreams/upstream-absent",
            Some(json!({
                "protocol": oagw::gts_helpers::PROTOCOL_HTTP,
                "server": {"endpoints": [{"scheme": "http", "host": "10.0.0.9", "port": 9000}]}
            })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{problem}");
}

#[tokio::test]
async fn delete_removes_the_upstream_and_its_routes() {
    let server = MockServer::start();
    let harness = Harness::new(&server);
    let created = seed_named_upstream(&harness, "payments.internal").await;
    let id = created["id"].as_str().unwrap().to_owned();
    harness.seed_route(&id, "/v1/pay", &["POST"]).await;

    let (status, _) = harness
        .json("DELETE", &format!("/oagw/v1/upstreams/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, problem) = harness
        .json("GET", &format!("/oagw/v1/upstreams/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{problem}");

    let (_, routes) = harness.json("GET", "/oagw/v1/routes", None).await;
    assert_eq!(routes["count"], json!(0), "the cascade removed the routes");
}

#[tokio::test]
async fn a_second_upstream_with_a_derivable_alias_conflicts() {
    let server = MockServer::start();
    let harness = Harness::new(&server);

    let (first, _) = harness
        .json(
            "POST",
            "/oagw/v1/upstreams",
            Some(json!({
                "alias": "payments.internal",
                "protocol": oagw::gts_helpers::PROTOCOL_HTTP,
                "server": {"endpoints": [{"scheme": "http", "host": "10.0.0.9", "port": 9000}]}
            })),
        )
        .await;
    assert_eq!(first, StatusCode::CREATED);

    let (status, problem) = harness
        .json(
            "POST",
            "/oagw/v1/upstreams",
            Some(json!({
                "alias": "payments.internal",
                "protocol": oagw::gts_helpers::PROTOCOL_HTTP,
                "server": {"endpoints": [{"scheme": "http", "host": "10.0.1.9", "port": 9000}]}
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");
    assert_eq!(
        problem["type"],
        json!(oagw::gts_helpers::error_type_id("conflict"))
    );
    assert_eq!(problem["status"], json!(409));
}

#[tokio::test]
async fn routes_crud_and_duplicate_detection() {
    let server = MockServer::start();
    let harness = Harness::new(&server);
    let upstream = seed_named_upstream(&harness, "payments.internal").await;
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();

    let (status, route) = harness
        .json(
            "POST",
            &format!("/oagw/v1/upstreams/{upstream_id}/routes"),
            Some(json!({"match": {"http": {"path": "/v1/pay", "methods": ["POST"]}}})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{route}");
    assert_eq!(route["upstream_id"], json!(upstream_id));
    assert_eq!(route["match"]["http"]["path"], json!("/v1/pay"));
    let route_id = route["id"].as_str().unwrap().to_owned();

    let (status, problem) = harness
        .json(
            "POST",
            &format!("/oagw/v1/upstreams/{upstream_id}/routes"),
            Some(json!({"match": {"http": {"path": "/v1/pay", "methods": ["POST"]}}})),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");

    let (status, listed) = harness.json("GET", "/oagw/v1/routes", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["count"], json!(1));

    let (status, found) = harness
        .json("GET", &format!("/oagw/v1/routes/{route_id}"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(found["id"], json!(route_id));

    let (status, problem) = harness
        .json(
            "POST",
            "/oagw/v1/upstreams/upstream-absent/routes",
            Some(json!({"match": {"http": {"path": "/v1/pay", "methods": ["POST"]}}})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");

    let (status, _) = harness
        .json("DELETE", &format!("/oagw/v1/routes/{route_id}"), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, problem) = harness
        .json("GET", &format!("/oagw/v1/routes/{route_id}"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{problem}");
}

#[tokio::test]
async fn route_replacement_keeps_the_upstream_immutable() {
    let server = MockServer::start();
    let harness = Harness::new(&server);
    let upstream = seed_named_upstream(&harness, "payments.internal").await;
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();
    let route = harness.seed_route(&upstream_id, "/v1/pay", &["POST"]).await;
    let route_id = route["id"].as_str().unwrap().to_owned();

    let (status, body) = harness
        .json(
            "PUT",
            &format!("/oagw/v1/routes/{route_id}"),
            Some(json!({
                "upstream_id": "upstream-elsewhere",
                "match": {"http": {"path": "/v2/pay", "methods": ["POST", "GET"]}}
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["upstream_id"], json!(upstream_id));
    assert_eq!(body["match"]["http"]["path"], json!("/v2/pay"));
}

#[tokio::test]
async fn plugins_crud_with_reference_protection() {
    let server = MockServer::start();
    let harness = Harness::new(&server);

    let (status, plugin) = harness
        .json(
            "POST",
            "/oagw/v1/plugins",
            Some(json!({
                "plugin_type": "guard",
                "name": "block-delete",
                "source_code": "def on_request(ctx):\n    return ctx\n"
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{plugin}");
    let plugin_id = plugin["id"].as_str().unwrap().to_owned();

    let (status, found) = harness
        .json("GET", &format!("/oagw/v1/plugins/{plugin_id}"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(found["name"], json!("block-delete"));

    let (status, source) = harness
        .json("GET", &format!("/oagw/v1/plugins/{plugin_id}/source"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        source.is_null(),
        "the source endpoint returns plain text, not JSON: {source}"
    );

    // A create cannot choose its own identity.
    let (status, _) = harness
        .json(
            "PUT",
            &format!("/oagw/v1/plugins/{plugin_id}"),
            Some(json!({"plugin_type": "guard", "name": "renamed", "source_code": "x"})),
        )
        .await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);

    let (status, problem) = harness
        .json(
            "POST",
            "/oagw/v1/plugins",
            Some(json!({"plugin_type": "guard", "name": "empty", "source_code": ""})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");

    // Referenced plugins are protected.
    let (status, _) = harness
        .json(
            "POST",
            "/oagw/v1/upstreams",
            Some(json!({
                "alias": "chained.internal",
                "protocol": oagw::gts_helpers::PROTOCOL_HTTP,
                "plugins": {"items": [plugin_id]},
                "server": {"endpoints": [{"scheme": "http", "host": "10.0.2.9", "port": 9000}]}
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, problem) = harness
        .json("DELETE", &format!("/oagw/v1/plugins/{plugin_id}"), None)
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");
    assert_eq!(
        problem["type"],
        json!(oagw::gts_helpers::error_type_id("plugin.in_use"))
    );

    let (status, _) = harness
        .json("DELETE", &format!("/oagw/v1/plugins/{plugin_id}"), None)
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
}
