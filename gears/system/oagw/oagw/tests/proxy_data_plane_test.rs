// Created: 2026-09-03 by Constructor Tech
//! Data-plane integration tests against a real local HTTP upstream.
//!
//! The gateway is driven with `Router::oneshot`; the upstream is a real
//! `httpmock` server on 127.0.0.1, registered through the management API with
//! `oagw.config.allow_http_upstream` enabled.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::missing_panics_doc,
    clippy::large_types_passed_by_value,
    clippy::redundant_clone
)]

mod common;

use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Response, header};
use httpmock::{Mock, MockServer, Method as UpstreamMethod};
use serde_json::json;
use uuid::Uuid;

use common::{PROTOCOL_HTTP, TENANT_A, TENANT_B};
use toolkit_security::SecurityContext;

/// Alias used for every upstream registered in these tests.
const ALIAS: &str = "local-upstream";

// ── Harness ─────────────────────────────────────────────────────────────

/// A gateway wired to one mock upstream through a single plaintext endpoint.
struct Gateway {
    app: Router,
    upstream_id: Uuid,
    /// `host:port` of the single configured endpoint.
    authority: String,
}

impl Gateway {
    /// Register an enabled plaintext upstream pointing at `server`.
    async fn new(server: &MockServer) -> Self {
        let (app, upstream_id) = Self::register(server, true).await;
        Self {
            app,
            upstream_id,
            authority: format!("{}:{}", server.host(), server.port()),
        }
    }

    /// Register the upstream through the management API.
    ///
    /// The endpoint host is an IP literal, so an explicit alias is required.
    async fn register(server: &MockServer, enabled: bool) -> (Router, Uuid) {
        let app = common::app(true);
        let created = common::expect_json(
            common::post(
                "/oagw/v1/upstreams",
                json!({
                    "alias": ALIAS,
                    "enabled": enabled,
                    "server": { "endpoints": [
                        { "scheme": "http", "host": server.host(), "port": server.port() }
                    ]},
                    "protocol": PROTOCOL_HTTP
                }),
            )
            .send(app.clone())
            .await,
            201,
        )
        .await;
        assert_eq!(created["alias"], ALIAS);
        let upstream_id: Uuid = created["id"].as_str().expect("id").parse().expect("uuid");
        (app, upstream_id)
    }

    /// Add a route on `/path` for `methods`.
    async fn route(&self, methods: &[&str], path: &str, suffix_mode: &str, allowlist: &[&str]) {
        let response = common::post(
            "/oagw/v1/routes",
            json!({
                "upstream_id": self.upstream_id,
                "match": { "http": {
                    "methods": methods,
                    "path": path,
                    "query_allowlist": allowlist,
                    "path_suffix_mode": suffix_mode
                }}
            }),
        )
        .send(self.app.clone())
        .await;
        common::expect_json(response, 201).await;
    }

    /// Proxy `{method} /oagw/v1/proxy/{alias}/{suffix}` with extra headers.
    async fn proxy(
        &self,
        method: Method,
        suffix: &str,
        headers: &[(&'static str, &str)],
    ) -> Response<Body> {
        let mut outgoing = common::Outgoing::new(method, format!("/oagw/v1/proxy/{ALIAS}/{suffix}"))
            .header("host", "client.example.com");
        for (name, value) in headers {
            outgoing = outgoing.header(name, *value);
        }
        outgoing.send(self.app.clone()).await
    }

    /// Proxy the bare alias (no path suffix).
    async fn proxy_root(&self, method: Method) -> Response<Body> {
        common::Outgoing::new(method, format!("/oagw/v1/proxy/{ALIAS}"))
            .header("host", "client.example.com")
            .send(self.app.clone())
            .await
    }
}

/// A mock that answers when a header the gateway must strip reaches it.
fn mock_leaked<'a>(server: &'a MockServer, name: &'static str, value: &'static str) -> Mock<'a> {
    server.mock(|when, then| {
        when.method(UpstreamMethod::GET).path("/v1/x").header(name, value);
        then.status(200).body("leaked");
    })
}

/// A catch-all mock for `GET path` returning `200 OK`.
fn mock_ok<'a>(server: &'a MockServer, path: &'a str) -> Mock<'a> {
    server.mock(|when, then| {
        when.method(UpstreamMethod::GET).path(path);
        then.status(200)
            .header("content-type", "text/plain")
            .header("x-upstream", "mock-1")
            .body("upstream-body");
    })
}

/// Assert that a request the upstream must never see did not happen.
fn assert_no_hits(mocks: &[&Mock]) {
    for mock in mocks {
        assert_eq!(mock.calls(), 0, "the upstream must not have been reached");
    }
}

/// Assert `status`/`body` of a proxied response and return the response headers.
async fn assert_body(response: Response<Body>, expected_status: u16, expected_body: &str) {
    let headers = response.headers().clone();
    let (status, body) = common::drain(response).await;
    assert_eq!(
        status.as_u16(),
        expected_status,
        "upstream said: {}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(body, expected_body);
    assert_ne!(
        headers.get(common::ERROR_SOURCE_HEADER).and_then(|v| v.to_str().ok()),
        Some(common::ERROR_SOURCE_GATEWAY),
        "a passthrough response must not be tagged as a gateway error"
    );
}

// ── Passthrough ─────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn get_request_is_forwarded_to_the_upstream() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(UpstreamMethod::GET).path("/v1/models");
        then.status(200)
            .header("content-type", "application/json")
            .header("x-upstream", "mock-1")
            .body(r#"{"models":[]}"#);
    });
    let gw = Gateway::new(&server).await;
    gw.route(&["GET"], "/", "append", &[]).await;

    let response = gw.proxy(Method::GET, "v1/models", &[]).await;
    assert_eq!(
        response
            .headers()
            .get("x-upstream")
            .and_then(|v| v.to_str().ok()),
        Some("mock-1"),
        "upstream response headers are passed through"
    );
    assert_body(response, 200, r#"{"models":[]}"#).await;
    assert_eq!(mock.calls(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn upstream_status_and_content_type_are_passed_through() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(UpstreamMethod::GET).path("/v1/stream");
        then.status(201)
            .header("content-type", "text/event-stream")
            .delay(Duration::from_millis(20))
            .body("data: one\n\ndata: two\n\n");
    });
    let gw = Gateway::new(&server).await;
    gw.route(&["GET"], "/", "append", &[]).await;

    let response = gw.proxy(Method::GET, "v1/stream", &[]).await;
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream")
    );
    assert_body(response, 201, "data: one\n\ndata: two\n\n").await;
    assert_eq!(mock.calls(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn client_host_header_is_replaced_by_the_endpoint_authority() {
    let server = MockServer::start();
    let gw = Gateway::new(&server).await;
    let authority = gw.authority.clone();
    // Registered first so it wins the first-match search over the catch-all.
    let host_mock = server.mock(|when, then| {
        when.method(UpstreamMethod::GET)
            .path("/v1/x")
            .header("host", authority);
        then.status(200).body("host-rewritten");
    });
    gw.route(&["GET"], "/", "append", &[]).await;

    let response = gw.proxy(Method::GET, "v1/x", &[]).await;
    assert_body(response, 200, "host-rewritten").await;
    assert_eq!(
        host_mock.calls(),
        1,
        "the upstream must see the endpoint authority, not the client Host"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn hop_by_hop_and_routing_headers_never_reach_the_upstream() {
    let server = MockServer::start();
    let seen = server.mock(|when, then| {
        when.method(UpstreamMethod::GET).path("/v1/x");
        then.status(200).body("ok");
    });
    let forbidden = [
        mock_leaked(&server, "connection", "keep-alive"),
        mock_leaked(&server, "keep-alive", "timeout=5"),
        mock_leaked(&server, "transfer-encoding", "chunked"),
        mock_leaked(&server, "upgrade", "h2c"),
        mock_leaked(&server, "x-oagw-target-host", "127.0.0.1"),
        mock_leaked(&server, "host", "client.example.com"),
    ];
    let gw = Gateway::new(&server).await;
    gw.route(&["GET"], "/", "append", &[]).await;

    let response = gw
        .proxy(
            Method::GET,
            "v1/x",
            &[
                ("connection", "keep-alive"),
                ("keep-alive", "timeout=5"),
                ("transfer-encoding", "chunked"),
                ("upgrade", "h2c"),
                ("x-oagw-target-host", "127.0.0.1"),
                ("x-kept", "value"),
            ],
        )
        .await;
    assert_body(response, 200, "ok").await;
    assert_eq!(seen.calls(), 1, "exactly one request must reach the upstream");
    assert_no_hits(&forbidden.iter().collect::<Vec<_>>());
}

#[tokio::test(flavor = "multi_thread")]
async fn inbound_headers_are_not_forwarded_by_default() {
    let server = MockServer::start();
    let seen = server.mock(|when, then| {
        when.method(UpstreamMethod::GET).path("/v1/x");
        then.status(200).body("ok");
    });
    let leaked = mock_leaked(&server, "authorization", "Bearer client-token");
    let gw = Gateway::new(&server).await;
    gw.route(&["GET"], "/", "append", &[]).await;

    let response = gw
        .proxy(Method::GET, "v1/x", &[("authorization", "Bearer client-token")])
        .await;
    assert_body(response, 200, "ok").await;
    assert_eq!(seen.calls(), 1);
    assert_no_hits(&[&leaked]);
}

#[tokio::test(flavor = "multi_thread")]
async fn allowed_query_parameters_are_forwarded() {
    let server = MockServer::start();
    let with_query = server.mock(|when, then| {
        when.method(UpstreamMethod::GET)
            .path("/v1/models")
            .query_param("model", "gpt");
        then.status(200).body("filtered");
    });
    let gw = Gateway::new(&server).await;
    gw.route(&["GET"], "/", "append", &["model"]).await;

    let response =
        common::Outgoing::new(Method::GET, format!("/oagw/v1/proxy/{ALIAS}/v1/models?model=gpt"))
            .send(gw.app.clone())
            .await;
    assert_body(response, 200, "filtered").await;
    assert_eq!(with_query.calls(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn disallowed_query_parameter_is_rejected() {
    let server = MockServer::start();
    let seen = server.mock(|when, then| {
        when.method(UpstreamMethod::GET).path("/v1/models");
        then.status(200).body("ok");
    });
    let gw = Gateway::new(&server).await;
    gw.route(&["GET"], "/", "append", &["model"]).await;

    let response = common::Outgoing::new(
        Method::GET,
        format!("/oagw/v1/proxy/{ALIAS}/v1/models?model=gpt&unknown=1"),
    )
    .send(gw.app.clone())
    .await;
    common::expect_problem(
        response,
        400,
        "Validation Error",
        "'unknown' is not allowed by this route",
        &["query"],
    )
    .await;
    assert_no_hits(&[&seen]);
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_query_allowlist_forbids_every_parameter() {
    let server = MockServer::start();
    let seen = server.mock(|when, then| {
        when.method(UpstreamMethod::GET).path("/v1/models");
        then.status(200).body("ok");
    });
    let gw = Gateway::new(&server).await;
    gw.route(&["GET"], "/", "append", &[]).await;

    let rejected =
        common::Outgoing::new(Method::GET, format!("/oagw/v1/proxy/{ALIAS}/v1/models?model=gpt"))
            .send(gw.app.clone())
            .await;
    common::expect_problem(rejected, 400, "Validation Error", "'model' is not allowed", &["query"])
        .await;
    assert_no_hits(&[&seen]);

    let allowed = common::Outgoing::new(Method::GET, format!("/oagw/v1/proxy/{ALIAS}/v1/models"))
        .send(gw.app.clone())
        .await;
    assert_body(allowed, 200, "ok").await;
    assert_eq!(seen.calls(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn post_body_is_forwarded_to_the_upstream() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(UpstreamMethod::POST)
            .path("/v1/chat")
            .body(r#"{"prompt":"hi"}"#);
        then.status(201).header("x-upstream", "created").body("accepted");
    });
    let gw = Gateway::new(&server).await;
    gw.route(&["POST"], "/", "append", &[]).await;

    let response = common::Outgoing::new(Method::POST, format!("/oagw/v1/proxy/{ALIAS}/v1/chat"))
        .header("content-type", "application/json")
        .json(json!({ "prompt": "hi" }))
        .send(gw.app.clone())
        .await;
    assert_body(response, 201, "accepted").await;
    assert_eq!(mock.calls(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_request_returns_204() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(UpstreamMethod::DELETE).path("/v1/items/7");
        then.status(204);
    });
    let gw = Gateway::new(&server).await;
    gw.route(&["DELETE"], "/", "append", &[]).await;

    let response = gw.proxy(Method::DELETE, "v1/items/7", &[]).await;
    let (status, body) = common::drain(response).await;
    assert_eq!(status.as_u16(), 204);
    assert!(body.is_empty());
    assert_eq!(mock.calls(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn head_request_is_forwarded() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(UpstreamMethod::HEAD).path("/v1/items/7");
        then.status(200).header("x-upstream", "head");
    });
    let gw = Gateway::new(&server).await;
    gw.route(&["HEAD"], "/", "append", &[]).await;

    let response = gw.proxy(Method::HEAD, "v1/items/7", &[]).await;
    assert_eq!(
        response
            .headers()
            .get("x-upstream")
            .and_then(|v| v.to_str().ok()),
        Some("head")
    );
    let (status, body) = common::drain(response).await;
    assert_eq!(status.as_u16(), 200);
    assert!(body.is_empty(), "HEAD must not carry a body");
    assert_eq!(mock.calls(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn put_and_patch_are_forwarded() {
    let server = MockServer::start();
    let put_mock = server.mock(|when, then| {
        when.method(UpstreamMethod::PUT).path("/v1/items/7");
        then.status(200).body("put-ok");
    });
    let patch_mock = server.mock(|when, then| {
        when.method(UpstreamMethod::PATCH).path("/v1/items/7");
        then.status(200).body("patch-ok");
    });
    let gw = Gateway::new(&server).await;
    gw.route(&["PUT", "PATCH"], "/", "append", &[]).await;

    assert_body(gw.proxy(Method::PUT, "v1/items/7", &[]).await, 200, "put-ok").await;
    assert_body(gw.proxy(Method::PATCH, "v1/items/7", &[]).await, 200, "patch-ok").await;
    assert_eq!(put_mock.calls(), 1);
    assert_eq!(patch_mock.calls(), 1);
}

// ── Path suffix handling ────────────────────────────────────────────────

/// The proxy suffix is appended to the route pattern, so a route on `/v1`
/// reached with the suffix `v1/chat/completions` targets
/// `/v1/v1/chat/completions` upstream.
#[tokio::test(flavor = "multi_thread")]
async fn path_suffix_is_appended_to_the_route_pattern() {
    let server = MockServer::start();
    let deep = server.mock(|when, then| {
        when.method(UpstreamMethod::GET).path("/v1/v1/chat/completions");
        then.status(200).body("deep");
    });
    let gw = Gateway::new(&server).await;
    gw.route(&["GET"], "/v1", "append", &[]).await;

    let response = gw.proxy(Method::GET, "v1/chat/completions", &[]).await;
    assert_body(response, 200, "deep").await;
    assert_eq!(deep.calls(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_bare_alias_targets_the_route_pattern_root() {
    let server = MockServer::start();
    let root = server.mock(|when, then| {
        when.method(UpstreamMethod::GET).path("/");
        then.status(200).body("root");
    });
    let gw = Gateway::new(&server).await;
    gw.route(&["GET"], "/", "append", &[]).await;

    let response = gw.proxy_root(Method::GET).await;
    assert_body(response, 200, "root").await;
    assert_eq!(root.calls(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn path_suffix_mode_disabled_rejects_a_suffix() {
    let server = MockServer::start();
    let seen = mock_ok(&server, "/fixed");
    let gw = Gateway::new(&server).await;
    gw.route(&["GET"], "/fixed", "disabled", &[]).await;

    let response = gw.proxy(Method::GET, "fixed/extra", &[]).await;
    common::expect_problem(
        response,
        400,
        "Validation Error",
        "this route does not accept a path suffix (path_suffix_mode: disabled)",
        &[],
    )
    .await;
    assert_no_hits(&[&seen]);
}

#[tokio::test(flavor = "multi_thread")]
async fn path_suffix_mode_disabled_accepts_the_bare_alias() {
    let server = MockServer::start();
    let seen = server.mock(|when, then| {
        when.method(UpstreamMethod::GET).path("/");
        then.status(200).body("ok");
    });
    let gw = Gateway::new(&server).await;
    gw.route(&["GET"], "/", "disabled", &[]).await;

    assert_body(gw.proxy_root(Method::GET).await, 200, "ok").await;
    assert_eq!(seen.calls(), 1);
}

// ── Routing failures ────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn unknown_alias_returns_404() {
    let server = MockServer::start();
    let seen = mock_ok(&server, "/v1");
    let app = common::app(true);

    let response = common::Outgoing::new(Method::GET, "/oagw/v1/proxy/no-such-upstream/v1")
        .send(app.clone())
        .await;
    common::expect_problem(
        response,
        404,
        "Route Not Found",
        "no upstream is registered for alias 'no-such-upstream'",
        &[],
    )
    .await;
    assert_no_hits(&[&seen]);
}

#[tokio::test(flavor = "multi_thread")]
async fn unmatched_method_returns_404() {
    let server = MockServer::start();
    let seen = mock_ok(&server, "/v1/items");
    let gw = Gateway::new(&server).await;
    gw.route(&["GET"], "/v1", "append", &[]).await;

    let response = gw.proxy(Method::POST, "items", &[]).await;
    common::expect_problem(response, 404, "Route Not Found", "no route matched POST items", &[]).await;
    assert_no_hits(&[&seen]);
}

#[tokio::test(flavor = "multi_thread")]
async fn unmatched_path_returns_404() {
    let server = MockServer::start();
    let seen = mock_ok(&server, "/v1/other");
    let gw = Gateway::new(&server).await;
    gw.route(&["GET"], "/v1", "append", &[]).await;

    let response = gw.proxy(Method::GET, "other", &[]).await;
    common::expect_problem(response, 404, "Route Not Found", "no route matched GET other", &[])
        .await;
    assert_no_hits(&[&seen]);
}

#[tokio::test(flavor = "multi_thread")]
async fn disabled_upstream_is_not_proxyable() {
    let server = MockServer::start();
    let seen = mock_ok(&server, "/v1");
    let (app, upstream_id) = Gateway::register(&server, false).await;
    common::expect_json(
        common::post(
            "/oagw/v1/routes",
            json!({
                "upstream_id": upstream_id,
                "match": { "http": { "methods": ["GET"], "path": "/v1" } }
            }),
        )
        .send(app.clone())
        .await,
        201,
    )
    .await;

    let response = common::Outgoing::new(Method::GET, format!("/oagw/v1/proxy/{ALIAS}/v1"))
        .send(app)
        .await;
    common::expect_problem(response, 503, "Upstream Disabled", "is disabled", &[]).await;
    assert_no_hits(&[&seen]);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_alias_is_resolvable_only_by_the_owner_tenant() {
    let server = MockServer::start();
    let seen = mock_ok(&server, "/v1");
    let gw = Gateway::new(&server).await;
    gw.route(&["GET"], "/", "append", &[]).await;

    let foreign = common::Outgoing::new(Method::GET, format!("/oagw/v1/proxy/{ALIAS}/v1"))
        .tenant(TENANT_B)
        .send(gw.app.clone())
        .await;
    common::expect_problem(foreign, 404, "Route Not Found", "no upstream is registered for alias", &[])
        .await;
    assert_no_hits(&[&seen]);

    let owner = common::Outgoing::new(Method::GET, format!("/oagw/v1/proxy/{ALIAS}/v1"))
        .tenant(TENANT_A)
        .send(gw.app.clone())
        .await;
    assert_body(owner, 200, "upstream-body").await;
    assert_eq!(seen.calls(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn proxy_target_host_pinning_selects_the_endpoint() {
    let server = MockServer::start();
    let seen = server.mock(|when, then| {
        when.method(UpstreamMethod::GET).path("/v1");
        then.status(200).body("pinned");
    });
    let gw = Gateway::new(&server).await;
    gw.route(&["GET"], "/", "append", &[]).await;

    let response = gw
        .proxy(Method::GET, "v1", &[("x-oagw-target-host", &server.host())])
        .await;
    assert_body(response, 200, "pinned").await;
    assert_eq!(seen.calls(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_target_host_is_rejected() {
    let server = MockServer::start();
    let seen = mock_ok(&server, "/v1");
    let gw = Gateway::new(&server).await;
    gw.route(&["GET"], "/", "append", &[]).await;

    let response = gw
        .proxy(Method::GET, "v1", &[("x-oagw-target-host", "elsewhere.example.com")])
        .await;
    common::expect_problem(
        response,
        400,
        "Unknown Target Host",
        "does not match any configured endpoint",
        &[],
    )
    .await;
    assert_no_hits(&[&seen]);
}

#[tokio::test(flavor = "multi_thread")]
async fn oversized_request_body_is_rejected() {
    let server = MockServer::start();
    let seen = mock_ok(&server, "/v1");
    let gw = Gateway::new(&server).await;
    gw.route(&["POST"], "/", "append", &[]).await;

    // The gateway rejects the declared length before buffering the body.
    let too_big = 100 * 1024 * 1024 + 1;
    let response = common::Outgoing::new(Method::POST, format!("/oagw/v1/proxy/{ALIAS}/v1"))
        .header("content-length", too_big.to_string())
        .header("content-type", "text/plain")
        .send(gw.app.clone())
        .await;
    common::expect_problem(response, 413, "Payload Too Large", "exceeds the", &[]).await;
    assert_no_hits(&[&seen]);
}

// ── CORS preflight ──────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn options_preflight_returns_204_without_reaching_the_upstream() {
    let server = MockServer::start();
    let seen = mock_ok(&server, "/v1");
    let gw = Gateway::new(&server).await;
    gw.route(&["GET"], "/", "append", &[]).await;

    for uri in [format!("/oagw/v1/proxy/{ALIAS}"), format!("/oagw/v1/proxy/{ALIAS}/v1")] {
        let response = common::Outgoing::new(Method::OPTIONS, uri)
            .send(gw.app.clone())
            .await;
        assert_eq!(response.status().as_u16(), 204);
        let headers = response.headers().clone();
        assert_eq!(
            headers
                .get("access-control-allow-origin")
                .and_then(|v| v.to_str().ok()),
            Some("*")
        );
        assert_eq!(
            headers
                .get("access-control-allow-methods")
                .and_then(|v| v.to_str().ok()),
            Some("GET, POST, PUT, PATCH, DELETE, HEAD, OPTIONS")
        );
        assert_eq!(
            headers
                .get("access-control-allow-headers")
                .and_then(|v| v.to_str().ok()),
            Some("*")
        );
        assert_eq!(
            headers
                .get("access-control-max-age")
                .and_then(|v| v.to_str().ok()),
            Some("600")
        );
        assert_eq!(headers.get("vary").and_then(|v| v.to_str().ok()), Some("Origin"));
        let (_, body) = common::drain(response).await;
        assert!(body.is_empty());
    }
    assert_no_hits(&[&seen]);
}

// ── WebSocket upgrade ──────────────────────────────────────────────────

/// Minimal RFC 6455 echo upstream on raw TCP — `httpmock` cannot complete a
/// protocol upgrade.
///
/// The handshake echoes the `Sec-WebSocket-Key` it received inside
/// `Sec-WebSocket-Accept`, so the client can assert that the gateway forwarded
/// the field: RFC 6455 §4.2.2 derives the accept from the client key, so a
/// gateway that drops it breaks every real client.
async fn spawn_websocket_upstream() -> std::net::SocketAddr {
    use tokio::io::AsyncWriteExt;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { break };
            tokio::spawn(async move {
                let head = read_until_head_end(&mut socket).await;
                let key = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.trim()
                            .eq_ignore_ascii_case("sec-websocket-key")
                            .then(|| value.trim().to_owned())
                    })
                    .unwrap_or_default();
                let response = format!(
                    "HTTP/1.1 101 Switching Protocols\r\n\
                     upgrade: websocket\r\n\
                     connection: Upgrade\r\n\
                     sec-websocket-accept: derived-from-{key}\r\n\r\n"
                );
                if socket.write_all(response.as_bytes()).await.is_err() {
                    return;
                }
                // Greet before the client sends anything, which proves the
                // upstream → client leg of the splice on its own.
                if send_text(&mut socket, "hello-from-upstream").await.is_err() {
                    return;
                }
                // Echo the first client frame back with an `echo:` prefix.
                let Some(frame) = read_frame(&mut socket).await else { return };
                let echoed = format!("echo:{frame}");
                let _ = send_text(&mut socket, &echoed).await;
            });
        }
    });
    addr
}

/// Read an HTTP head, terminated by the empty line.
async fn read_until_head_end(socket: &mut tokio::net::TcpStream) -> String {
    use tokio::io::AsyncReadExt;

    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if socket.read_exact(&mut byte).await.is_err() {
            return String::new();
        }
        head.push(byte[0]);
    }
    String::from_utf8_lossy(&head).to_string()
}

/// Send one unmasked server text frame.
async fn send_text(socket: &mut tokio::net::TcpStream, text: &str) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;

    let payload = text.as_bytes();
    let mut frame = vec![0x81u8, payload.len() as u8];
    frame.extend_from_slice(payload);
    socket.write_all(&frame).await
}

/// Read one masked client frame and return its decoded text.
async fn read_frame(socket: &mut tokio::net::TcpStream) -> Option<String> {
    use tokio::io::AsyncReadExt;

    let mut header = [0u8; 2];
    socket.read_exact(&mut header).await.ok()?;
    let length = usize::from(header[1] & 0x7f);
    let mut rest = vec![0u8; 4 + length];
    socket.read_exact(&mut rest).await.ok()?;
    let payload = rest[4..]
        .iter()
        .enumerate()
        .map(|(index, byte)| byte ^ rest[index % 4])
        .collect::<Vec<u8>>();
    Some(String::from_utf8_lossy(&payload).to_string())
}

/// A masked client text frame, as a browser would send it.
///
/// The mask cycles per RFC 6455 §5.3, so payloads longer than four bytes stay
/// intact.
fn masked_text_frame(text: &str) -> Vec<u8> {
    let payload = text.as_bytes();
    let mask = [0x11, 0x22, 0x33, 0x44];
    let mut frame = vec![0x81u8, 0x80 | payload.len() as u8];
    frame.extend_from_slice(&mask);
    frame.extend(
        payload
            .iter()
            .enumerate()
            .map(|(index, byte)| byte ^ mask[index % 4]),
    );
    frame
}

#[tokio::test(flavor = "multi_thread")]
async fn websocket_handshake_and_frames_are_proxied_end_to_end() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let upstream = spawn_websocket_upstream().await;
    let app = common::app(true);
    let created = common::expect_json(
        common::post(
            "/oagw/v1/upstreams",
            json!({
                "alias": ALIAS,
                "server": { "endpoints": [
                    { "scheme": "http", "host": upstream.ip().to_string(), "port": upstream.port() }
                ]},
                "protocol": PROTOCOL_HTTP
            }),
        )
        .send(app.clone())
        .await,
        201,
    )
    .await;
    let upstream_id: Uuid = created["id"].as_str().expect("id").parse().expect("uuid");
    common::expect_json(
        common::post(
            "/oagw/v1/routes",
            json!({
                "upstream_id": upstream_id,
                "match": { "http": {
                    "methods": ["GET"],
                    "path": "/",
                    "query_allowlist": [],
                    "path_suffix_mode": "append"
                }}
            }),
        )
        .send(app.clone())
        .await,
        201,
    )
    .await;

    // The router is served on a real socket so hyper hands the gateway a
    // client-side `OnUpgrade`, which `Router::oneshot` cannot provide.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let gateway = listener.local_addr().expect("gateway addr");
    let ctx = SecurityContext::builder()
        .subject_id(TENANT_A)
        .subject_type("user")
        .subject_tenant_id(TENANT_A)
        .build()
        .expect("valid security context");
    let server = tokio::spawn(async move {
        axum::serve(listener, app.layer(axum::Extension(ctx))).await
    });

    let mut client = tokio::net::TcpStream::connect(gateway).await.expect("connect");
    // The RFC 6455 §1.3 example key.
    let key = "dGhlIHNhbXBsZSBub25jZQ==";
    let handshake = format!(
        "GET /oagw/v1/proxy/{ALIAS}/ws HTTP/1.1\r\n\
         host: gateway.local\r\n\
         upgrade: websocket\r\n\
         connection: Upgrade\r\n\
         sec-websocket-key: {key}\r\n\
         sec-websocket-version: 13\r\n\r\n"
    );
    client.write_all(handshake.as_bytes()).await.expect("handshake sent");
    let mut raw = vec![0u8; 4096];
    let read = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        client.read(&mut raw),
    )
    .await
    .expect("the gateway answered the handshake")
    .expect("handshake response");
    let response = String::from_utf8_lossy(&raw[..read]).to_string();
    assert!(
        response.starts_with("HTTP/1.1 101"),
        "the gateway must complete the upgrade, got: {response}"
    );
    assert!(
        response.contains(&format!("sec-websocket-accept: derived-from-{key}")),
        "Sec-WebSocket-Key must reach the upstream, got: {response}"
    );
    assert!(
        response.contains("upgrade: websocket"),
        "the 101 must stay an upgrade response, got: {response}"
    );

    // The upstream greeting arrives through the gateway before the client
    // sends anything, which proves the upstream → client splice leg.
    let mut greeting = vec![0u8; 2 + "hello-from-upstream".len()];
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        client.read_exact(&mut greeting),
    )
    .await
    .expect("the greeting arrived")
    .expect("greeting read");
    assert_eq!(
        &greeting[..2],
        &[0x81, "hello-from-upstream".len() as u8],
        "greeting must be a text frame"
    );
    assert_eq!(String::from_utf8_lossy(&greeting[2..]), "hello-from-upstream");

    // And the client → upstream leg carries frames back, echoed.
    client.write_all(&masked_text_frame("hello")).await.expect("frame sent");
    let mut header = [0u8; 2];
    tokio::time::timeout(std::time::Duration::from_secs(10), client.read_exact(&mut header))
        .await
        .expect("the echo arrived")
        .expect("echo header");
    let length = usize::from(header[1] & 0x7f);
    let mut payload = vec![0u8; length];
    client.read_exact(&mut payload).await.expect("echo payload");
    assert_eq!(String::from_utf8_lossy(&payload), "echo:hello");

    server.abort();
}
