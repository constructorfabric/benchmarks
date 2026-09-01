// Created: 2026-08-29 by Constructor Tech
//! Route management CRUD, match validation and longest-prefix selection.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{get, json_body, post, put, tenant};
use httpmock::MockServer;
use serde_json::{Value, json};
use uuid::Uuid;

fn route(upstream_id: &str, path: &str) -> Value {
    json!({
        "upstream_id": upstream_id,
        "match": { "http": { "methods": ["GET"], "path": path } },
    })
}

fn upstream_payload(host: &str, port: u16) -> Value {
    json!({
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "server": { "endpoints": [ { "scheme": "http", "host": host, "port": port } ] },
    })
}

#[tokio::test]
async fn route_crud_round_trip() {
    let harness = common::Harness::new(common::test_config(), None);
    let upstream = json_body(
        post(
            harness.router(),
            "/oagw/v1/upstreams",
            upstream_payload("route-crud.example.com", 443),
            tenant(),
        )
        .await,
    )
    .await;
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();

    let created = json_body(
        post(
            harness.router(),
            "/oagw/v1/routes",
            route(&upstream_id, "/api"),
            tenant(),
        )
        .await,
    )
    .await;
    assert_eq!(created["upstream_id"], upstream_id.as_str());
    assert_eq!(created["enabled"], true);
    assert_eq!(created["match"]["http"]["path"], "/api");
    let id = created["id"].as_str().unwrap().to_owned();

    let list = json_body(get(harness.router(), "/oagw/v1/routes", tenant()).await).await;
    assert_eq!(list["items"].as_array().unwrap().len(), 1);

    let fetched =
        json_body(get(harness.router(), &format!("/oagw/v1/routes/{id}"), tenant()).await).await;
    assert_eq!(fetched["match"]["http"]["methods"], json!(["GET"]));

    let replaced = json!({
        "upstream_id": upstream_id,
        "match": { "http": { "methods": ["GET", "POST"], "path": "/api" } },
    });
    let replaced = json_body(
        put(
            harness.router(),
            &format!("/oagw/v1/routes/{id}"),
            replaced,
            tenant(),
        )
        .await,
    )
    .await;
    assert_eq!(replaced["match"]["http"]["methods"], json!(["GET", "POST"]));

    let response = harness
        .send("DELETE", &format!("/oagw/v1/routes/{id}"), None, tenant())
        .await;
    assert_eq!(response.status(), 204);
    assert_eq!(
        get(harness.router(), &format!("/oagw/v1/routes/{id}"), tenant())
            .await
            .status(),
        404
    );
}

#[tokio::test]
async fn missing_route_is_404() {
    let harness = common::Harness::new(common::test_config(), None);
    let missing = Uuid::new_v4();
    assert_eq!(
        get(
            harness.router(),
            &format!("/oagw/v1/routes/{missing}"),
            tenant()
        )
        .await
        .status(),
        404
    );
    let payload = json!({
        "upstream_id": Uuid::new_v4().to_string(),
        "match": { "http": { "methods": ["GET"], "path": "/x" } },
    });
    assert_eq!(
        put(
            harness.router(),
            &format!("/oagw/v1/routes/{missing}"),
            payload,
            tenant()
        )
        .await
        .status(),
        404
    );
}

#[tokio::test]
async fn match_validation_rejects_bad_rules() {
    let harness = common::Harness::new(common::test_config(), None);
    let upstream = json_body(
        post(
            harness.router(),
            "/oagw/v1/upstreams",
            upstream_payload("match-validation.example.com", 443),
            tenant(),
        )
        .await,
    )
    .await;
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();

    let cases: Vec<(Value, &str)> = vec![
        (
            json!({ "upstream_id": upstream_id, "match": { "http": { "methods": [], "path": "/api" } } }),
            "empty methods",
        ),
        (
            json!({ "upstream_id": upstream_id, "match": { "http": { "methods": ["GET"], "path": "" } } }),
            "empty path",
        ),
        (
            json!({ "upstream_id": upstream_id, "match": { "http": { "methods": ["GET"], "path": "api" } } }),
            "relative path",
        ),
        (
            json!({ "upstream_id": upstream_id, "match": { "http": { "methods": ["TRACE"], "path": "/api" } } }),
            "method outside the allowlist",
        ),
        (
            json!({ "upstream_id": upstream_id, "match": {
                "http": { "methods": ["GET"], "path": "/api" },
                "grpc": { "service": "svc", "method": "m" } } }),
            "both http and grpc",
        ),
        (
            json!({ "upstream_id": upstream_id, "match": {} }),
            "neither http nor grpc",
        ),
        (
            json!({ "match": { "http": { "methods": ["GET"], "path": "/api" } } }),
            "no upstream",
        ),
        (
            json!({ "upstream_id": Uuid::new_v4().to_string(), "match": { "http": { "methods": ["GET"], "path": "/api" } } }),
            "unknown upstream",
        ),
        (
            json!({ "upstream_id": upstream_id, "match": { "grpc": { "service": "svc", "method": "" } } }),
            "grpc method empty",
        ),
    ];
    for (payload, why) in cases {
        let response = post(harness.router(), "/oagw/v1/routes", payload, tenant()).await;
        assert_eq!(response.status(), 400, "expected 400 for {why}");
    }
}

#[tokio::test]
async fn longest_prefix_wins() {
    let server = MockServer::start();
    let deep = server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/api/v1/things");
        then.status(200).body("deep");
    });

    let harness = common::Harness::new(common::test_config(), None);
    // Endpoint hosts are IP literals, so the upstream needs an explicit alias.
    let upstream = json_body(
        post(
            harness.router(),
            "/oagw/v1/upstreams",
            json!({
                "alias": "prefix",
                "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
                "server": { "endpoints": [ { "scheme": "http", "host": server.host(), "port": server.port() } ] },
            }),
            tenant(),
        )
        .await,
    )
    .await;
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();

    // The shallow route forbids a path suffix, so a match on it is observable.
    post(
        harness.router(),
        "/oagw/v1/routes",
        json!({
            "upstream_id": upstream_id,
            "match": { "http": { "methods": ["GET"], "path": "/api", "path_suffix_mode": "disabled" } },
        }),
        tenant(),
    )
    .await;
    post(
        harness.router(),
        "/oagw/v1/routes",
        route(&upstream_id, "/api/v1"),
        tenant(),
    )
    .await;

    // `/api/v1/things` matches the deeper route, which appends the suffix.
    let response = harness
        .send("GET", "/oagw/v1/proxy/prefix/api/v1/things", None, tenant())
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(common::body_bytes(response).await.as_ref(), b"deep");
    assert_eq!(deep.calls(), 1);

    // `/api/other` matches only the shallow route, whose suffix is forbidden.
    let response = harness
        .send("GET", "/oagw/v1/proxy/prefix/api/other", None, tenant())
        .await;
    assert_eq!(response.status(), 400);
    let body = json_body(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
    assert!(body["detail"].as_str().unwrap().contains("path suffix"));
}

#[tokio::test]
async fn method_allowlist_excludes_a_route() {
    let harness = common::Harness::new(common::test_config(), None);
    let upstream = json_body(
        post(
            harness.router(),
            "/oagw/v1/upstreams",
            upstream_payload("methods.example.com", 443),
            tenant(),
        )
        .await,
    )
    .await;
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();
    post(
        harness.router(),
        "/oagw/v1/routes",
        json!({
            "upstream_id": upstream_id,
            "match": { "http": { "methods": ["POST"], "path": "/only-post" } },
        }),
        tenant(),
    )
    .await;

    let response = harness
        .send(
            "GET",
            "/oagw/v1/proxy/methods.example.com/only-post",
            None,
            tenant(),
        )
        .await;
    assert_eq!(response.status(), 404);
    let body = json_body(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
    );
    assert_eq!(body["status"], 404);
}

#[tokio::test]
async fn disabled_route_is_skipped() {
    let harness = common::Harness::new(common::test_config(), None);
    let upstream = json_body(
        post(
            harness.router(),
            "/oagw/v1/upstreams",
            upstream_payload("disabled-route.example.com", 443),
            tenant(),
        )
        .await,
    )
    .await;
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();
    let created = json_body(
        post(
            harness.router(),
            "/oagw/v1/routes",
            route(&upstream_id, "/gone"),
            tenant(),
        )
        .await,
    )
    .await;
    let id = created["id"].as_str().unwrap().to_owned();
    let replaced = json!({
        "upstream_id": upstream_id,
        "enabled": false,
        "match": { "http": { "methods": ["GET"], "path": "/gone" } },
    });
    let replaced = json_body(
        put(
            harness.router(),
            &format!("/oagw/v1/routes/{id}"),
            replaced,
            tenant(),
        )
        .await,
    )
    .await;
    assert_eq!(replaced["enabled"], false);

    let response = harness
        .send(
            "GET",
            "/oagw/v1/proxy/disabled-route.example.com/gone",
            None,
            tenant(),
        )
        .await;
    assert_eq!(response.status(), 404);
}
