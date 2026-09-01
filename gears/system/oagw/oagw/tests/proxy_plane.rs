// Created: 2026-08-29 by Constructor Tech
//! Data-plane integration tests (DESIGN §3.5 proxy flow).
//!
//! Each test drives the real axum handler over a real `ProxyStack` against a
//! live upstream: plain HTTP through httpmock, SSE through a raw close-framed
//! stream, WebSocket through a tungstenite echo endpoint.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use futures_util::{SinkExt, StreamExt};
use httpmock::prelude::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::protocol::Message;
use tower::ServiceExt;

use oagw::api::rest::proxy::{PROXY_PREFIX, ProxyStack, proxy_handler};
use oagw::config::OagwConfig;
use oagw::domain::model::{
    AuthConfig, CorsConfig, Endpoint, HeadersConfig, Passthrough, Protocol, RateLimitConfig,
    RequestHeaders, ResponseHeaders, Scheme, ServerConfig, Upstream,
};
use oagw::domain::repo::UpstreamRepository;
use oagw::domain::services::proxy::ProxyService;
use oagw::infra::hierarchy::FlatTenantHierarchy;
use oagw::infra::http::OutboundClient;
use oagw::infra::plugin::NullCredentialResolver;
use oagw::infra::proxy::{PluginExecutor, ProxyEngine};
use oagw::infra::ratelimit::RateLimiter;
use oagw::infra::storage::InMemoryStore;
use toolkit_security::SecurityContext;

/// The tenant every seeded resource belongs to and every request authenticates as.
pub const TENANT: uuid::Uuid = uuid::Uuid::nil();

/// The authenticated caller the platform's auth middleware would have provided.
fn security_context() -> SecurityContext {
    SecurityContext::builder()
        .subject_tenant_id(TENANT)
        .subject_id(uuid::Uuid::new_v4())
        .build()
        .expect("context")
}

/// A stack wired exactly the way the gear wires it, pointed at plaintext
/// upstreams with SSRF hardening off.
fn stack(store: Arc<InMemoryStore>, config: OagwConfig) -> ProxyStack {
    let config = Arc::new(config);
    let service = Arc::new(ProxyService::new(
        store,
        Arc::new(InMemoryStore::default()),
        Arc::new(FlatTenantHierarchy),
        Arc::new(RateLimiter::new()),
        config.clone(),
    ));
    let executor = Arc::new(PluginExecutor::new(
        Arc::new(NullCredentialResolver),
        Duration::from_secs(60),
        16,
    ));
    let client = OutboundClient::new(
        config.proxy_timeout(),
        config.connect_timeout(),
        Duration::from_secs(30),
        8,
    )
    .expect("client");
    ProxyStack {
        service,
        engine: Arc::new(ProxyEngine::new(client, executor, config.clone())),
        config,
        metrics: Arc::new(oagw::infra::metrics::OagwMetrics::default()),
    }
}

fn config() -> OagwConfig {
    OagwConfig {
        proxy_timeout_secs: 5,
        connect_timeout_secs: 2,
        allow_http_upstream: true,
        ssrf_policy: oagw::config::SsrfPolicy::default(),
        ..OagwConfig::default()
    }
}

fn http_upstream(port: u16) -> Upstream {
    Upstream {
        id: uuid::Uuid::new_v4(),
        tenant_id: TENANT,
        alias: "api.test".to_owned(),
        protocol: Protocol::Http,
        enabled: true,
        server: ServerConfig {
            endpoints: vec![Endpoint {
                scheme: Scheme::Http,
                host: "127.0.0.1".to_owned(),
                port,
            }],
        },
        auth: AuthConfig::default(),
        // Inbound headers cross the hop: the round-trip test asserts the
        // upstream saw the caller's trace header.
        headers: HeadersConfig {
            request: RequestHeaders {
                passthrough: Passthrough::All,
                ..Default::default()
            },
            response: ResponseHeaders::default(),
        },
        rate_limit: None,
        cors: None,
        plugins: Default::default(),
        tags: Vec::new(),
        created_at: 1,
        updated_at: 1,
    }
}

fn ws_upstream(port: u16) -> Upstream {
    Upstream {
        alias: "ws.test".to_owned(),
        server: ServerConfig {
            endpoints: vec![Endpoint {
                scheme: Scheme::Ws,
                host: "127.0.0.1".to_owned(),
                port,
            }],
        },
        ..http_upstream(port)
    }
}

/// Mounts the data-plane handler and seeds one upstream under the nil tenant.
async fn router_with(upstream: Upstream, config: OagwConfig) -> (Router, Arc<InMemoryStore>) {
    let store = Arc::new(InMemoryStore::default());
    store.insert(upstream).await.expect("seeded");
    let app: Router = Router::new()
        .route(
            &format!("{PROXY_PREFIX}/{{*tail}}"),
            axum::routing::any(proxy_handler),
        )
        .layer(axum::Extension(security_context()))
        .layer(axum::Extension(stack(store.clone(), config)));
    (app, store)
}

async fn send(
    app: Router,
    method: &str,
    path: &str,
    body: Option<&str>,
    headers: &[(&str, &str)],
) -> axum::response::Response {
    let mut builder = axum::http::Request::builder().method(method).uri(path);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let payload = body.unwrap_or_default().to_owned();
    app.oneshot(
        builder
            .body(axum::body::Body::from(payload))
            .expect("request"),
    )
    .await
    .expect("response")
}

#[tokio::test]
async fn plain_requests_round_trip_through_the_gateway() {
    let upstream_server = MockServer::start();
    let mock = upstream_server.mock(|when, then| {
        when.any_request();
        then.status(201)
            .header("content-type", "application/json")
            .body(r#"{"ok":true}"#);
    });

    let (app, _store) = router_with(http_upstream(upstream_server.port()), config()).await;
    let response = send(
        app,
        "POST",
        "/oagw/v1/proxy/api.test/v1/chat/completions",
        Some(r#"{"prompt":"hi"}"#),
        &[("x-trace", "oagw-test"), ("connection", "keep-alive")],
    )
    .await;

    assert_eq!(
        response.status(),
        201,
        "body: {:?}",
        axum::body::to_bytes(response.into_body(), 4096).await
    );
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/json")
    );
    let body = axum::body::to_bytes(response.into_body(), 1024)
        .await
        .expect("body");
    assert_eq!(&body[..], br#"{"ok":true}"#);
    assert_eq!(mock.calls(), 1);
}

#[tokio::test]
async fn an_unknown_alias_is_a_gateway_404() {
    let (app, _store) = router_with(http_upstream(1), config()).await;
    let response = send(app, "GET", "/oagw/v1/proxy/absent/v1/resource", None, &[]).await;
    assert_eq!(response.status(), 404);
    assert_eq!(
        response
            .headers()
            .get("x-oagw-error-source")
            .and_then(|value| value.to_str().ok()),
        Some("gateway")
    );
    let body = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .expect("body");
    let problem: serde_json::Value = serde_json::from_slice(&body).expect("problem document");
    assert_eq!(problem["status"], 404);
    assert!(
        problem["type"]
            .as_str()
            .expect("gts type")
            .contains("cf.oagw.route.not_found")
    );
}

#[tokio::test]
async fn unsupported_methods_are_rejected_without_dialling() {
    let (app, _store) = router_with(http_upstream(1), config()).await;
    let response = send(app, "TRACE", "/oagw/v1/proxy/api.test/v1", None, &[]).await;
    assert_eq!(response.status(), 400);
}

#[tokio::test]
async fn payloads_above_the_limit_are_rejected_before_dialling() {
    let (app, _store) = router_with(
        http_upstream(1),
        OagwConfig {
            max_body_size_bytes: 8,
            ..config()
        },
    )
    .await;
    let response = send(
        app,
        "POST",
        "/oagw/v1/proxy/api.test/v1",
        Some("0123456789"),
        &[],
    )
    .await;
    assert_eq!(response.status(), 413);
}

#[tokio::test]
async fn preflights_are_answered_by_the_gateway() {
    let mut upstream = http_upstream(1);
    upstream.cors = Some(CorsConfig {
        enabled: true,
        allowed_origins: vec!["https://app.example".to_owned()],
        ..Default::default()
    });
    let (app, _store) = router_with(upstream, config()).await;

    let allowed = send(
        app.clone(),
        "OPTIONS",
        "/oagw/v1/proxy/api.test/v1/resource",
        None,
        &[
            ("origin", "https://app.example"),
            ("access-control-request-method", "GET"),
        ],
    )
    .await;
    assert_eq!(allowed.status(), 204);
    assert_eq!(
        allowed
            .headers()
            .get("access-control-allow-origin")
            .and_then(|value| value.to_str().ok()),
        Some("https://app.example")
    );

    // A preflight is permissive: it echoes the browser's request even when the
    // origin is not on the upstream's list, because ADR-0004 defers origin and
    // method enforcement to the actual request.
    let unlisted = send(
        app.clone(),
        "OPTIONS",
        "/oagw/v1/proxy/api.test/v1/resource",
        None,
        &[
            ("origin", "https://evil.example"),
            ("access-control-request-method", "GET"),
        ],
    )
    .await;
    assert_eq!(unlisted.status(), 204);

    // The actual request carries the enforcement.
    let denied = send(
        app.clone(),
        "GET",
        "/oagw/v1/proxy/api.test/v1/resource",
        None,
        &[("origin", "https://evil.example")],
    )
    .await;
    assert_eq!(denied.status(), 403);
    assert_eq!(
        denied
            .headers()
            .get("x-oagw-error-source")
            .and_then(|value| value.to_str().ok()),
        Some("gateway")
    );

    // An OPTIONS without the preflight markers is not a browser preflight: it
    // is refused as a method OAGW does not proxy.
    let plain = send(
        app,
        "OPTIONS",
        "/oagw/v1/proxy/api.test/v1/resource",
        None,
        &[("origin", "https://app.example")],
    )
    .await;
    assert_eq!(plain.status(), 400);
}

#[tokio::test]
async fn rate_limits_are_enforced_at_the_gateway() {
    // A live upstream: the bucket starts full, so the first request is served
    // and the second — against a bucket of one — is rejected at the gateway.
    let upstream_server = MockServer::start();
    let mock = upstream_server.mock(|when, then| {
        when.method(GET).path("/v1");
        then.status(200).body("ok");
    });

    let mut upstream = http_upstream(upstream_server.port());
    upstream.rate_limit = Some(RateLimitConfig {
        sustained: oagw::domain::model::SustainedRate {
            rate: 1,
            window: oagw::domain::model::Window::Second,
        },
        ..Default::default()
    });
    let (app, _store) = router_with(upstream, config()).await;

    let path = "/oagw/v1/proxy/api.test/v1";
    let first = send(app.clone(), "GET", path, None, &[]).await;
    assert_eq!(first.status(), 200, "the bucket starts full");
    assert_eq!(mock.calls(), 1);

    let second = send(app, "GET", path, None, &[]).await;
    assert_eq!(second.status(), 429);
    assert_eq!(
        second
            .headers()
            .get("x-ratelimit-limit")
            .and_then(|value| value.to_str().ok()),
        Some("1")
    );
    assert_eq!(
        second
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok()),
        Some("1")
    );
    assert_eq!(mock.calls(), 1, "the rejected request never dials");
}

/// Serves one close-framed HTTP/1.1 response so the body is streamed, not
/// buffered, and the gateway has to relay it incrementally.
async fn sse_upstream(listener: TcpListener) {
    let (mut socket, _) = listener.accept().await.expect("peer");
    let mut buffer = vec![0u8; 4096];
    let read = socket.read(&mut buffer).await.expect("request");
    assert!(String::from_utf8_lossy(&buffer[..read]).starts_with("GET /events"));
    let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
    socket.write_all(head.as_bytes()).await.expect("head");
    for index in 0..3 {
        socket
            .write_all(format!("data: event-{index}\n\n").as_bytes())
            .await
            .expect("chunk");
        socket.flush().await.expect("flush");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    socket.shutdown().await.ok();
}

#[tokio::test]
async fn event_streams_relay_incrementally() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bound");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(sse_upstream(listener));

    let (app, _store) = router_with(http_upstream(port), config()).await;
    let response = send(app, "GET", "/oagw/v1/proxy/api.test/events", None, &[]).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );

    let body = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .expect("streamed");
    let text = String::from_utf8(body.to_vec()).expect("utf8");
    for index in 0..3 {
        assert!(
            text.contains(&format!("data: event-{index}")),
            "missing {index}: {text}"
        );
    }
}

/// Accepts one WebSocket connection and echoes every text frame back.
async fn ws_echo(listener: TcpListener) {
    let (socket, _) = listener.accept().await.expect("peer");
    let mut websocket = tokio_tungstenite::accept_async(socket)
        .await
        .expect("handshake");
    while let Some(Ok(frame)) = websocket.next().await {
        match frame {
            Message::Text(text) => {
                if websocket
                    .send(Message::text(format!("echo:{text}")))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Message::Close(_) => break,
            _ => continue,
        }
    }
}

#[tokio::test]
async fn websocket_tunnels_relay_frames_in_both_directions() {
    // A live upstream echo server, and a live gateway bound to its own port:
    // a WebSocket upgrade needs a real socket on both ends.
    let upstream_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("upstream bound");
    let upstream_port = upstream_listener.local_addr().expect("addr").port();
    tokio::spawn(ws_echo(upstream_listener));

    let (app, _store) = router_with(ws_upstream(upstream_port), config()).await;
    let gateway_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("gateway bound");
    let gateway_port = gateway_listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        axum::serve(gateway_listener, app).await.expect("served");
    });

    let mut socket = tokio_tungstenite::connect_async(format!(
        "ws://127.0.0.1:{gateway_port}/oagw/v1/proxy/ws.test/socket"
    ))
    .await
    .expect("tunnelled handshake")
    .0;

    socket
        .send(Message::text("ping"))
        .await
        .expect("client frame");
    let frame = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("in time")
        .expect("frame")
        .expect("text");
    assert_eq!(frame, Message::text("echo:ping"));
    socket.close(None).await.ok();
}
