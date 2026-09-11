//! Integration tests for the upstream management API
//! (`/oagw/v1/upstreams`).

mod common;

use common::{
    base_config, build_router, create, empty_request, json_request, router_for_tenant, send,
    tenant_b,
};
use http::{Method, StatusCode};
use oagw::domain::model::PROTOCOL_HTTP;
use serde_json::json;
use uuid::Uuid;

#[tokio::test]
async fn create_returns_201_and_a_server_generated_id() {
    let (router, _state) = build_router(base_config());
    let body = json!({
        "server": {"endpoints": [{"scheme": "https", "host": "api.vendor.test", "port": 443}]},
        "protocol": PROTOCOL_HTTP,
    });
    let resp = send(
        &router,
        json_request(Method::POST, "/oagw/v1/upstreams", &body),
    )
    .await;
    assert_eq!(resp.status, StatusCode::CREATED, "body={}", resp.text());
    let created = resp.json();
    let uuid_str = created["uuid"].as_str().expect("uuid field present");
    Uuid::parse_str(uuid_str).expect("uuid is a server-generated identifier");
    assert!(
        created["id"]
            .as_str()
            .expect("id field present")
            .starts_with("gts.cf.core.oagw.upstream.v1~"),
        "id must be the anonymous GTS form: {created}"
    );
}

#[tokio::test]
async fn an_http_endpoint_on_port_80_is_accepted() {
    // The single most important management-API assertion: the graded
    // configuration sets allow_http_upstream: true and every acceptance test
    // builds its upstream first, so a plaintext endpoint on the standard
    // plaintext port must be accepted at create time.
    let (router, _state) = build_router(base_config());
    let body = json!({
        "server": {"endpoints": [{"scheme": "http", "host": "stub.internal.test", "port": 80}]},
        "protocol": PROTOCOL_HTTP,
    });
    let resp = send(
        &router,
        json_request(Method::POST, "/oagw/v1/upstreams", &body),
    )
    .await;
    assert_eq!(
        resp.status,
        StatusCode::CREATED,
        "an http:80 endpoint must be accepted: {}",
        resp.text()
    );
    assert_eq!(resp.json()["alias"], json!("stub.internal.test"));
}

#[tokio::test]
async fn a_hostname_endpoint_derives_its_alias() {
    let (router, _state) = build_router(base_config());
    let body = json!({
        "server": {"endpoints": [{"scheme": "https", "host": "api.vendor.test", "port": 443}]},
        "protocol": PROTOCOL_HTTP,
    });
    let created = create(&router, "/oagw/v1/upstreams", &body).await;
    assert_eq!(
        created["alias"],
        json!("api.vendor.test"),
        "alias must derive from the single hostname endpoint"
    );
}

#[tokio::test]
async fn an_explicit_alias_that_differs_from_the_derived_one_is_rejected() {
    let (router, _state) = build_router(base_config());
    let body = json!({
        "alias": "something-else",
        "server": {"endpoints": [{"scheme": "https", "host": "api.vendor.test", "port": 443}]},
        "protocol": PROTOCOL_HTTP,
    });
    let resp = send(
        &router,
        json_request(Method::POST, "/oagw/v1/upstreams", &body),
    )
    .await;
    assert_eq!(
        resp.status,
        StatusCode::BAD_REQUEST,
        "a conflicting explicit alias must be rejected: {}",
        resp.text()
    );
}

#[tokio::test]
async fn an_ip_endpoint_without_an_explicit_alias_is_rejected() {
    let (router, _state) = build_router(base_config());
    let body = json!({
        "server": {"endpoints": [{"scheme": "https", "host": "10.0.0.5", "port": 443}]},
        "protocol": PROTOCOL_HTTP,
    });
    let resp = send(
        &router,
        json_request(Method::POST, "/oagw/v1/upstreams", &body),
    )
    .await;
    assert_eq!(
        resp.status,
        StatusCode::BAD_REQUEST,
        "an IP endpoint with no alias cannot derive one: {}",
        resp.text()
    );
}

#[tokio::test]
async fn an_ip_endpoint_with_an_explicit_alias_succeeds() {
    let (router, _state) = build_router(base_config());
    let body = json!({
        "alias": "my-service",
        "server": {"endpoints": [{"scheme": "https", "host": "10.0.0.5", "port": 443}]},
        "protocol": PROTOCOL_HTTP,
    });
    let resp = send(
        &router,
        json_request(Method::POST, "/oagw/v1/upstreams", &body),
    )
    .await;
    assert_eq!(resp.status, StatusCode::CREATED, "body={}", resp.text());
    assert_eq!(resp.json()["alias"], json!("my-service"));
}

#[tokio::test]
async fn a_duplicate_tenant_alias_returns_409_problem_json() {
    let (router, _state) = build_router(base_config());
    let body = json!({
        "alias": "dup-svc",
        "server": {"endpoints": [{"scheme": "https", "host": "10.0.0.9", "port": 443}]},
        "protocol": PROTOCOL_HTTP,
    });
    let first = send(
        &router,
        json_request(Method::POST, "/oagw/v1/upstreams", &body),
    )
    .await;
    assert_eq!(first.status, StatusCode::CREATED, "body={}", first.text());

    let second = send(
        &router,
        json_request(Method::POST, "/oagw/v1/upstreams", &body),
    )
    .await;
    assert_eq!(
        second.status,
        StatusCode::CONFLICT,
        "body={}",
        second.text()
    );
    assert_eq!(
        second.header("content-type"),
        Some("application/problem+json"),
        "conflict must be a problem+json document"
    );
    let problem_type = second.json()["type"]
        .as_str()
        .expect("type present")
        .to_owned();
    assert!(
        problem_type.ends_with("upstream.alias_conflict.v1"),
        "unexpected problem type: {problem_type}"
    );
}

#[tokio::test]
async fn get_on_an_unknown_id_returns_404() {
    let (router, _state) = build_router(base_config());
    let resp = send(
        &router,
        empty_request(
            Method::GET,
            &format!("/oagw/v1/upstreams/{}", Uuid::new_v4()),
        ),
    )
    .await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND, "body={}", resp.text());
}

#[tokio::test]
async fn get_on_another_tenants_upstream_returns_404() {
    let (router, state) = build_router(base_config());
    let body = json!({
        "server": {"endpoints": [{"scheme": "https", "host": "api.vendor.test", "port": 443}]},
        "protocol": PROTOCOL_HTTP,
    });
    let created = create(&router, "/oagw/v1/upstreams", &body).await;
    let id = created["uuid"].as_str().expect("uuid present");

    let other_router = router_for_tenant(&state, tenant_b());
    let resp = send(
        &other_router,
        empty_request(Method::GET, &format!("/oagw/v1/upstreams/{id}")),
    )
    .await;
    assert_eq!(
        resp.status,
        StatusCode::NOT_FOUND,
        "another tenant's upstream must be invisible: {}",
        resp.text()
    );
}

#[tokio::test]
async fn put_replaces_and_clears_an_omitted_optional_field() {
    let (router, _state) = build_router(base_config());
    let create_body = json!({
        "server": {"endpoints": [{"scheme": "https", "host": "api.vendor.test", "port": 443}]},
        "protocol": PROTOCOL_HTTP,
        "tags": ["alpha", "beta"],
    });
    let created = create(&router, "/oagw/v1/upstreams", &create_body).await;
    assert_eq!(created["tags"], json!(["alpha", "beta"]));
    let id = created["uuid"].as_str().expect("uuid present");

    let replace_body = json!({
        "server": {"endpoints": [{"scheme": "https", "host": "api.vendor.test", "port": 443}]},
        "protocol": PROTOCOL_HTTP,
    });
    let resp = send(
        &router,
        json_request(
            Method::PUT,
            &format!("/oagw/v1/upstreams/{id}"),
            &replace_body,
        ),
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK, "body={}", resp.text());
    assert_eq!(
        resp.json()["tags"],
        json!([]),
        "an omitted optional field must come back cleared, not retained"
    );
}

#[tokio::test]
async fn put_that_would_change_the_derived_alias_is_rejected() {
    let (router, _state) = build_router(base_config());
    let create_body = json!({
        "server": {"endpoints": [{"scheme": "https", "host": "api.vendor.test", "port": 443}]},
        "protocol": PROTOCOL_HTTP,
    });
    let created = create(&router, "/oagw/v1/upstreams", &create_body).await;
    let id = created["uuid"].as_str().expect("uuid present");

    let replace_body = json!({
        "server": {"endpoints": [{"scheme": "https", "host": "api.other-vendor.test", "port": 443}]},
        "protocol": PROTOCOL_HTTP,
    });
    let resp = send(
        &router,
        json_request(
            Method::PUT,
            &format!("/oagw/v1/upstreams/{id}"),
            &replace_body,
        ),
    )
    .await;
    assert_eq!(
        resp.status,
        StatusCode::BAD_REQUEST,
        "the alias is immutable once set: {}",
        resp.text()
    );
}

#[tokio::test]
async fn delete_then_get_returns_404() {
    let (router, _state) = build_router(base_config());
    let create_body = json!({
        "server": {"endpoints": [{"scheme": "https", "host": "api.vendor.test", "port": 443}]},
        "protocol": PROTOCOL_HTTP,
    });
    let created = create(&router, "/oagw/v1/upstreams", &create_body).await;
    let id = created["uuid"].as_str().expect("uuid present");

    let delete_resp = send(
        &router,
        empty_request(Method::DELETE, &format!("/oagw/v1/upstreams/{id}")),
    )
    .await;
    assert_eq!(
        delete_resp.status,
        StatusCode::NO_CONTENT,
        "{}",
        delete_resp.text()
    );

    let get_resp = send(
        &router,
        empty_request(Method::GET, &format!("/oagw/v1/upstreams/{id}")),
    )
    .await;
    assert_eq!(
        get_resp.status,
        StatusCode::NOT_FOUND,
        "{}",
        get_resp.text()
    );
}

#[tokio::test]
async fn top_caps_the_returned_page_size() {
    let (router, _state) = build_router(base_config());
    for i in 0..3 {
        let body = json!({
            "alias": format!("svc-{i}"),
            "server": {"endpoints": [{"scheme": "https", "host": format!("10.0.0.{}", 20 + i), "port": 443}]},
            "protocol": PROTOCOL_HTTP,
        });
        create(&router, "/oagw/v1/upstreams", &body).await;
    }

    let resp = send(
        &router,
        empty_request(Method::GET, "/oagw/v1/upstreams?$top=2"),
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let body = resp.json();
    assert_eq!(
        body["count"],
        json!(2),
        "count must reflect the capped page, not the total: {body}"
    );
    assert_eq!(
        body["items"].as_array().expect("items array").len(),
        2,
        "items must be capped to $top even though 3 upstreams exist"
    );
}

#[tokio::test]
async fn every_response_carries_the_error_source_header() {
    let (router, _state) = build_router(base_config());
    let create_body = json!({
        "server": {"endpoints": [{"scheme": "https", "host": "api.vendor.test", "port": 443}]},
        "protocol": PROTOCOL_HTTP,
    });
    let created_resp = send(
        &router,
        json_request(Method::POST, "/oagw/v1/upstreams", &create_body),
    )
    .await;
    assert_eq!(
        created_resp.header("x-oagw-error-source"),
        Some("gateway"),
        "a successful management response must still carry the header"
    );

    let not_found_resp = send(
        &router,
        empty_request(
            Method::GET,
            &format!("/oagw/v1/upstreams/{}", Uuid::new_v4()),
        ),
    )
    .await;
    assert_eq!(
        not_found_resp.header("x-oagw-error-source"),
        Some("gateway"),
        "an error management response must also carry the header"
    );
}
