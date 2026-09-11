//! Integration tests for the route management API (`/oagw/v1/routes`).

mod common;

use common::{base_config, build_router, create, empty_request, json_request, send};
use http::{Method, StatusCode};
use oagw::domain::model::PROTOCOL_HTTP;
use serde_json::json;
use uuid::Uuid;

async fn create_upstream(router: &axum::Router, alias: &str, host: &str) -> serde_json::Value {
    let body = json!({
        "alias": alias,
        "server": {"endpoints": [{"scheme": "https", "host": host, "port": 443}]},
        "protocol": PROTOCOL_HTTP,
    });
    create(router, "/oagw/v1/upstreams", &body).await
}

#[tokio::test]
async fn create_with_a_valid_upstream_id_returns_201() {
    let (router, _state) = build_router(base_config());
    let upstream = create_upstream(&router, "svc", "10.0.1.1").await;

    let body = json!({
        "upstream_id": upstream["uuid"],
        "match": {"http": {"methods": ["GET"], "path": "/v1"}},
    });
    let resp = send(
        &router,
        json_request(Method::POST, "/oagw/v1/routes", &body),
    )
    .await;
    assert_eq!(resp.status, StatusCode::CREATED, "body={}", resp.text());
    assert_eq!(resp.json()["upstream_id"], upstream["uuid"]);
}

#[tokio::test]
async fn an_unknown_upstream_id_returns_400() {
    let (router, _state) = build_router(base_config());
    let body = json!({
        "upstream_id": Uuid::new_v4().to_string(),
        "match": {"http": {"methods": ["GET"], "path": "/v1"}},
    });
    let resp = send(
        &router,
        json_request(Method::POST, "/oagw/v1/routes", &body),
    )
    .await;
    assert_eq!(
        resp.status,
        StatusCode::BAD_REQUEST,
        "an upstream_id naming nothing must be rejected: {}",
        resp.text()
    );
}

#[tokio::test]
async fn a_match_carrying_both_http_and_grpc_is_rejected() {
    let (router, _state) = build_router(base_config());
    let upstream = create_upstream(&router, "svc", "10.0.1.2").await;
    let body = json!({
        "upstream_id": upstream["uuid"],
        "match": {
            "http": {"methods": ["GET"], "path": "/v1"},
            "grpc": {"service": "S", "method": "M"},
        },
    });
    let resp = send(
        &router,
        json_request(Method::POST, "/oagw/v1/routes", &body),
    )
    .await;
    assert_eq!(
        resp.status,
        StatusCode::BAD_REQUEST,
        "both http and grpc must be rejected: {}",
        resp.text()
    );
}

#[tokio::test]
async fn a_match_carrying_neither_http_nor_grpc_is_rejected() {
    let (router, _state) = build_router(base_config());
    let upstream = create_upstream(&router, "svc", "10.0.1.3").await;
    let body = json!({
        "upstream_id": upstream["uuid"],
        "match": {},
    });
    let resp = send(
        &router,
        json_request(Method::POST, "/oagw/v1/routes", &body),
    )
    .await;
    assert_eq!(
        resp.status,
        StatusCode::BAD_REQUEST,
        "neither http nor grpc must be rejected: {}",
        resp.text()
    );
}

#[tokio::test]
async fn a_duplicate_enabled_match_rule_returns_409() {
    let (router, _state) = build_router(base_config());
    let upstream = create_upstream(&router, "svc", "10.0.1.4").await;
    let body = json!({
        "upstream_id": upstream["uuid"],
        "match": {"http": {"methods": ["GET"], "path": "/v1"}},
    });
    let first = send(
        &router,
        json_request(Method::POST, "/oagw/v1/routes", &body),
    )
    .await;
    assert_eq!(first.status, StatusCode::CREATED, "{}", first.text());

    let second = send(
        &router,
        json_request(Method::POST, "/oagw/v1/routes", &body),
    )
    .await;
    assert_eq!(second.status, StatusCode::CONFLICT, "{}", second.text());
    let problem_type = second.json()["type"]
        .as_str()
        .expect("type present")
        .to_owned();
    assert!(
        problem_type.ends_with("route.match_conflict.v1"),
        "unexpected problem type: {problem_type}"
    );
}

#[tokio::test]
async fn a_disabled_route_does_not_block_creating_an_equivalent_enabled_one() {
    // Regression guard: a disabled sibling with the same path and method must
    // not participate in the match-determinism conflict check.
    let (router, _state) = build_router(base_config());
    let upstream = create_upstream(&router, "svc", "10.0.1.5").await;

    let disabled_body = json!({
        "upstream_id": upstream["uuid"],
        "enabled": false,
        "match": {"http": {"methods": ["GET"], "path": "/v1"}},
    });
    let disabled_resp = send(
        &router,
        json_request(Method::POST, "/oagw/v1/routes", &disabled_body),
    )
    .await;
    assert_eq!(
        disabled_resp.status,
        StatusCode::CREATED,
        "{}",
        disabled_resp.text()
    );

    let enabled_body = json!({
        "upstream_id": upstream["uuid"],
        "enabled": true,
        "match": {"http": {"methods": ["GET"], "path": "/v1"}},
    });
    let enabled_resp = send(
        &router,
        json_request(Method::POST, "/oagw/v1/routes", &enabled_body),
    )
    .await;
    assert_eq!(
        enabled_resp.status,
        StatusCode::CREATED,
        "a disabled sibling must not block an equivalent enabled route: {}",
        enabled_resp.text()
    );
}

#[tokio::test]
async fn put_cannot_change_the_upstream_id() {
    let (router, _state) = build_router(base_config());
    let upstream1 = create_upstream(&router, "svc-1", "10.0.1.6").await;
    let upstream2 = create_upstream(&router, "svc-2", "10.0.1.7").await;

    let create_body = json!({
        "upstream_id": upstream1["uuid"],
        "match": {"http": {"methods": ["GET"], "path": "/v1"}},
    });
    let created = create(&router, "/oagw/v1/routes", &create_body).await;
    let route_id = created["uuid"].as_str().expect("uuid present");
    assert_eq!(created["upstream_id"], upstream1["uuid"]);

    // The replace DTO has no upstream_id field at all; even smuggling one in
    // the JSON body must not change the stored value.
    let replace_body = json!({
        "upstream_id": upstream2["uuid"],
        "match": {"http": {"methods": ["GET", "POST"], "path": "/v1"}},
    });
    let resp = send(
        &router,
        json_request(
            Method::PUT,
            &format!("/oagw/v1/routes/{route_id}"),
            &replace_body,
        ),
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    assert_eq!(
        resp.json()["upstream_id"],
        upstream1["uuid"],
        "the upstream reference must be retained, not replaced"
    );
}

#[tokio::test]
async fn a_method_outside_the_allowed_set_is_rejected() {
    let (router, _state) = build_router(base_config());
    let upstream = create_upstream(&router, "svc", "10.0.1.8").await;
    let body = json!({
        "upstream_id": upstream["uuid"],
        "match": {"http": {"methods": ["TRACE"], "path": "/v1"}},
    });
    let resp = send(
        &router,
        json_request(Method::POST, "/oagw/v1/routes", &body),
    )
    .await;
    assert_eq!(
        resp.status,
        StatusCode::BAD_REQUEST,
        "TRACE is outside GET|POST|PUT|DELETE|PATCH: {}",
        resp.text()
    );
}

#[tokio::test]
async fn get_route_after_create_returns_it() {
    let (router, _state) = build_router(base_config());
    let upstream = create_upstream(&router, "svc", "10.0.1.9").await;
    let body = json!({
        "upstream_id": upstream["uuid"],
        "match": {"http": {"methods": ["GET"], "path": "/v1"}},
    });
    let created = create(&router, "/oagw/v1/routes", &body).await;
    let id = created["uuid"].as_str().expect("uuid present");
    let resp = send(
        &router,
        empty_request(Method::GET, &format!("/oagw/v1/routes/{id}")),
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
}
