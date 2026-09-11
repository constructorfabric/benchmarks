//! Black-box, external-crate tests for DECOMPOSITION entry 2.6
//! (proxy-streaming), exercising the publicly reachable surface of
//! `oagw::*`.
//!
//! `oagw::proxy::stream` and every other `oagw::proxy::*` module are
//! `pub(crate)`, so the SSE-incrementality, WebSocket-handshake and
//! frame-relay, and `StreamAborted` lifecycle tests that actually drive
//! `engine::handle_proxy_request` end to end live as an inline
//! `#[cfg(test)] mod streaming_tests` inside `src/proxy/engine.rs`, which --
//! being part of the `oagw` crate itself -- has the access this file
//! cannot, exactly as `tests/proxy_core.rs` documents for the analogous
//! entry 2.5 case. This file exercises the part of
//! `cpt-cf-oagw-algo-stream-abort-handling`'s `StreamAborted` catalog entry
//! (`cpt-cf-oagw-dod-stream-abort-error`) that genuinely is public: the
//! renderer every `StreamAborted` gateway error (rendered from
//! `proxy::stream`) goes through.
//!
//! It also reaches one acceptance criterion the inline `engine.rs` suite
//! does not yet cover (client-disconnect closes the upstream connection,
//! acceptance criterion 2) through the crate's public `oagw::OagwGear`
//! (`impl toolkit::Gear` / `impl toolkit::RestApiCapability`) -- the same
//! entry point the platform calls in production, reached here without
//! naming a single `pub(crate)` item (see `tests/cors_handling.rs`'s module
//! doc comment for the full reasoning behind this technique, which this
//! file reuses -- duplicated locally rather than shared, per this phase's
//! "no `tests/common`" constraint).

#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::IntoResponse;
use http_body_util::BodyExt;
use oagw::OagwGear;
use oagw::error::{ERROR_SOURCE_GATEWAY, ERROR_SOURCE_HEADER_NAME, OagwError, OagwErrorKind};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use toolkit::api::OpenApiRegistryImpl;
use toolkit::{ClientHub, ConfigProvider, Gear, GearCtx, RestApiCapability};
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

/// `cpt-cf-oagw-dod-stream-abort-error`: a `StreamAborted` gateway error
/// renders the documented `502` status, GTS type and
/// `X-OAGW-Error-Source: gateway` header -- the exact envelope
/// `proxy::stream::StreamAuditContext::render_stream_aborted` and
/// `websocket_handshake`-failure rendering both build on top of.
#[tokio::test]
async fn stream_aborted_renders_the_documented_502_envelope() {
    let error = OagwError::new(
        OagwErrorKind::StreamAborted,
        "the upstream connection failed before the event-stream response was committed to the client",
    )
    .with_instance("/oagw/v1/proxy/sse-svc/events")
    .with_host("sse-svc")
    .with_trace_id("req-123");

    let response = error.into_response();
    assert_eq!(response.status().as_u16(), 502);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/problem+json")
    );
    assert_eq!(
        response
            .headers()
            .get(ERROR_SOURCE_HEADER_NAME)
            .and_then(|v| v.to_str().ok()),
        Some(ERROR_SOURCE_GATEWAY)
    );

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1"
    );
    assert_eq!(json["status"], 502);
    assert_eq!(json["host"], "sse-svc");
    assert_eq!(json["trace_id"], "req-123");
    assert_eq!(json["instance"], "/oagw/v1/proxy/sse-svc/events");
}

// ---------------------------------------------------------------------
// Full-router, black-box test driven through the real `oagw::OagwGear`
// (see the module doc comment, and `tests/cors_handling.rs`'s module doc
// comment for the full reasoning).
// ---------------------------------------------------------------------

struct FixedConfig(Value);

impl ConfigProvider for FixedConfig {
    fn get_gear_config(&self, gear: &str) -> Option<&Value> {
        (gear == OagwGear::MODULE_NAME).then_some(&self.0)
    }
}

/// Build the real `oagw` gear router through the public
/// `toolkit::Gear`/`toolkit::RestApiCapability` entry points, exactly as
/// the platform does in production.
async fn build_router(allow_http_upstream: bool) -> Router {
    let gear = OagwGear::default();
    let ctx = GearCtx::new(
        OagwGear::MODULE_NAME,
        Uuid::new_v4(),
        Arc::new(FixedConfig(json!({
            "config": {
                "proxy_timeout_secs": 5,
                "allow_http_upstream": allow_http_upstream,
            }
        }))) as Arc<dyn ConfigProvider>,
        Arc::new(ClientHub::new()),
        Default::default(),
    );
    gear.init(&ctx)
        .await
        .expect("gear init must succeed with a well-formed fixed config");
    let openapi = OpenApiRegistryImpl::new();
    gear.register_rest(&ctx, Router::new(), &openapi)
        .expect("register_rest must succeed")
}

fn security_context(tenant_id: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(tenant_id)
        .build()
        .unwrap()
}

fn json_request(method: &str, uri: &str, tenant_id: Uuid, body: Value) -> Request<Body> {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    req.extensions_mut().insert(security_context(tenant_id));
    req
}

async fn response_json(response: axum::response::Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap_or_default()
}

/// Create a plaintext-`http` Upstream and a `GET {path}` Route through the
/// real Upstream/Route Management REST endpoints, bound to
/// `127.0.0.1:{port}`. Returns the derived alias.
async fn create_http_upstream_and_route(
    router: &Router,
    tenant_id: Uuid,
    alias: &str,
    port: u16,
    path: &str,
) -> String {
    let body = json!({
        "alias": alias,
        "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": port } ] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
    });
    let response = router
        .clone()
        .oneshot(json_request("POST", "/oagw/v1/upstreams", tenant_id, body))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let created = response_json(response).await;
    let upstream_id: Uuid = created["id"].as_str().unwrap().parse().unwrap();

    let body = json!({
        "upstream_id": upstream_id,
        "match": { "http": { "methods": ["GET"], "path": path } },
        "priority": 1,
    });
    let response = router
        .clone()
        .oneshot(json_request("POST", "/oagw/v1/routes", tenant_id, body))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    alias.to_owned()
}

fn http_chunk(data: &[u8]) -> Vec<u8> {
    let mut out = format!("{:x}\r\n", data.len()).into_bytes();
    out.extend_from_slice(data);
    out.extend_from_slice(b"\r\n");
    out
}

/// A raw chunked-encoding `text/event-stream` upstream that sends one
/// chunk, then blocks on reading from the same socket: once the gateway's
/// own connection to this upstream is closed (as a consequence of the
/// downstream client disconnecting), that read observes end-of-stream
/// (`Ok(0)`), which this function reports on `report`. Mirrors
/// `spawn_sse_upstream` in `src/proxy/engine.rs`'s inline test module,
/// duplicated locally rather than shared.
async fn spawn_sse_upstream_reporting_peer_close() -> (u16, tokio::sync::oneshot::Receiver<bool>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buf = [0u8; 4096];
            // Skip proxy-core's own throwaway `probe_connect` (an
            // immediate `Ok(0)` read), exactly as
            // `src/proxy/engine.rs`'s `spawn_sse_upstream` does.
            match socket.read(&mut buf).await {
                Ok(0) | Err(_) => continue,
                Ok(_) => {}
            }
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n")
                .await
                .unwrap();
            socket
                .write_all(&http_chunk(b"data: first\n\n"))
                .await
                .unwrap();
            socket.flush().await.unwrap();

            let mut probe = [0u8; 16];
            let closed = tokio::time::timeout(Duration::from_secs(2), socket.read(&mut probe))
                .await
                .map(|read_result| matches!(read_result, Ok(0)))
                .unwrap_or(false);
            let _ = tx.send(closed);
            return;
        }
    });
    (port, rx)
}

/// Acceptance criterion 2: when the client disconnects while an SSE stream
/// is open, the corresponding upstream connection is observably closed as
/// part of the same request lifecycle (`cpt-cf-oagw-dod-stream-sse-lifecycle`).
/// A real TCP client is required (not `oneshot`, which never opens a socket
/// the gateway could observe closing) -- driven through the real gear
/// router via `axum::serve`, exactly as `src/proxy/engine.rs`'s own
/// WebSocket tests do for the analogous reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_disconnect_while_sse_streaming_closes_the_upstream_connection() {
    let (upstream_port, upstream_closed_rx) = spawn_sse_upstream_reporting_peer_close().await;

    let router = build_router(true).await;
    let tenant_id = Uuid::new_v4();
    let alias = create_http_upstream_and_route(
        &router,
        tenant_id,
        "sse-disconnect-svc",
        upstream_port,
        "/events",
    )
    .await;

    // `build_router`'s `SecurityContext` is only usable via a hand-built
    // `Request`'s extensions (as every other test in this file does); a
    // real inbound `axum::serve` connection has no such per-request
    // Rust-typed extension point, so mount a shim in front of the real
    // router that injects a fixed `SecurityContext` -- the same
    // constraint, and the same fix, `src/proxy/engine.rs`'s own
    // `build_router` documents for its WebSocket tests.
    let router = router.layer(axum::Extension(security_context(tenant_id)));
    let gateway_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway_addr = gateway_listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(gateway_listener, router).await.unwrap();
    });

    let mut client = tokio::net::TcpStream::connect(gateway_addr).await.unwrap();
    client
        .write_all(
            format!(
                "GET /oagw/v1/proxy/{alias}/events HTTP/1.1\r\n\
                 Host: {gateway_addr}\r\n\
                 Connection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();

    // Read until the first event has arrived (status line, headers, and
    // the chunked payload), proving the stream was actually committed and
    // streaming before this test disconnects it.
    let mut received = Vec::new();
    let mut buf = [0u8; 256];
    loop {
        let n = tokio::time::timeout(Duration::from_secs(2), client.read(&mut buf))
            .await
            .expect("must receive the first event before timing out")
            .unwrap();
        assert_ne!(n, 0, "connection closed before the first event arrived");
        received.extend_from_slice(&buf[..n]);
        if received.windows(b"first".len()).any(|w| w == b"first") {
            break;
        }
    }

    // Disconnect: drop the client's TCP connection entirely while the
    // stream is still open.
    drop(client);

    let upstream_observed_close = tokio::time::timeout(Duration::from_secs(3), upstream_closed_rx)
        .await
        .expect("the upstream must observe the connection close within the timeout")
        .expect("the reporting channel must not be dropped without a value");
    assert!(
        upstream_observed_close,
        "the upstream connection must be closed as a direct consequence of the client disconnect"
    );
}
