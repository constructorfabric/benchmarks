// Created: 2026-09-03 by Constructor Tech
//! Data-plane integration tests.
//!
//! The gateway is served on a real listener and pointed at an `httpmock`
//! upstream, so the tests cover the full dial, forward, stream and error
//! mapping path of `/oagw/v1/proxy/{alias}`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::{
    assert_gateway_problem, assert_problem, router, route_match_value, security_context, send_json,
    state, state_with, tenant,
};
use http::{Method, StatusCode};
use httpmock::{Mock, MockServer};
use httpmock::prelude::*;
use http_body_util::BodyExt;
use tower::ServiceExt;

use serde_json::{Value, json};

const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
const ALIAS: &str = "mock.internal";

/// A running gateway with one HTTP upstream bound to the mock server.
struct Fixture {
    gateway: axum::Router,
    server: MockServer,
    upstream_id: String,
}

impl Fixture {
    /// Starts a mock server, registers an upstream pointing at it and creates
    /// a `GET /` route with the supplied extensions.
    async fn start(extra: Value) -> Self {
        Self::start_with(state(true), move |port| default_upstream(port).merge_extra(extra)).await
    }

    /// Starts a mock server with a caller-supplied state and upstream payload.
    ///
    /// `payload` receives the port of the mock server so that configuration
    /// sections such as an OAuth2 `token_url` can point at it.
    async fn start_with(
        gateway_state: Arc<oagw::state::OagwState>,
        payload: impl FnOnce(u16) -> Value,
    ) -> Self {
        let server = MockServer::start();
        let port = server.port();
        let app = router(gateway_state, security_context(tenant()));
        let document = payload(port);
        let (status, _, document) =
            send_json(app.clone(), Method::POST, "/oagw/v1/upstreams", Some(document)).await;
        assert_eq!(status, StatusCode::CREATED, "document: {document}");
        let upstream_id = document["id"].as_str().expect("id").to_owned();
        Self { gateway: app, server, upstream_id }
    }

    /// Creates a route accepting `methods` below `path`.
    async fn route(&self, methods: Vec<&str>, path: &str, extra: Value) -> Value {
        let payload = json!({
            "upstream_id": self.upstream_id,
            "match": route_match_value(methods, path)
        })
        .merge_extra(extra);
        let (status, _, document) =
            send_json(self.gateway.clone(), Method::POST, "/oagw/v1/routes", Some(payload)).await;
        assert_eq!(status, StatusCode::CREATED, "document: {document}");
        document
    }

    /// Registers a mock returning `status` with `body`.
    fn serve<'a>(&'a self, method: Method, path: &'a str, status: u16, body: &'static str) -> Mock<'a> {
        self.server.mock(move |when, then| {
            when.method(method.as_str()).path(path);
            then.status(status).body(body);
        })
    }
}

/// Small helper merging a second JSON object into a payload.
trait MergeExtra {
    fn merge_extra(self, extra: Value) -> Value;
}

/// The default single-endpoint upstream payload bound to `port`.
fn default_upstream(port: u16) -> Value {
    json!({
        "alias": ALIAS,
        "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": port } ] },
        "protocol": PROTOCOL_HTTP
    })
}

impl MergeExtra for Value {
    fn merge_extra(mut self, extra: Value) -> Value {
        if let (Some(target), Some(source)) = (self.as_object_mut(), extra.as_object()) {
            for (key, value) in source {
                target.insert(key.clone(), value.clone());
            }
        }
        self
    }
}

#[tokio::test]
async fn proxy_forwards_the_request_and_passes_the_response_through() {
    let fixture = Fixture::start(json!({})).await;
    fixture.route(vec!["GET"], "/v1", json!({})).await;
    let mock = fixture.serve(Method::GET, "/v1/pets", 200, "{\"ok\":true}");

    let (status, headers, document) = common::send_bytes(
        fixture.gateway.clone(),
        Method::GET,
        &format!("/oagw/v1/proxy/{ALIAS}/v1/pets"),
        b"",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {}", String::from_utf8_lossy(&document));
    assert_eq!(document, &b"{\"ok\":true}"[..]);
    assert_eq!(
        headers.get("x-oagw-error-source").map(|value| value.to_str().expect("ascii")),
        None,
        "a successful response must not be tagged as an error"
    );
    assert!(headers.contains_key("x-oagw-trace-id"));
    assert_eq!(mock.calls(), 1);
}

#[tokio::test]
async fn proxy_prefixes_the_configured_path_and_appends_the_suffix() {
    let fixture = Fixture::start(json!({})).await;
    fixture.route(vec!["GET"], "/api/v2", json!({})).await;
    let mock = fixture.server.mock(|when, then| {
        when.method(GET).path("/api/v2/items");
        then.status(200).body("ok");
    });

    // The proxy suffix is appended to the matched route prefix.
    let (status, _, _) = common::send_bytes(
        fixture.gateway.clone(),
        Method::GET,
        &format!("/oagw/v1/proxy/{ALIAS}/api/v2/items"),
        b"",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(mock.calls(), 1);

    // A path outside the prefix matches no route.
    let (status, _, _) = common::send_bytes(
        fixture.gateway.clone(),
        Method::GET,
        &format!("/oagw/v1/proxy/{ALIAS}/elsewhere"),
        b"",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(mock.calls(), 1);
}

#[tokio::test]
async fn apikey_credentials_are_injected() {
    let fixture = Fixture::start(json!({
        "auth": {
            "type": "apikey",
            "sharing": "private",
            "config": { "key": "secret-key", "header": "x-api-key", "prefix": "Bearer " }
        }
    }))
    .await;
    fixture.route(vec!["GET"], "/", json!({})).await;
    let mock = fixture.server.mock(|when, then| {
        when.method(GET).path("/").header("x-api-key", "Bearer secret-key");
        then.status(204);
    });

    let (status, _, _) = common::send_bytes(fixture.gateway.clone(), Method::GET, &format!("/oagw/v1/proxy/{ALIAS}"), b"").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(mock.calls(), 1);
}

#[tokio::test]
async fn header_transform_rules_apply_to_both_directions() {
    let fixture = Fixture::start(json!({
        "headers": {
            "request": {
                "passthrough": "all",
                "remove": ["x-stripped"],
                "add": { "x-added": "upstream" },
                "set": { "x-set": "upstream" }
            },
            "response": {
                "set": { "x-gateway": "present" },
                "remove": ["x-server"]
            }
        }
    }))
    .await;
    fixture.route(vec!["GET"], "/", json!({})).await;
    let mock = fixture.server.mock(|when, then| {
        when.method(GET)
            .path("/")
            .header_missing("x-stripped")
            .header("x-added", "upstream")
            .header("x-set", "upstream")
            .header_exists("x-trace-us");
        then.status(200)
            .header("x-server", "mock")
            .body("payload");
    });

    let request = http::Request::builder()
        .method(Method::GET)
        .uri(format!("/oagw/v1/proxy/{ALIAS}"))
        .header("x-stripped", "client")
        .header("x-added", "client")
        .header("x-trace-us", "1")
        .body(axum::body::Body::empty())
        .expect("request");
    let response = fixture.gateway.clone().oneshot(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers().clone();
    assert_eq!(headers.get("x-gateway").and_then(|value| value.to_str().ok()), Some("present"));
    assert_eq!(headers.get("x-server"), None);
    assert_eq!(mock.calls(), 1);
}

#[tokio::test]
async fn rate_limit_exhaustion_returns_429_with_retry_after() {
    let fixture = Fixture::start(json!({
        "rate_limit": {
            "sharing": "private",
            "algorithm": "token_bucket",
            "sustained": { "rate": 1, "window": "second" },
            "strategy": "reject",
            "scope": "tenant"
        }
    }))
    .await;
    fixture.route(vec!["GET"], "/", json!({})).await;
    let mock = fixture.server.mock(|when, then| {
        when.method(GET).path("/");
        then.status(200).body("ok");
    });

    let mut seen_429 = false;
    for _ in 0..4 {
        let (status, headers, document) =
            common::send_bytes(fixture.gateway.clone(), Method::GET, &format!("/oagw/v1/proxy/{ALIAS}"), b"").await;
        if u16::from(status) == 429 {
            let problem = serde_json::from_slice(&document).expect("problem");
            let kind = assert_problem(status, &headers, &problem, 429);
            assert_eq!(kind, "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1");
            assert!(headers.contains_key("retry-after"));
            assert!(problem["retry_after_seconds"].is_u64(), "problem: {problem}");
            seen_429 = true;
            break;
        }
    }
    assert!(seen_429, "the limiter never rejected a burst of four requests");
    assert!(mock.calls() <= 3);
}

#[tokio::test]
async fn unknown_alias_and_missing_route_render_distinct_404s() {
    let fixture = Fixture::start(json!({})).await;

    let (status, headers, document) =
        common::send_bytes(fixture.gateway.clone(), Method::GET, "/oagw/v1/proxy/does-not-exist", b"").await;
    let problem = serde_json::from_slice(&document).expect("problem");
    let kind = assert_problem(status, &headers, &problem, 404);
    assert_eq!(kind, "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1");
    assert_eq!(problem["host"], "does-not-exist");

    // The alias exists but no route matches the method.
    fixture.route(vec!["POST"], "/only-post", json!({})).await;
    let (status, headers, document) =
        common::send_bytes(fixture.gateway.clone(), Method::GET, &format!("/oagw/v1/proxy/{ALIAS}/only-post"), b"").await;
    let kind = assert_problem(status, &headers, &serde_json::from_slice(&document).expect("problem"), 404);
    assert_eq!(kind, "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1");
}

#[tokio::test]
async fn unreachable_upstreams_map_to_link_unavailable() {
    let app = router(state(true), security_context(tenant()));
    let payload = json!({
        "alias": "closed.local",
        "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": 1 } ] },
        "protocol": PROTOCOL_HTTP
    });
    let (status, _, upstream) = send_json(app.clone(), Method::POST, "/oagw/v1/upstreams", Some(payload)).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _, route) = send_json(
        app.clone(),
        Method::POST,
        "/oagw/v1/routes",
        Some(json!({
            "upstream_id": upstream["id"],
            "match": route_match_value(vec!["GET"], "/")
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "route: {route}");

    let (status, headers, document) =
        common::send_bytes(app, Method::GET, "/oagw/v1/proxy/closed.local", b"").await;
    let problem = serde_json::from_slice(&document).expect("problem");
    let kind = assert_problem(status, &headers, &problem, 503);
    assert_eq!(kind, "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1");
    assert_eq!(problem["upstream_id"], upstream["id"]);
}

#[tokio::test]
async fn upstream_errors_pass_through_with_the_upstream_source() {
    let fixture = Fixture::start(json!({})).await;
    fixture.route(vec!["GET"], "/", json!({})).await;
    fixture.serve(Method::GET, "/", 404, "{\"error\":\"missing\"}");

    let (status, headers, body) =
        common::send_bytes(fixture.gateway.clone(), Method::GET, &format!("/oagw/v1/proxy/{ALIAS}"), b"").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, &b"{\"error\":\"missing\"}"[..]);
    assert_eq!(
        headers.get("x-oagw-error-source").and_then(|value| value.to_str().ok()),
        Some("upstream")
    );
}

#[tokio::test]
async fn multi_endpoint_pools_require_a_target_host_header() {
    // A derivable multi-host pool must disambiguate its endpoint explicitly.
    let app = router(state(true), security_context(tenant()));
    let payload = json!({
        "server": {
            "endpoints": [
                { "scheme": "http", "host": "us.vendor.com", "port": 9100 },
                { "scheme": "http", "host": "eu.vendor.com", "port": 9100 }
            ]
        },
        "protocol": PROTOCOL_HTTP
    });
    let (status, _, upstream) = send_json(app.clone(), Method::POST, "/oagw/v1/upstreams", Some(payload)).await;
    assert_eq!(status, StatusCode::CREATED, "document: {upstream}");
    assert_eq!(upstream["alias"], "vendor.com:9100");
    let (status, _, _) = send_json(
        app.clone(),
        Method::POST,
        "/oagw/v1/routes",
        Some(json!({
            "upstream_id": upstream["id"],
            "match": route_match_value(vec!["GET"], "/")
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, headers, document) =
        common::send_bytes(app.clone(), Method::GET, "/oagw/v1/proxy/vendor.com:9100", b"").await;
    let problem = serde_json::from_slice(&document).expect("problem");
    let kind = assert_problem(status, &headers, &problem, 400);
    assert_eq!(kind, "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1");

    // A foreign host is rejected, as is a value that is not a host name.
    for (value, kind) in [
        ("elsewhere.example.com", "routing.unknown_target_host"),
        ("bad host!", "routing.invalid_target_host"),
    ] {
        let request = http::Request::builder()
            .method(Method::GET)
            .uri("/oagw/v1/proxy/vendor.com:9100")
            .header(TARGET_HOST, value)
            .body(axum::body::Body::empty())
            .expect("request");
        let response = app.clone().oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "value: {value}");
        let body = response.into_body().collect().await.expect("body").to_bytes();
        let problem: Value = serde_json::from_slice(&body).expect("problem");
        assert_eq!(
            problem["type"],
            format!("gts.cf.core.errors.err.v1~cf.oagw.{kind}.v1"),
            "value: {value}"
        );
    }
}

/// The target-host selector header of the data plane.
const TARGET_HOST: &str = "x-oagw-target-host";

#[tokio::test]
async fn query_allowlists_decide_which_parameters_are_forwarded() {
    let fixture = Fixture::start(json!({})).await;
    fixture
        .route(
            vec!["GET"],
            "/",
            json!({ "match": { "http": {
                "methods": ["GET"], "path": "/", "query_allowlist": ["keep"],
                "path_suffix_mode": "append"
            } } }),
        )
        .await;
    let strict = fixture.server.mock(|when, then| {
        when.method(GET).path("/").query_param_exists("keep").query_param_missing("drop");
        then.status(200).body("ok");
    });

    let (status, _, _) = common::send_bytes(
        fixture.gateway.clone(),
        Method::GET,
        &format!("/oagw/v1/proxy/{ALIAS}?keep=1&drop=2"),
        b"",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(strict.calls(), 1);

    // A route without an allowlist forwards nothing.
    fixture.route(vec!["GET"], "/strict", json!({})).await;
    let empty = fixture.server.mock(|when, then| {
        when.method(GET).path("/strict").query_param_missing("keep");
        then.status(200).body("ok");
    });
    let (status, _, _) = common::send_bytes(
        fixture.gateway.clone(),
        Method::GET,
        &format!("/oagw/v1/proxy/{ALIAS}/strict?keep=1"),
        b"",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(empty.calls(), 1);
}

#[tokio::test]
async fn streaming_bodies_pass_through() {
    let fixture = Fixture::start(json!({})).await;
    fixture.route(vec!["GET"], "/events", json!({})).await;
    let events = "data: one\n\ndata: two\n\n";
    let mock = fixture.server.mock(move |when, then| {
        when.method(GET).path("/events");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(events);
    });

    let (status, headers, body) = common::send_bytes(
        fixture.gateway.clone(),
        Method::GET,
        &format!("/oagw/v1/proxy/{ALIAS}/events"),
        b"",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get("content-type").and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );
    assert_eq!(body, events.as_bytes());
    assert_eq!(mock.calls(), 1);
}

#[tokio::test]
async fn request_bodies_are_forwarded() {
    let fixture = Fixture::start(json!({})).await;
    fixture.route(vec!["POST"], "/echo", json!({})).await;
    let mock = fixture.server.mock(|when, then| {
        when.method(POST).path("/echo").body("ping-payload");
        then.status(201).body("echo:ping-payload");
    });

    let (status, _, body) = common::send_bytes(
        fixture.gateway.clone(),
        Method::POST,
        &format!("/oagw/v1/proxy/{ALIAS}/echo"),
        b"ping-payload",
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body, &b"echo:ping-payload"[..]);
    assert_eq!(mock.calls(), 1);
}

#[tokio::test]
async fn oversized_request_bodies_are_refused() {
    let config = oagw::config::OagwConfig {
        allow_http_upstream: true,
        proxy_timeout_secs: 5,
        max_body_bytes: 8,
        ..oagw::config::OagwConfig::default()
    };
    let gateway = router(std::sync::Arc::new(oagw::state::OagwState::new(config)), security_context(tenant()));
    let server = MockServer::start();
    let payload = json!({
        "alias": "small.local",
        "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": server.port() } ] },
        "protocol": PROTOCOL_HTTP
    });
    let (status, _, upstream) = send_json(gateway.clone(), Method::POST, "/oagw/v1/upstreams", Some(payload)).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _, _) = send_json(
        gateway.clone(),
        Method::POST,
        "/oagw/v1/routes",
        Some(json!({
            "upstream_id": upstream["id"],
            "match": route_match_value(vec!["POST"], "/")
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, headers, document) =
        common::send_bytes(gateway, Method::POST, "/oagw/v1/proxy/small.local", b"0123456789").await;
    let problem = serde_json::from_slice(&document).expect("problem");
    let kind = assert_problem(status, &headers, &problem, 413);
    assert_eq!(kind, "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1");
}

#[tokio::test]
async fn websocket_upgrades_are_bridged_to_the_upstream() {
    // A hand-rolled upstream that answers the upgrade and echoes one frame.
    let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let upstream_addr = upstream.local_addr().expect("addr");
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = upstream.accept().await else { return };
            tokio::spawn(async move {
                let mut socket = socket;
                let mut buffer = [0u8; 4096];
                let mut request = String::new();
                loop {
                    let read = tokio::io::AsyncReadExt::read(&mut socket, &mut buffer).await.expect("read");
                    if read == 0 {
                        return;
                    }
                    request.push_str(&String::from_utf8_lossy(&buffer[..read]));
                    if request.contains("\r\n\r\n") {
                        break;
                    }
                }
                assert!(request.to_ascii_lowercase().contains("upgrade: websocket"));
                let response = "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n";
                tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes()).await.expect("write");
                loop {
                    let read = tokio::io::AsyncReadExt::read(&mut socket, &mut buffer).await.expect("read");
                    if read == 0 {
                        return;
                    }
                    tokio::io::AsyncWriteExt::write_all(&mut socket, &buffer[..read]).await.expect("echo");
                }
            });
        }
    });

    let app = router(state(true), security_context(tenant()));
    let payload = json!({
        "alias": "ws.local",
        "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": upstream_addr.port() } ] },
        "protocol": PROTOCOL_HTTP
    });
    let (status, _, upstream) = send_json(app.clone(), Method::POST, "/oagw/v1/upstreams", Some(payload)).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _, _) = send_json(
        app.clone(),
        Method::POST,
        "/oagw/v1/routes",
        Some(json!({
            "upstream_id": upstream["id"],
            "match": route_match_value(vec!["GET"], "/")
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let gateway_addr = listener.local_addr().expect("addr");
    let serve = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });

    let tcp = tokio::net::TcpStream::connect(gateway_addr).await.expect("connect");
    let (mut sender, connection) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(tcp)).await.expect("handshake");
    tokio::spawn(async move {
        let _ = connection.with_upgrades().await;
    });

    let request = http::Request::builder()
        .method(Method::GET)
        .uri(format!("http://{gateway_addr}/oagw/v1/proxy/ws.local"))
        .header("host", format!("{gateway_addr}"))
        .header("connection", "Upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .header("sec-websocket-version", "13")
        .body(http_body_util::Empty::<bytes::Bytes>::new())
        .expect("request");
    let response = sender.send_request(request).await.expect("upgrade response");
    assert_eq!(response.status(), http::StatusCode::SWITCHING_PROTOCOLS);
    assert_eq!(
        response.headers().get("upgrade").and_then(|value| value.to_str().ok()),
        Some("websocket")
    );

    let upgraded = hyper::upgrade::on(response).await.expect("upgraded stream");
    let mut upgraded = hyper_util::rt::TokioIo::new(upgraded);
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    upgraded.write_all(b"ping").await.expect("write");
    let mut frame = [0u8; 4];
    tokio::time::timeout(Duration::from_secs(5), upgraded.read_exact(&mut frame))
        .await
        .expect("echo within the budget")
        .expect("read");
    assert_eq!(&frame, b"ping");
    serve.abort();
}

#[tokio::test]
async fn disabled_upstreams_are_not_routable() {
    let fixture = Fixture::start(json!({ "enabled": false })).await;
    fixture.route(vec!["GET"], "/", json!({})).await;
    let (status, headers, document) =
        common::send_bytes(fixture.gateway.clone(), Method::GET, &format!("/oagw/v1/proxy/{ALIAS}"), b"").await;
    let problem = serde_json::from_slice(&document).expect("problem");
    let kind = assert_problem(status, &headers, &problem, 503);
    assert_eq!(kind, "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1");
    assert_eq!(problem["upstream_id"], fixture.upstream_id);
}

#[tokio::test]
async fn gateway_problems_carry_instance_and_trace() {
    let app = router(state(true), security_context(tenant()));
    let (status, headers, body) =
        common::send_bytes(app, Method::GET, "/oagw/v1/proxy/ghost.local", b"").await;
    let problem: Value = serde_json::from_slice(&body).expect("problem");
    assert_problem(status, &headers, &problem, 404);
    assert_eq!(
        problem["instance"].as_str().expect("instance"),
        "/oagw/v1/proxy/ghost.local"
    );
    assert!(problem["trace_id"].as_str().is_some(), "problem: {problem}");
}

// ---------------------------------------------------------------------------
// OAuth2 client-credentials auth plugin (ADR/0008)
// ---------------------------------------------------------------------------

/// The `Authorization` header value the Basic variant of the grant must send.
const BASIC_CREDENTIALS: &str = "Basic Z2F0ZXdheS1jbGllbnQ6Z2F0ZXdheS1zZWNyZXQ=";

#[tokio::test]
async fn oauth2_client_credentials_are_injected_and_cached() {
    let fixture = Fixture::start_with(state(true), |port| {
        json!({
            "alias": ALIAS,
            "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": port } ] },
            "protocol": PROTOCOL_HTTP,
            "auth": {
                "type": "oauth2_client_cred",
                "sharing": "private",
                "config": {
                    "token_url": format!("http://127.0.0.1:{port}/oauth/token"),
                    "client_id": "gateway-client",
                    "client_secret": "gateway-secret",
                    "scopes": ["gateway.read", "gateway.write"]
                }
            }
        })
    })
    .await;
    fixture.route(vec!["GET"], "/", json!({})).await;

    let token_endpoint = fixture.server.mock(|when, then| {
        when.method(POST)
            .path("/oauth/token")
            .header("content-type", "application/x-www-form-urlencoded")
            .body_includes("grant_type=client_credentials")
            .body_includes("client_id=gateway-client")
            .body_includes("client_secret=gateway-secret")
            .body_includes("scope=gateway.read+gateway.write");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"access_token":"token-1","token_type":"Bearer","expires_in":3600}"#);
    });
    let upstream = fixture.server.mock(|when, then| {
        when.method(GET).path("/").header("authorization", "Bearer token-1");
        then.status(200).body("ok");
    });

    for _ in 0..2 {
        let (status, _, body) = common::send_bytes(
            fixture.gateway.clone(),
            Method::GET,
            &format!("/oagw/v1/proxy/{ALIAS}"),
            b"",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "body: {}", String::from_utf8_lossy(&body));
    }
    assert_eq!(upstream.calls(), 2, "both proxied requests must reach the upstream");
    assert_eq!(
        token_endpoint.calls(),
        1,
        "the second proxied request must reuse the cached access token"
    );
}

#[tokio::test]
async fn oauth2_client_credentials_basic_uses_a_basic_header_on_the_token_request() {
    let fixture = Fixture::start_with(state(true), |port| {
        json!({
            "alias": ALIAS,
            "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": port } ] },
            "protocol": PROTOCOL_HTTP,
            "auth": {
                "type": "oauth2_client_cred_basic",
                "sharing": "private",
                "config": {
                    "token_url": format!("http://127.0.0.1:{port}/oauth/token"),
                    "client_id": "gateway-client",
                    "client_secret": "gateway-secret"
                }
            }
        })
    })
    .await;
    fixture.route(vec!["GET"], "/", json!({})).await;

    let token_endpoint = fixture.server.mock(|when, then| {
        when.method(POST)
            .path("/oauth/token")
            .header("authorization", BASIC_CREDENTIALS)
            .body_includes("grant_type=client_credentials")
            .body_excludes("gateway-secret");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"access_token":"token-2","token_type":"Bearer","expires_in":3600}"#);
    });
    let upstream = fixture.server.mock(|when, then| {
        when.method(GET).path("/").header("authorization", "Bearer token-2");
        then.status(200).body("ok");
    });

    let (status, _, body) = common::send_bytes(
        fixture.gateway.clone(),
        Method::GET,
        &format!("/oagw/v1/proxy/{ALIAS}"),
        b"",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {}", String::from_utf8_lossy(&body));
    assert_eq!(upstream.calls(), 1);
    assert_eq!(token_endpoint.calls(), 1);
}

#[tokio::test]
async fn a_refused_grant_maps_to_an_auth_failure() {
    let fixture = Fixture::start_with(state(true), |port| {
        json!({
            "alias": ALIAS,
            "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": port } ] },
            "protocol": PROTOCOL_HTTP,
            "auth": {
                "type": "oauth2_client_cred",
                "sharing": "private",
                "config": {
                    "token_url": format!("http://127.0.0.1:{port}/oauth/token"),
                    "client_id": "gateway-client",
                    "client_secret": "wrong-secret"
                }
            }
        })
    })
    .await;
    fixture.route(vec!["GET"], "/", json!({})).await;
    let token_endpoint = fixture.server.mock(|when, then| {
        when.method(POST).path("/oauth/token");
        then.status(400).body(r#"{"error":"invalid_client"}"#);
    });
    let upstream = fixture.server.mock(|when, then| {
        when.method(GET).path("/");
        then.status(200).body("ok");
    });

    let (status, headers, document) = common::send_bytes(
        fixture.gateway.clone(),
        Method::GET,
        &format!("/oagw/v1/proxy/{ALIAS}"),
        b"",
    )
    .await;
    let problem = serde_json::from_slice(&document).expect("problem");
    let kind = assert_problem(status, &headers, &problem, 401);
    assert_eq!(kind, "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1");
    assert_eq!(token_endpoint.calls(), 1);
    assert_eq!(upstream.calls(), 0, "no request may reach the upstream without a token");
}

// ---------------------------------------------------------------------------
// Required-headers guard plugin (ADR/0009)
// ---------------------------------------------------------------------------

const REQUIRED_HEADERS: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";

#[tokio::test]
async fn required_request_headers_are_enforced_before_the_upstream_is_dialled() {
    let fixture = Fixture::start(json!({
        "plugins": {
            "sharing": "private",
            "items": [
                { "plugin_ref": REQUIRED_HEADERS, "config": { "required_request_headers": "x-correlation-id" } }
            ]
        }
    }))
    .await;
    fixture.route(vec!["GET"], "/", json!({})).await;
    let upstream = fixture.server.mock(|when, then| {
        when.method(GET).path("/");
        then.status(200).body("ok");
    });

    let (status, headers, document) = common::send_bytes(
        fixture.gateway.clone(),
        Method::GET,
        &format!("/oagw/v1/proxy/{ALIAS}"),
        b"",
    )
    .await;
    let problem = serde_json::from_slice(&document).expect("problem");
    let kind = assert_problem(status, &headers, &problem, 400);
    assert_eq!(kind, "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1");
    assert!(
        problem["detail"].as_str().unwrap_or_default().contains("x-correlation-id"),
        "problem: {problem}"
    );
    assert_eq!(upstream.calls(), 0, "the guard must run before the upstream is dialled");

    // A request carrying the header is forwarded.
    let request = http::Request::builder()
        .method(Method::GET)
        .uri(format!("/oagw/v1/proxy/{ALIAS}"))
        .header("x-correlation-id", "trace-1")
        .body(axum::body::Body::empty())
        .expect("request");
    let response = fixture.gateway.clone().oneshot(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(upstream.calls(), 1);
}

#[tokio::test]
async fn required_response_headers_are_enforced_before_the_response_is_relayed() {
    let fixture = Fixture::start(json!({
        "plugins": {
            "sharing": "private",
            "items": [
                { "plugin_ref": REQUIRED_HEADERS, "config": { "required_response_headers": "x-signature" } }
            ]
        }
    }))
    .await;
    fixture.route(vec!["GET"], "/", json!({})).await;
    fixture.route(vec!["GET"], "/signed", json!({})).await;
    let unsigned = fixture.server.mock(|when, then| {
        when.method(GET).path("/");
        then.status(200).body("unsigned");
    });
    let signed = fixture.server.mock(|when, then| {
        when.method(GET).path("/signed");
        then.status(200).header("x-signature", "hmac").body("signed");
    });

    let (status, headers, document) = common::send_bytes(
        fixture.gateway.clone(),
        Method::GET,
        &format!("/oagw/v1/proxy/{ALIAS}"),
        b"",
    )
    .await;
    let problem = serde_json::from_slice(&document).expect("problem");
    let kind = assert_problem(status, &headers, &problem, 502);
    assert_eq!(kind, "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1");
    assert!(
        problem["detail"].as_str().unwrap_or_default().contains("x-signature"),
        "problem: {problem}"
    );
    assert_eq!(unsigned.calls(), 1);

    let (status, _, body) = common::send_bytes(
        fixture.gateway.clone(),
        Method::GET,
        &format!("/oagw/v1/proxy/{ALIAS}/signed"),
        b"",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, &b"signed"[..]);
    assert_eq!(signed.calls(), 1);
}

#[tokio::test]
async fn an_unconfigured_required_headers_guard_fails_open() {
    // A blank requirement is a no-op for the phase it applies to.
    let fixture = Fixture::start(json!({
        "plugins": {
            "sharing": "private",
            "items": [
                { "plugin_ref": REQUIRED_HEADERS, "config": { "required_request_headers": "  " } }
            ]
        }
    }))
    .await;
    fixture.route(vec!["GET"], "/", json!({})).await;
    let upstream = fixture.server.mock(|when, then| {
        when.method(GET).path("/");
        then.status(200).body("ok");
    });

    let (status, _, _) = common::send_bytes(
        fixture.gateway.clone(),
        Method::GET,
        &format!("/oagw/v1/proxy/{ALIAS}"),
        b"",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(upstream.calls(), 1);
}

// ---------------------------------------------------------------------------
// CORS (ADR/0004)
// ---------------------------------------------------------------------------

/// An upstream whose CORS configuration allows one browser origin.
fn cors_upstream(port: u16) -> Value {
    json!({
        "alias": ALIAS,
        "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": port } ] },
        "protocol": PROTOCOL_HTTP,
        "cors": {
            "enabled": true,
            "allowed_origins": ["https://app.example.com"],
            "allowed_methods": ["GET", "POST"],
            "expose_headers": ["x-request-id"],
            "allow_credentials": true
        }
    })
}

#[tokio::test]
async fn cors_preflight_is_answered_locally_without_dialling_the_upstream() {
    let fixture = Fixture::start_with(state(true), cors_upstream).await;
    fixture.route(vec!["GET"], "/", json!({})).await;
    let upstream = fixture.server.mock(|when, then| {
        when.method(GET).path("/");
        then.status(200).body("ok");
    });

    let request = http::Request::builder()
        .method(Method::OPTIONS)
        .uri(format!("/oagw/v1/proxy/{ALIAS}"))
        .header("origin", "https://app.example.com")
        .header("access-control-request-method", "POST")
        .header("access-control-request-headers", "content-type")
        .body(axum::body::Body::empty())
        .expect("request");
    let response = fixture.gateway.clone().oneshot(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::NO_CONTENT, "the preflight must be answered locally");
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .and_then(|value| value.to_str().ok()),
        Some("https://app.example.com")
    );
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-methods")
            .and_then(|value| value.to_str().ok()),
        Some("POST")
    );
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-headers")
            .and_then(|value| value.to_str().ok()),
        Some("content-type")
    );
    assert_eq!(upstream.calls(), 0, "a preflight must never reach the upstream");
}

#[tokio::test]
async fn cors_allowed_origin_request_carries_the_cors_response_headers() {
    let fixture = Fixture::start_with(state(true), cors_upstream).await;
    fixture.route(vec!["GET"], "/", json!({})).await;
    let upstream = fixture.server.mock(|when, then| {
        when.method(GET).path("/");
        then.status(200).body("ok");
    });

    let request = http::Request::builder()
        .method(Method::GET)
        .uri(format!("/oagw/v1/proxy/{ALIAS}"))
        .header("origin", "https://app.example.com")
        .body(axum::body::Body::empty())
        .expect("request");
    let response = fixture.gateway.clone().oneshot(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers().clone();
    assert_eq!(
        headers.get("access-control-allow-origin").and_then(|value| value.to_str().ok()),
        Some("https://app.example.com")
    );
    assert_eq!(
        headers.get("access-control-allow-credentials").and_then(|value| value.to_str().ok()),
        Some("true")
    );
    assert_eq!(
        headers.get("access-control-expose-headers").and_then(|value| value.to_str().ok()),
        Some("x-request-id")
    );
    assert_eq!(upstream.calls(), 1);
}

#[tokio::test]
async fn cors_disallowed_origin_never_reaches_the_upstream() {
    let fixture = Fixture::start_with(state(true), cors_upstream).await;
    fixture.route(vec!["GET"], "/", json!({})).await;
    let upstream = fixture.server.mock(|when, then| {
        when.method(GET).path("/");
        then.status(200).body("ok");
    });

    let request = http::Request::builder()
        .method(Method::GET)
        .uri(format!("/oagw/v1/proxy/{ALIAS}"))
        .header("origin", "https://evil.example.com")
        .body(axum::body::Body::empty())
        .expect("request");
    let response = fixture.gateway.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.expect("body").to_bytes();
    let problem: Value = serde_json::from_slice(&body).expect("problem");
    assert_gateway_problem(status, &headers, &problem);
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1"
    );
    assert_eq!(upstream.calls(), 0, "a disallowed origin must be rejected before dialling");
}

#[tokio::test]
async fn cors_disallowed_origin_is_rejected_with_403() {
    let fixture = Fixture::start_with(state(true), cors_upstream).await;
    fixture.route(vec!["GET"], "/", json!({})).await;
    let upstream = fixture.server.mock(|when, then| {
        when.method(GET).path("/");
        then.status(200).body("ok");
    });

    let request = http::Request::builder()
        .method(Method::GET)
        .uri(format!("/oagw/v1/proxy/{ALIAS}"))
        .header("origin", "https://evil.example.com")
        .body(axum::body::Body::empty())
        .expect("request");
    let response = fixture.gateway.clone().oneshot(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(upstream.calls(), 0);
}

#[tokio::test]
async fn cors_disallowed_method_is_rejected_with_403() {
    let fixture = Fixture::start_with(state(true), cors_upstream).await;
    fixture.route(vec!["GET"], "/", json!({})).await;
    fixture.route(vec!["DELETE"], "/resources", json!({})).await;
    let upstream = fixture.server.mock(|when, then| {
        when.method(DELETE).path("/resources");
        then.status(204);
    });

    let request = http::Request::builder()
        .method(Method::DELETE)
        .uri(format!("/oagw/v1/proxy/{ALIAS}/resources"))
        .header("origin", "https://app.example.com")
        .body(axum::body::Body::empty())
        .expect("request");
    let response = fixture.gateway.clone().oneshot(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(upstream.calls(), 0);
}

#[tokio::test]
async fn cors_requests_without_an_origin_are_forwarded() {
    // A same-origin or non-browser request is unaffected by the CORS policy.
    let fixture = Fixture::start_with(state(true), cors_upstream).await;
    fixture.route(vec!["GET"], "/", json!({})).await;
    let upstream = fixture.server.mock(|when, then| {
        when.method(GET).path("/").header_missing("origin");
        then.status(200).body("ok");
    });

    let (status, _, _) = common::send_bytes(
        fixture.gateway.clone(),
        Method::GET,
        &format!("/oagw/v1/proxy/{ALIAS}"),
        b"",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(upstream.calls(), 1);
}

// ---------------------------------------------------------------------------
// SSRF policy (PRD cpt-cf-oagw-nfr-ssrf-protection)
// ---------------------------------------------------------------------------

/// Error types produced by the upstream transport itself. A refusal issued by
/// the SSRF policy before the connection is dialled is none of these.
const TRANSPORT_FAILURES: [&str; 6] = [
    "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1",
    "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1",
    "gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1",
    "gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1",
    "gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1",
    "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1",
];

/// A state with the SSRF policy switched on.
fn ssrf_state() -> Arc<oagw::state::OagwState> {
    state_with(oagw::config::OagwConfig {
        allow_http_upstream: true,
        proxy_timeout_secs: 5,
        connect_timeout_secs: Some(1),
        ssrf_policy: oagw::config::SsrfPolicy { enabled: true },
        ..oagw::config::OagwConfig::default()
    })
}

#[tokio::test]
async fn ssrf_policy_blocks_non_public_endpoint_hosts() {
    let fixture = Fixture::start_with(ssrf_state(), |port| {
        json!({
            "alias": ALIAS,
            "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": port } ] },
            "protocol": PROTOCOL_HTTP
        })
    })
    .await;
    fixture.route(vec!["GET"], "/", json!({})).await;

    // The loopback address is the mock server itself, so a dial would succeed.
    let loopback = fixture.server.mock(|when, then| {
        when.method(GET).path("/");
        then.status(200).body("ok");
    });
    let (status, headers, document) = common::send_bytes(
        fixture.gateway.clone(),
        Method::GET,
        &format!("/oagw/v1/proxy/{ALIAS}"),
        b"",
    )
    .await;
    assert!(
        (400..600).contains(&u16::from(status)),
        "the loopback endpoint must be refused by the policy; the gateway dialled it instead: {status}"
    );
    let problem = serde_json::from_slice(&document).expect("problem");
    let kind = assert_gateway_problem(status, &headers, &problem);
    assert!(
        !TRANSPORT_FAILURES.contains(&kind.as_str()),
        "the loopback endpoint must be refused by the policy, not by the transport: {kind}"
    );
    assert_eq!(loopback.calls(), 0, "the upstream must not be dialled");

    // Private and link-local literals are refused the same way.
    for (alias, host) in [("private.local", "10.0.0.1"), ("link-local.local", "169.254.1.1")] {
        let payload = json!({
            "alias": alias,
            "server": { "endpoints": [ { "scheme": "http", "host": host, "port": 80 } ] },
            "protocol": PROTOCOL_HTTP
        });
        let (status, _, document) =
            send_json(fixture.gateway.clone(), Method::POST, "/oagw/v1/upstreams", Some(payload)).await;
        assert_eq!(status, StatusCode::CREATED, "document: {document}");
        let (status, _, _) = send_json(
            fixture.gateway.clone(),
            Method::POST,
            "/oagw/v1/routes",
            Some(json!({
                "upstream_id": document["id"],
                "match": route_match_value(vec!["GET"], "/")
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);

        let (status, headers, body) = common::send_bytes(
            fixture.gateway.clone(),
            Method::GET,
            &format!("/oagw/v1/proxy/{alias}"),
            b"",
        )
        .await;
        assert!(
            (400..600).contains(&u16::from(status)),
            "endpoint {host} must be refused by the policy; the gateway dialled it instead: {status}"
        );
        let problem: Value = serde_json::from_slice(&body).expect("problem");
        let kind = assert_gateway_problem(status, &headers, &problem);
        assert!(
            !TRANSPORT_FAILURES.contains(&kind.as_str()),
            "endpoint {host} must be refused by the policy, not by the transport: {kind}"
        );
    }
}

#[tokio::test]
async fn ssrf_policy_dials_public_endpoint_hosts() {
    // TEST-NET-3 is never routable, so a dialled exchange fails in the
    // transport instead of being refused by the policy up front.
    let app = router(ssrf_state(), security_context(tenant()));
    let payload = json!({
        "alias": "public.local",
        "server": { "endpoints": [ { "scheme": "http", "host": "203.0.113.7", "port": 80 } ] },
        "protocol": PROTOCOL_HTTP
    });
    let (status, _, upstream) = send_json(app.clone(), Method::POST, "/oagw/v1/upstreams", Some(payload)).await;
    assert_eq!(status, StatusCode::CREATED, "document: {upstream}");
    let (status, _, _) = send_json(
        app.clone(),
        Method::POST,
        "/oagw/v1/routes",
        Some(json!({
            "upstream_id": upstream["id"],
            "match": route_match_value(vec!["GET"], "/")
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, headers, body) =
        common::send_bytes(app, Method::GET, "/oagw/v1/proxy/public.local", b"").await;
    let problem = serde_json::from_slice(&body).expect("problem");
    let kind = assert_gateway_problem(status, &headers, &problem);
    assert!(
        TRANSPORT_FAILURES.contains(&kind.as_str()),
        "a public endpoint must be dialled, not refused by the policy: {kind}"
    );
}

// ---------------------------------------------------------------------------
// Request body limits
// ---------------------------------------------------------------------------

/// A state with a small request-body ceiling.
fn limited_state(max_body_bytes: u64) -> Arc<oagw::state::OagwState> {
    state_with(oagw::config::OagwConfig {
        allow_http_upstream: true,
        proxy_timeout_secs: 5,
        max_body_bytes,
        ..oagw::config::OagwConfig::default()
    })
}

#[tokio::test]
async fn a_declared_content_length_above_the_limit_is_refused_before_dialling() {
    let fixture = Fixture::start_with(limited_state(16), default_upstream).await;
    fixture.route(vec!["POST"], "/", json!({})).await;
    let upstream = fixture.server.mock(|when, then| {
        when.method(POST).path("/");
        then.status(200).body("ok");
    });

    let request = http::Request::builder()
        .method(Method::POST)
        .uri(format!("/oagw/v1/proxy/{ALIAS}"))
        .header("content-length", "17")
        .body(axum::body::Body::empty())
        .expect("request");
    let response = fixture.gateway.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.expect("body").to_bytes();
    let problem: Value = serde_json::from_slice(&body).expect("problem");
    let kind = assert_problem(status, &headers, &problem, 413);
    assert_eq!(kind, "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1");
    let detail = problem["detail"].as_str().unwrap_or_default();
    assert!(detail.contains("17") && detail.contains("16"), "detail: {detail}");
    assert_eq!(
        upstream.calls(),
        0,
        "the request must be refused before any byte reaches the upstream"
    );
}

#[tokio::test]
async fn a_body_at_the_configured_limit_is_forwarded() {
    let fixture = Fixture::start_with(limited_state(16), default_upstream).await;
    fixture.route(vec!["POST"], "/", json!({})).await;
    let upstream = fixture.server.mock(|when, then| {
        when.method(POST).path("/").body("0123456789abcdef");
        then.status(200).body("ok");
    });

    let (status, _, _) = common::send_bytes(
        fixture.gateway.clone(),
        Method::POST,
        &format!("/oagw/v1/proxy/{ALIAS}"),
        b"0123456789abcdef",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(upstream.calls(), 1);
}

// ---------------------------------------------------------------------------
// Incremental streaming
// ---------------------------------------------------------------------------

/// The complete three-event SSE stream written by the hand-rolled upstream.
const SSE_ALL_FRAMES: &str = "data: frame-1\n\ndata: frame-2\n\ndata: frame-3\n\n";

/// Reads body frames until `relayed` contains `marker`.
///
/// # Panics
/// Panics when a frame does not arrive within the five second budget or the
/// stream frame cannot be decoded.
async fn read_frames_until(body: &mut axum::body::Body, relayed: &mut String, marker: &str) {
    while !relayed.contains(marker) {
        let frame = tokio::time::timeout(Duration::from_secs(5), body.frame())
            .await
            .expect("frame within the budget")
            .expect("frame")
            .expect("data");
        if let Some(payload) = frame.data_ref() {
            relayed.push_str(&String::from_utf8_lossy(payload));
        }
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn sse_events_are_relayed_incrementally() {
    // A hand-rolled upstream writes three `data:` events with pauses between
    // them, so a relay that buffers the whole response cannot pass.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let upstream_addr = listener.local_addr().expect("addr");
    let progress = Arc::new(AtomicUsize::new(0));
    let writer_progress = Arc::clone(&progress);
    let writer = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.expect("accept");
        let mut socket = socket;
        let mut buffer = [0u8; 4096];
        let mut request = String::new();
        loop {
            let read = tokio::io::AsyncReadExt::read(&mut socket, &mut buffer).await.expect("read");
            if read == 0 {
                return;
            }
            request.push_str(&String::from_utf8_lossy(&buffer[..read]));
            if request.contains("\r\n\r\n") {
                break;
            }
        }
        let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n";
        tokio::io::AsyncWriteExt::write_all(&mut socket, head.as_bytes()).await.expect("write head");
        for event in ["frame-1", "frame-2", "frame-3"] {
            let payload = format!("data: {event}\n\n");
            let chunk = format!("{:x}\r\n{payload}\r\n", payload.len());
            tokio::io::AsyncWriteExt::write_all(&mut socket, chunk.as_bytes()).await.expect("write chunk");
            tokio::io::AsyncWriteExt::flush(&mut socket).await.expect("flush");
            writer_progress.fetch_add(1, Ordering::SeqCst);
            if event != "frame-3" {
                tokio::time::sleep(Duration::from_millis(400)).await;
            }
        }
        tokio::io::AsyncWriteExt::write_all(&mut socket, b"0\r\n\r\n").await.expect("write terminator");
        tokio::io::AsyncWriteExt::flush(&mut socket).await.expect("flush terminator");
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    let app = router(state(true), security_context(tenant()));
    let payload = json!({
        "alias": "sse.local",
        "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": upstream_addr.port() } ] },
        "protocol": PROTOCOL_HTTP
    });
    let (status, _, upstream) = send_json(app.clone(), Method::POST, "/oagw/v1/upstreams", Some(payload)).await;
    assert_eq!(status, StatusCode::CREATED, "document: {upstream}");
    let (status, _, _) = send_json(
        app.clone(),
        Method::POST,
        "/oagw/v1/routes",
        Some(json!({
            "upstream_id": upstream["id"],
            "match": route_match_value(vec!["GET"], "/events")
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let request = http::Request::builder()
        .method(Method::GET)
        .uri("/oagw/v1/proxy/sse.local/events")
        .body(axum::body::Body::empty())
        .expect("request");
    let response = app.oneshot(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );

    // The first event must be observable before the upstream writes the second.
    let mut body = response.into_body();
    let mut relayed = String::new();
    read_frames_until(&mut body, &mut relayed, "data: frame-1\n\n").await;
    assert_eq!(
        progress.load(Ordering::SeqCst),
        1,
        "the first event must be observable while the upstream is still writing"
    );

    read_frames_until(&mut body, &mut relayed, SSE_ALL_FRAMES).await;
    assert_eq!(relayed, SSE_ALL_FRAMES);
    assert_eq!(progress.load(Ordering::SeqCst), 3, "the upstream must have finished");
    writer.abort();
}
