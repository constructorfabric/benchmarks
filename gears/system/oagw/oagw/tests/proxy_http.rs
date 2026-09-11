//! Integration tests for the proxy data plane (`/oagw/v1/proxy/{alias}/...`).
//!
//! Every upstream in this file is an [`httpmock::MockServer`] bound to
//! `127.0.0.1`; the SSRF policy is disabled in [`common::base_config`] so a
//! loopback endpoint is not rejected before it is ever reached.

mod common;

use common::{base_config, build_router, create, empty_request, json_request, request, send};
use http::{Method, StatusCode};
use httpmock::MockServer;
use oagw::domain::model::PROTOCOL_HTTP;
use serde_json::json;

fn upstream_body(alias: &str, port: u16) -> serde_json::Value {
    json!({
        "alias": alias,
        "server": {"endpoints": [{"scheme": "http", "host": "127.0.0.1", "port": port}]},
        "protocol": PROTOCOL_HTTP,
    })
}

fn route_body(upstream_id: &serde_json::Value, path: &str, methods: &[&str]) -> serde_json::Value {
    json!({
        "upstream_id": upstream_id,
        "match": {"http": {"methods": methods, "path": path}},
    })
}

#[tokio::test]
async fn get_reaches_the_upstream_and_relays_status_and_body() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/echo");
        then.status(200).body("hello-upstream");
    });

    let (router, _state) = build_router(base_config());
    let upstream = create(
        &router,
        "/oagw/v1/upstreams",
        &upstream_body("svc", server.port()),
    )
    .await;
    create(
        &router,
        "/oagw/v1/routes",
        &route_body(&upstream["uuid"], "/echo", &["GET"]),
    )
    .await;

    let resp = send(
        &router,
        empty_request(Method::GET, "/oagw/v1/proxy/svc/echo"),
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    assert_eq!(resp.text(), "hello-upstream");
    assert_eq!(mock.calls(), 1);
}

#[tokio::test]
async fn host_is_rewritten_and_hop_by_hop_headers_are_stripped() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/echo")
            .header("host", format!("127.0.0.1:{}", server.port()))
            .header_missing("connection")
            .header_missing("keep-alive")
            .header_missing("te")
            .header_missing("transfer-encoding")
            .header_missing("upgrade")
            .header_missing("proxy-authorization")
            .header_missing("x-oagw-target-host");
        then.status(200).body("ok");
    });

    let (router, _state) = build_router(base_config());
    let upstream = create(
        &router,
        "/oagw/v1/upstreams",
        &upstream_body("svc", server.port()),
    )
    .await;
    create(
        &router,
        "/oagw/v1/routes",
        &route_body(&upstream["uuid"], "/echo", &["GET"]),
    )
    .await;

    let req = request(Method::GET, "/oagw/v1/proxy/svc/echo")
        .header("connection", "keep-alive")
        .header("keep-alive", "timeout=5")
        .header("te", "trailers")
        .header("transfer-encoding", "chunked")
        .header("upgrade", "h2c")
        .header("proxy-authorization", "Basic xyz")
        .header("x-oagw-target-host", "127.0.0.1")
        .body(axum::body::Body::empty())
        .expect("well-formed request");
    let resp = send(&router, req).await;
    assert_eq!(
        resp.status,
        StatusCode::OK,
        "the mock's header matchers must have matched: {}",
        resp.text()
    );
    assert_eq!(
        mock.calls(),
        1,
        "the upstream must have seen exactly one, header-clean request"
    );
}

#[tokio::test]
async fn an_unknown_alias_returns_404_with_gateway_error_source() {
    let (router, _state) = build_router(base_config());
    let resp = send(
        &router,
        empty_request(Method::GET, "/oagw/v1/proxy/no-such-alias/echo"),
    )
    .await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND, "{}", resp.text());
    assert_eq!(resp.header("x-oagw-error-source"), Some("gateway"));
}

#[tokio::test]
async fn a_disabled_upstream_returns_503() {
    let server = MockServer::start();
    let (router, _state) = build_router(base_config());
    let mut body = upstream_body("svc-disabled", server.port());
    body["enabled"] = json!(false);
    create(&router, "/oagw/v1/upstreams", &body).await;

    let resp = send(
        &router,
        empty_request(Method::GET, "/oagw/v1/proxy/svc-disabled/echo"),
    )
    .await;
    assert_eq!(
        resp.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        resp.text()
    );
}

#[tokio::test]
async fn a_method_outside_the_routes_allowlist_returns_404() {
    let server = MockServer::start();
    let (router, _state) = build_router(base_config());
    let upstream = create(
        &router,
        "/oagw/v1/upstreams",
        &upstream_body("svc", server.port()),
    )
    .await;
    create(
        &router,
        "/oagw/v1/routes",
        &route_body(&upstream["uuid"], "/echo", &["GET"]),
    )
    .await;

    let resp = send(
        &router,
        empty_request(Method::POST, "/oagw/v1/proxy/svc/echo"),
    )
    .await;
    assert_eq!(
        resp.status,
        StatusCode::NOT_FOUND,
        "no route matches a POST when only GET is allowed: {}",
        resp.text()
    );
}

#[tokio::test]
async fn a_query_parameter_outside_the_allowlist_returns_400() {
    let server = MockServer::start();
    let (router, _state) = build_router(base_config());
    let upstream = create(
        &router,
        "/oagw/v1/upstreams",
        &upstream_body("svc-q", server.port()),
    )
    .await;
    let route_body = json!({
        "upstream_id": upstream["uuid"],
        "match": {"http": {"methods": ["GET"], "path": "/search", "query_allowlist": ["limit"]}},
    });
    create(&router, "/oagw/v1/routes", &route_body).await;

    let resp = send(
        &router,
        empty_request(Method::GET, "/oagw/v1/proxy/svc-q/search?offset=1"),
    )
    .await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST, "{}", resp.text());
}

#[tokio::test]
async fn a_path_suffix_while_disabled_returns_400() {
    let server = MockServer::start();
    let (router, _state) = build_router(base_config());
    let upstream = create(
        &router,
        "/oagw/v1/upstreams",
        &upstream_body("svc-p", server.port()),
    )
    .await;
    let route_body = json!({
        "upstream_id": upstream["uuid"],
        "match": {
            "http": {"methods": ["GET"], "path": "/fixed", "path_suffix_mode": "disabled"},
        },
    });
    create(&router, "/oagw/v1/routes", &route_body).await;

    let resp = send(
        &router,
        empty_request(Method::GET, "/oagw/v1/proxy/svc-p/fixed/extra"),
    )
    .await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST, "{}", resp.text());
}

#[tokio::test]
async fn an_upstream_500_is_relayed_unchanged() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/err");
        then.status(500).body("boom");
    });

    let (router, _state) = build_router(base_config());
    let upstream = create(
        &router,
        "/oagw/v1/upstreams",
        &upstream_body("svc", server.port()),
    )
    .await;
    create(
        &router,
        "/oagw/v1/routes",
        &route_body(&upstream["uuid"], "/err", &["GET"]),
    )
    .await;

    let resp = send(
        &router,
        empty_request(Method::GET, "/oagw/v1/proxy/svc/err"),
    )
    .await;
    assert_eq!(
        resp.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "{}",
        resp.text()
    );
    assert_eq!(resp.text(), "boom");
    assert_eq!(resp.header("x-oagw-error-source"), Some("upstream"));
    assert_eq!(mock.calls(), 1);
}

#[tokio::test]
async fn content_length_validation_rejects_non_integer_and_oversize() {
    let server = MockServer::start();
    let (router, _state) = build_router(base_config());
    let upstream = create(
        &router,
        "/oagw/v1/upstreams",
        &upstream_body("svc", server.port()),
    )
    .await;
    create(
        &router,
        "/oagw/v1/routes",
        &route_body(&upstream["uuid"], "/echo", &["GET"]),
    )
    .await;

    let non_integer = request(Method::GET, "/oagw/v1/proxy/svc/echo")
        .header("content-length", "abc")
        .body(axum::body::Body::empty())
        .expect("well-formed request");
    let resp = send(&router, non_integer).await;
    assert_eq!(
        resp.status,
        StatusCode::BAD_REQUEST,
        "a non-integer Content-Length must be rejected: {}",
        resp.text()
    );

    let oversize = request(Method::GET, "/oagw/v1/proxy/svc/echo")
        .header("content-length", (200 * 1024 * 1024).to_string())
        .body(axum::body::Body::empty())
        .expect("well-formed request");
    let resp = send(&router, oversize).await;
    assert_eq!(
        resp.status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "a Content-Length over 100MB must be rejected before buffering: {}",
        resp.text()
    );
}

#[tokio::test]
async fn the_longest_matching_path_prefix_wins() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/models/detail");
        then.status(200).body("matched-deep");
    });

    let (router, _state) = build_router(base_config());
    let upstream = create(
        &router,
        "/oagw/v1/upstreams",
        &upstream_body("svc", server.port()),
    )
    .await;
    // Two routes could both match the request path; only their distinct
    // query allowlists let us prove which one actually won.
    let shallow = json!({
        "upstream_id": upstream["uuid"],
        "match": {"http": {"methods": ["GET"], "path": "/v1", "query_allowlist": ["a"]}},
    });
    let deep = json!({
        "upstream_id": upstream["uuid"],
        "match": {"http": {"methods": ["GET"], "path": "/v1/models", "query_allowlist": ["b"]}},
    });
    create(&router, "/oagw/v1/routes", &shallow).await;
    create(&router, "/oagw/v1/routes", &deep).await;

    let resp = send(
        &router,
        empty_request(Method::GET, "/oagw/v1/proxy/svc/v1/models/detail?b=1"),
    )
    .await;
    assert_eq!(
        resp.status,
        StatusCode::OK,
        "query `b` is only allowed on the deeper route, so a match proves it won: {}",
        resp.text()
    );
    assert_eq!(resp.text(), "matched-deep");
    assert_eq!(mock.calls(), 1);
}

#[tokio::test]
async fn a_common_suffix_alias_requires_target_host_and_then_routes_to_it() {
    // Both hostnames resolve to loopback via the RFC 6761 `*.localhost`
    // special case, so this reaches a real (local) server rather than a
    // fictitious one — see the final report for the assumption this relies on.
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/multi");
        then.status(200).body("reached");
    });

    let (router, _state) = build_router(base_config());
    let upstream_body = json!({
        "server": {"endpoints": [
            {"scheme": "http", "host": "us.vendor.localhost", "port": server.port()},
            {"scheme": "http", "host": "eu.vendor.localhost", "port": server.port()},
        ]},
        "protocol": PROTOCOL_HTTP,
    });
    let upstream = create(&router, "/oagw/v1/upstreams", &upstream_body).await;
    let alias = upstream["alias"]
        .as_str()
        .expect("alias present")
        .to_owned();
    assert!(
        alias.starts_with("vendor.localhost"),
        "alias must derive from the common registrable suffix: {alias}"
    );

    create(
        &router,
        "/oagw/v1/routes",
        &route_body(&upstream["uuid"], "/multi", &["GET"]),
    )
    .await;

    let without_header = send(
        &router,
        empty_request(Method::GET, &format!("/oagw/v1/proxy/{alias}/multi")),
    )
    .await;
    assert_eq!(
        without_header.status,
        StatusCode::BAD_REQUEST,
        "{}",
        without_header.text()
    );
    let problem_type = without_header.json()["type"]
        .as_str()
        .expect("type present")
        .to_owned();
    assert!(
        problem_type.ends_with("routing.missing_target_host.v1"),
        "unexpected problem type: {problem_type}"
    );

    let with_header = request(Method::GET, &format!("/oagw/v1/proxy/{alias}/multi"))
        .header("x-oagw-target-host", "us.vendor.localhost")
        .body(axum::body::Body::empty())
        .expect("well-formed request");
    let resp = send(&router, with_header).await;
    assert_eq!(
        resp.status,
        StatusCode::OK,
        "a named endpoint must be reached once the header is supplied: {}",
        resp.text()
    );
    assert_eq!(resp.text(), "reached");
    assert_eq!(mock.calls(), 1);
}

#[tokio::test]
async fn post_with_a_json_body_reaches_the_upstream_intact() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/submit")
            .json_body(json!({"key": "value"}));
        then.status(201).body("stored");
    });

    let (router, _state) = build_router(base_config());
    let upstream = create(
        &router,
        "/oagw/v1/upstreams",
        &upstream_body("svc", server.port()),
    )
    .await;
    create(
        &router,
        "/oagw/v1/routes",
        &route_body(&upstream["uuid"], "/submit", &["POST"]),
    )
    .await;

    let resp = send(
        &router,
        json_request(
            Method::POST,
            "/oagw/v1/proxy/svc/submit",
            &json!({"key": "value"}),
        ),
    )
    .await;
    assert_eq!(
        resp.status,
        StatusCode::CREATED,
        "the mock only matches an intact body: {}",
        resp.text()
    );
    assert_eq!(mock.calls(), 1);
}
