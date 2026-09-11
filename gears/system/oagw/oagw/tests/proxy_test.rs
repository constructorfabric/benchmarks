//! Proxy API integration tests: the data-plane behaviour an API consumer
//! sees, driven through the gear's own router with a real upstream behind it.
//!
//! `httpmock` 0.8 does not expose a call log, so what the upstream received is
//! asserted with matchers: a mock that only answers the expected shape, plus a
//! catch-all mock on the same path that answers `500`. If the catch-all is
//! hit, the request did not look the way the test demands.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use axum::http::{StatusCode, header};
use common::Harness;
use httpmock::Method::POST;
use httpmock::{Mock, MockServer};
use serde_json::{Value, json};

/// Seeds an upstream on the mock server with one route.
async fn seeded(harness: &Harness, server: &MockServer) -> Value {
    let upstream = harness.seed_upstream("api.example.com", server).await;
    let id = upstream["id"].as_str().unwrap().to_owned();
    harness.seed_route(&id, "/v1/chat", &["POST"]).await;
    upstream
}

/// A mock that answers every POST to the path with `200`.
fn echo(server: &MockServer) -> Mock<'_> {
    server.mock(|when, then| {
        when.method(POST).path("/v1/chat");
        then.status(200).header("content-type", "application/json");
    })
}

#[tokio::test]
async fn a_proxied_request_reaches_the_upstream_and_returns_its_answer() {
    let server = MockServer::start();
    let harness = Harness::new(&server);
    seeded(&harness, &server).await;

    let upstream_answer = server.mock(|when, then| {
        when.method(POST).path("/v1/chat/completions");
        then.status(200)
            .header("content-type", "application/json")
            .json_body(json!({"id": "cmpl-1", "object": "chat.completion"}));
    });

    let (status, body) = harness
        .json(
            "POST",
            "/oagw/v1/proxy/api.example.com/v1/chat/completions?api-version=1",
            Some(json!({"model": "gpt-4o"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], json!("cmpl-1"));
    assert_eq!(upstream_answer.calls(), 1, "the upstream served the call");
}

#[tokio::test]
async fn the_query_string_is_forwarded() {
    let server = MockServer::start();
    let harness = Harness::new(&server);
    seeded(&harness, &server).await;

    let with_query = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/chat")
            .query_param_exists("api-version");
        then.status(200);
    });
    let without_query = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/chat")
            .query_param_missing("api-version");
        then.status(500);
    });

    let (status, _) = harness
        .json(
            "POST",
            "/oagw/v1/proxy/api.example.com/v1/chat?api-version=1",
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(with_query.calls(), 1);
    assert_eq!(
        without_query.calls(),
        0,
        "no query string was dropped or invented"
    );
}

#[tokio::test]
async fn the_upstream_status_and_content_type_pass_through() {
    let server = MockServer::start();
    let harness = Harness::new(&server);
    seeded(&harness, &server).await;

    server.mock(|when, then| {
        when.method(POST).path("/v1/chat");
        then.status(201).header("content-type", "application/json");
    });

    let (status, _, headers) = harness
        .json_with_headers(
            "POST",
            "/oagw/v1/proxy/api.example.com/v1/chat",
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        headers.get(header::CONTENT_TYPE).unwrap(),
        "application/json"
    );
    assert!(
        headers.get("x-request-id").is_some(),
        "the gateway stamps a request identifier"
    );
}

#[tokio::test]
async fn hop_by_hop_headers_do_not_reach_the_upstream() {
    let server = MockServer::start();
    let harness = Harness::new(&server);
    seeded(&harness, &server).await;

    // The `connection` header the downstream sent must be stripped before the
    // hop to the upstream; `host` must be rewritten to the endpoint.
    let clean = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/chat")
            .header_missing("connection")
            .header("host", format!("{}:{}", server.host(), server.port()));
        then.status(204);
    });
    let dirty = server.mock(|when, then| {
        when.method(POST).path("/v1/chat");
        then.status(500);
    });

    let (status, _, _) = harness
        .json_with_headers(
            "POST",
            "/oagw/v1/proxy/api.example.com/v1/chat",
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(clean.calls(), 1);
    assert_eq!(dirty.calls(), 0, "a hop-by-hop or host header survived");
}

#[tokio::test]
async fn a_configured_request_header_is_added() {
    let server = MockServer::start();
    let harness = Harness::new(&server);
    let upstream = seeded(&harness, &server).await;
    let id = upstream["id"].as_str().unwrap().to_owned();

    let (status, body) = harness
        .json(
            "PUT",
            &format!("/oagw/v1/upstreams/{id}"),
            Some(json!({
                "protocol": oagw::gts_helpers::PROTOCOL_HTTP,
                "server": {"endpoints": [
                    {"scheme": "http", "host": server.host(), "port": server.port()}
                ]},
                "headers": {"request": {"add": {"x-tenant-mark": "acme"}}}
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let marked = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/chat")
            .header("x-tenant-mark", "acme");
        then.status(200);
    });
    let unmarked = server.mock(|when, then| {
        when.method(POST).path("/v1/chat");
        then.status(500);
    });

    let (status, _) = harness
        .json(
            "POST",
            "/oagw/v1/proxy/api.example.com/v1/chat",
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(marked.calls(), 1);
    assert_eq!(unmarked.calls(), 0, "the configured header was not added");
}

#[tokio::test]
async fn an_unknown_alias_is_a_404_problem_document() {
    let server = MockServer::start();
    let harness = Harness::new(&server);

    let (status, problem) = harness
        .json("GET", "/oagw/v1/proxy/no-such-alias/v1/things", None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{problem}");
    assert_eq!(
        problem["type"],
        json!(oagw::gts_helpers::error_type_id("route.not_found"))
    );
    assert_eq!(problem["status"], json!(404));
    assert_eq!(
        problem["instance"],
        json!("/oagw/v1/proxy/no-such-alias/v1/things")
    );
}

#[tokio::test]
async fn a_request_without_a_matching_route_is_a_404() {
    let server = MockServer::start();
    let harness = Harness::new(&server);
    seeded(&harness, &server).await;

    let (status, problem) = harness
        .json("DELETE", "/oagw/v1/proxy/api.example.com/v1/chat", None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{problem}");
}

#[tokio::test]
async fn a_disabled_upstream_is_a_503() {
    let server = MockServer::start();
    let harness = Harness::new(&server);
    let upstream = seeded(&harness, &server).await;
    let id = upstream["id"].as_str().unwrap().to_owned();

    let (status, _) = harness
        .json(
            "PUT",
            &format!("/oagw/v1/upstreams/{id}"),
            Some(json!({
                "enabled": false,
                "protocol": oagw::gts_helpers::PROTOCOL_HTTP,
                "server": {"endpoints": [
                    {"scheme": "http", "host": server.host(), "port": server.port()}
                ]}
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let (status, problem) = harness
        .json(
            "POST",
            "/oagw/v1/proxy/api.example.com/v1/chat",
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{problem}");
    assert_eq!(
        problem["type"],
        json!(oagw::gts_helpers::error_type_id("link.unavailable"))
    );
}

#[tokio::test]
async fn an_upstream_failure_is_reported_with_the_upstream_error_source() {
    let server = MockServer::start();
    let harness = Harness::new(&server);
    seeded(&harness, &server).await;

    server.mock(|when, then| {
        when.method(POST).path("/v1/chat");
        then.status(500).body("the model is on fire");
    });

    let (_, problem, headers) = harness
        .json_with_headers(
            "POST",
            "/oagw/v1/proxy/api.example.com/v1/chat",
            Some(json!({})),
        )
        .await;
    assert_eq!(
        headers
            .get("x-oagw-error-source")
            .map(|value| value.to_str().unwrap_or_default()),
        Some("upstream"),
        "the upstream answered, so the source is upstream: {problem}"
    );
}

#[tokio::test]
async fn a_target_host_outside_the_pool_is_rejected() {
    let server = MockServer::start();
    let harness = Harness::new(&server);
    seeded(&harness, &server).await;

    let problem = harness
        .send_json(common::raw_request(
            "POST",
            "/oagw/v1/proxy/api.example.com/v1/chat",
            &[("x-oagw-target-host", "evil.example.com")],
            Some(json!({})),
        ))
        .await;
    assert_eq!(problem["status"], json!(400), "{problem}");
    assert_eq!(
        problem["type"],
        json!(oagw::gts_helpers::error_type_id(
            "routing.unknown_target_host"
        ))
    );
}

#[tokio::test]
async fn a_target_host_that_is_not_a_bare_host_is_invalid() {
    let server = MockServer::start();
    let harness = Harness::new(&server);
    seeded(&harness, &server).await;

    for value in ["127.0.0.1:8080", "127.0.0.1/path"] {
        let problem = harness
            .send_json(common::raw_request(
                "POST",
                "/oagw/v1/proxy/api.example.com/v1/chat",
                &[("x-oagw-target-host", value)],
                Some(json!({})),
            ))
            .await;
        assert_eq!(problem["status"], json!(400), "{value}: {problem}");
        assert_eq!(
            problem["type"],
            json!(oagw::gts_helpers::error_type_id(
                "routing.invalid_target_host"
            )),
            "{value}: {problem}"
        );
        assert_eq!(
            problem["instance"],
            json!("/oagw/v1/proxy/api.example.com/v1/chat"),
            "{value}: {problem}"
        );
    }
}

#[tokio::test]
async fn a_pool_whose_alias_is_its_common_suffix_demands_a_target_host() {
    let server = MockServer::start();
    let harness = Harness::new(&server);
    // Two endpoints sharing a registrable suffix, with the suffix as the
    // alias: ADR-0001 makes the header mandatory for exactly this pool.
    let (status, upstream) = harness
        .json(
            "POST",
            "/oagw/v1/upstreams",
            Some(json!({
                "alias": "vendor.com",
                "protocol": oagw::gts_helpers::PROTOCOL_HTTP,
                "server": {"endpoints": [
                    {"scheme": "http", "host": "us.vendor.com", "port": 80},
                    {"scheme": "http", "host": "eu.vendor.com", "port": 80}
                ]}
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let id = upstream["id"].as_str().unwrap().to_owned();
    harness.seed_route(&id, "/v1/chat", &["POST"]).await;

    let problem = harness
        .send_json(common::raw_request(
            "POST",
            "/oagw/v1/proxy/vendor.com/v1/chat",
            &[],
            Some(json!({})),
        ))
        .await;
    assert_eq!(problem["status"], json!(400), "{problem}");
    assert_eq!(
        problem["type"],
        json!(oagw::gts_helpers::error_type_id(
            "routing.missing_target_host"
        ))
    );
    assert_eq!(
        problem["instance"],
        json!("/oagw/v1/proxy/vendor.com/v1/chat"),
        "{problem}"
    );
}

#[tokio::test]
async fn an_explicit_target_host_inside_the_pool_is_honoured() {
    let server = MockServer::start();
    let harness = Harness::new(&server);

    let upstream = seeded(&harness, &server).await;
    let _ = upstream;

    let echo = echo(&server);
    // The header names a bare host; the port comes from the pool it resolves
    // inside (ADR-0007: no port, path or special characters).
    let problem = harness
        .send_json(common::raw_request(
            "POST",
            "/oagw/v1/proxy/api.example.com/v1/chat",
            &[("x-oagw-target-host", &server.host())],
            Some(json!({})),
        ))
        .await;
    assert_eq!(echo.calls(), 1, "the named endpoint answered: {problem}");
}
