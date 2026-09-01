#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Integration tests for the data-plane routes (DESIGN §3.5).
//!
//! The router under test is built exactly as the gear builds it —
//! [`crate::api::rest::routes::register_routes`] followed by
//! [`crate::api::rest::routes::register_data_plane`] — so the assertions cover
//! the registration itself, not only the engine.
//!
//! Every test drives a **real** upstream: `httpmock` for plain HTTP, a
//! hand-rolled tokio TCP server for `text/event-stream` (one flush per event)
//! and a `tokio-tungstenite` echo server for the WebSocket upgrade, which is
//! driven through a real `axum::serve` listener because an upgrade can only
//! complete over a live connection.

use std::sync::Arc;

use axum::Router;
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use toolkit_security::context::SecurityContext;
use tokio_tungstenite::tungstenite::protocol::Message;
use tower::ServiceExt;
use uuid::Uuid;

use crate::api::rest::routes::{register_data_plane, register_routes};
use crate::config::OagwConfig;
use crate::domain::models::{
    Endpoint, EndpointScheme, HttpMatch, HttpMethod, MatchConfig, PathSuffixMode, Protocol,
    ServerConfig,
};
use crate::domain::service::{ControlPlaneService, RouteDraft, UpstreamDraft};
use crate::infra::metrics::MetricsRegistry;
use crate::infra::plugin;
use crate::infra::proxy::HttpProxyEngine;
use crate::infra::ratelimit::LimiterRegistry;
use crate::infra::storage::InMemoryStore;
use crate::infra::tenant::TenantChainResolver;
use crate::infra::transport::Transport;

const BASE: &str = "/api/oagw/v1";
const ALIAS: &str = "api.vendor.com";

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

/// The data plane wired the way [`crate::gear::OagwGear`] wires it.
struct DataPlane {
    router: Router,
    service: Arc<ControlPlaneService>,
    tenant: Uuid,
}

fn data_plane() -> (DataPlane, httpmock::MockServer) {
    let upstream_server = httpmock::MockServer::start();
    let config = OagwConfig {
        allow_http_upstream: true,
        ..OagwConfig::default()
    };
    let service = Arc::new(ControlPlaneService::new(InMemoryStore::new(), &config));
    let transport = Arc::new(Transport::new(config.proxy_timeout()).unwrap());
    let registry = plugin::registry(&plugin::PluginBundle {
        secrets: Arc::new(plugin::LiteralSecretResolver),
        transport: Arc::clone(&transport),
        token_cache_ttl: config.token_cache_ttl(),
        token_cache_capacity: config.token_cache_capacity,
    });
    let engine = Arc::new(HttpProxyEngine::new(
        Arc::clone(&service),
        TenantChainResolver::new(None),
        transport,
        registry,
        LimiterRegistry::new(),
        Arc::new(MetricsRegistry::new()),
        config,
    ));
    let openapi = toolkit::api::OpenApiRegistryImpl::new();
    let router = register_routes(Router::new(), &openapi, Arc::clone(&service));
    let router = register_data_plane(router, engine);
    (
        DataPlane {
            router,
            service,
            tenant: Uuid::new_v4(),
        },
        upstream_server,
    )
}

impl DataPlane {
    /// Provisions the `api.vendor.com` upstream plus one route on `path`.
    async fn provision(&self, port: u16, path: &str) {
        let created = self
            .service
            .create_upstream(
                self.tenant,
                UpstreamDraft {
                    alias: Some(ALIAS.to_owned()),
                    enabled: true,
                    protocol: Protocol::Http,
                    server: ServerConfig {
                        endpoints: vec![Endpoint::new(EndpointScheme::Http, "127.0.0.1", port)],
                    },
                    auth: None,
                    headers: None,
                    plugins: None,
                    rate_limit: None,
                    cors: None,
                    tags: Vec::new(),
                },
            )
            .await
            .unwrap();
        self.service
            .create_route(
                self.tenant,
                RouteDraft {
                    upstream_id: created.id,
                    enabled: true,
                    priority: 0,
                    match_config: MatchConfig {
                        http: Some(HttpMatch {
                            methods: vec![HttpMethod::Get, HttpMethod::Post],
                            path: path.to_owned(),
                            query_allowlist: Vec::new(),
                            path_suffix_mode: PathSuffixMode::Append,
                        }),
                        grpc: None,
                    },
                    plugins: None,
                    rate_limit: None,
                    tags: Vec::new(),
                },
            )
            .await
            .unwrap();
    }
}

/// Sends a request through the registered router and buffers the response.
async fn send(
    plane: &DataPlane,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> (axum::http::StatusCode, axum::http::HeaderMap, bytes::Bytes) {
    let mut builder = http::Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder
        .body(axum::body::Body::from(body.to_vec()))
        .unwrap();
    // The production middleware installs the `SecurityContext` extension
    // before the router runs; the test reproduces that per request.
    let security = SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(plane.tenant)
        .build()
        .unwrap();
    let mut request = request;
    request.extensions_mut().insert(security);
    let response = plane.router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let response_headers = response.headers().clone();
    let payload = response.into_body().collect().await.unwrap().to_bytes();
    (status, response_headers, payload)
}

/// Serves `router` on a loopback port and returns its address.
///
/// The production middleware installs the `SecurityContext` extension before
/// the router runs; the test reproduces that with a layer.
async fn serve(plane: &DataPlane) -> String {
    let security = SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(plane.tenant)
        .build()
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let app = plane.router.clone().layer(axum::Extension(security));
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    address
}

/// An upstream that speaks WebSocket and echoes every frame back.
async fn echo_upstream() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let Ok(socket) = tokio_tungstenite::accept_async(stream).await else {
                    return;
                };
                let (mut sink, mut stream) = socket.split();
                while let Some(frame) = stream.next().await {
                    let Ok(frame) = frame else { break };
                    if sink.send(frame).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    port
}

// ---------------------------------------------------------------------------
// plain HTTP
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_proxied_request_reaches_the_real_upstream_through_the_registered_route() {
    let (plane, server) = data_plane();
    plane.provision(server.port(), "/v1").await;
    server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/things");
        then.status(200)
            .header("content-type", "application/json")
            .body("hello upstream");
    });

    let (status, headers, body) = send(
        &plane,
        "GET",
        &format!("{BASE}/proxy/{ALIAS}/v1/things"),
        &[],
        b"",
    )
    .await;

    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(headers.get("content-type").unwrap(), "application/json");
    assert_eq!(
        headers
            .get(crate::infra::proxy::ERROR_SOURCE_HEADER)
            .unwrap(),
        crate::infra::proxy::ERROR_SOURCE_UPSTREAM
    );
    assert_eq!(body, "hello upstream");
}

#[tokio::test]
async fn an_unknown_alias_is_rendered_as_a_problem_document() {
    let (plane, _server) = data_plane();

    let (status, headers, body) = send(&plane, "GET", &format!("{BASE}/proxy/nowhere/v1"), &[], b"")
        .await;

    assert_eq!(status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        headers
            .get(crate::infra::proxy::ERROR_SOURCE_HEADER)
            .unwrap(),
        crate::infra::proxy::ERROR_SOURCE_GATEWAY
    );
    let document: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(document["status"], 503);
    assert!(document["type"].is_string());
    assert!(document["title"].is_string());
}

// ---------------------------------------------------------------------------
// CORS preflight (ADR 0004)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_preflight_is_answered_by_the_gateway_itself() {
    let (plane, server) = data_plane();
    plane.provision(server.port(), "/v1").await;

    let (status, headers, body) = send(
        &plane,
        "OPTIONS",
        &format!("{BASE}/proxy/{ALIAS}/v1/things"),
        &[
            ("origin", "https://console.example.org"),
            ("access-control-request-method", "POST"),
            ("access-control-request-headers", "authorization"),
        ],
        b"",
    )
    .await;

    assert_eq!(status, axum::http::StatusCode::NO_CONTENT);
    assert!(body.is_empty());
    assert_eq!(
        headers
            .get("access-control-allow-origin")
            .and_then(|value| value.to_str().ok()),
        Some("https://console.example.org")
    );
    assert_eq!(
        headers
            .get("access-control-allow-methods")
            .and_then(|value| value.to_str().ok()),
        Some("POST")
    );
    assert_eq!(
        headers
            .get("access-control-allow-headers")
            .and_then(|value| value.to_str().ok()),
        Some("authorization")
    );
    assert_eq!(
        headers
            .get("access-control-max-age")
            .and_then(|value| value.to_str().ok()),
        Some("86400")
    );
}

// ---------------------------------------------------------------------------
// SSE pass-through
// ---------------------------------------------------------------------------

/// A TCP upstream that answers one `text/event-stream` request, writing each
/// event in its own chunked body part and flushing it separately.
async fn event_upstream(events: &[&str]) -> (u16, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let chunks: Vec<Vec<u8>> = events
        .iter()
        .map(|event| {
            let payload = format!("{event}\n\n");
            let mut chunk = format!("{:x}\r\n", payload.len()).into_bytes();
            chunk.extend_from_slice(payload.as_bytes());
            chunk.extend_from_slice(b"\r\n");
            chunk
        })
        .collect();
    let handle = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = vec![0_u8; 8192];
        let read = socket.read(&mut buffer).await.unwrap_or(0);
        if read == 0 {
            return;
        }
        let mut response = Vec::from(
            &b"HTTP/1.1 200 OK\r\n\
               Content-Type: text/event-stream\r\n\
               Cache-Control: no-cache\r\n\
               Transfer-Encoding: chunked\r\n\r\n"[..],
        );
        for chunk in &chunks {
            response.extend_from_slice(chunk);
            // A separate write per event: the gateway must not batch them.
            if socket.write_all(&response).await.is_err() {
                return;
            }
            response.clear();
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        socket.write_all(b"0\r\n\r\n").await.unwrap();
        socket.flush().await.unwrap();
    });
    (port, handle)
}

#[tokio::test]
async fn an_event_stream_is_relayed_event_by_event() {
    let (plane, _server) = data_plane();
    let (port, server_task) = event_upstream(&["data: one", "data: two", "data: three"]).await;
    plane.provision(port, "/v1").await;

    let mut request = http::Request::builder()
        .method("GET")
        .uri(format!("{BASE}/proxy/{ALIAS}/v1/events"))
        .body(axum::body::Body::empty())
        .unwrap();
    request.extensions_mut().insert(
        SecurityContext::builder()
            .subject_id(Uuid::new_v4())
            .subject_tenant_id(plane.tenant)
            .build()
            .unwrap(),
    );
    let response = plane.router.clone().oneshot(request).await.unwrap();

    assert_eq!(response.status(), axum::http::StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );

    // The body must arrive in more than one chunk: an event stream that is
    // buffered whole has no value as a pass-through.
    let mut body = response.into_body().into_data_stream();
    let mut received: Vec<Vec<u8>> = Vec::new();
    while let Some(chunk) = body.next().await {
        received.push(chunk.unwrap().to_vec());
    }
    assert!(received.len() > 1, "expected one chunk per event");
    let text: String = received
        .iter()
        .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
        .collect();
    assert_eq!(text, "data: one\n\ndata: two\n\ndata: three\n\n");
    server_task.await.unwrap();
}

// ---------------------------------------------------------------------------
// WebSocket tunnelling (A1)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_websocket_upgrade_is_tunnelled_to_the_upstream() {
    let (plane, _server) = data_plane();
    let port = echo_upstream().await;
    plane.provision(port, "/ws").await;
    let address = serve(&plane).await;

    let (mut socket, handshake) =
        tokio_tungstenite::connect_async(format!("ws://{address}{BASE}/proxy/{ALIAS}/ws"))
            .await
            .unwrap();
    assert_eq!(handshake.status(), 101);

    socket.send(Message::text("ping")).await.unwrap();
    let echoed = socket.next().await.unwrap().unwrap();
    assert_eq!(echoed, Message::text("ping"));

    socket
        .send(Message::binary(bytes::Bytes::from_static(b"\x01\x02")))
        .await
        .unwrap();
    let echoed = socket.next().await.unwrap().unwrap();
    assert_eq!(echoed, Message::binary(bytes::Bytes::from_static(b"\x01\x02")));
}

// ---------------------------------------------------------------------------
// metrics
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_metrics_endpoint_renders_the_prometheus_exposition() {
    let (plane, server) = data_plane();
    plane.provision(server.port(), "/v1").await;
    server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/things");
        then.status(200).body("ok");
    });
    let _ = send(
        &plane,
        "GET",
        &format!("{BASE}/proxy/{ALIAS}/v1/things"),
        &[],
        b"",
    )
    .await;

    let (status, headers, body) = send(&plane, "GET", &format!("{BASE}/metrics"), &[], b"").await;

    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(
        headers
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/plain; version=0.0.4; charset=utf-8")
    );
    let rendered = String::from_utf8_lossy(&body);
    assert!(rendered.contains("oagw_requests_total"));
    assert!(rendered.contains("oagw_request_duration_seconds_bucket"));
}

#[tokio::test]
async fn a_preflight_needs_no_security_context() {
    let (plane, _server) = data_plane();
    let request = http::Request::builder()
        .method("OPTIONS")
        .uri(format!("{BASE}/proxy/{ALIAS}/v1/things"))
        .header("origin", "https://console.example.org")
        .header("access-control-request-method", "POST")
        .body(axum::body::Body::empty())
        .unwrap();
    // Deliberately no `SecurityContext` extension: a browser preflight is sent
    // without credentials, so the gateway must answer it before resolving the
    // caller.
    let response = plane.router.clone().oneshot(request).await.unwrap();

    assert_eq!(response.status(), axum::http::StatusCode::NO_CONTENT);
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .and_then(|value| value.to_str().ok()),
        Some("https://console.example.org")
    );
}

#[tokio::test]
async fn a_proxied_request_without_a_security_context_is_a_gateway_error() {
    let (plane, _server) = data_plane();
    let request = http::Request::builder()
        .method("GET")
        .uri(format!("{BASE}/proxy/{ALIAS}/v1/things"))
        .body(axum::body::Body::empty())
        .unwrap();
    let response = plane.router.clone().oneshot(request).await.unwrap();

    assert_eq!(response.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        response
            .headers()
            .get(crate::infra::proxy::ERROR_SOURCE_HEADER)
            .unwrap(),
        crate::infra::proxy::ERROR_SOURCE_GATEWAY
    );
}
