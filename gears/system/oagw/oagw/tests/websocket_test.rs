//! WebSocket integration tests: a real handshake through the gateway's router
//! served over a real HTTP connection, frames relayed in both directions and a
//! client that walks away closing the upstream leg.
//!
//! `tower::ServiceExt::oneshot` cannot carry an upgrade — there is no
//! connection underneath it — so the harness router is served with
//! `axum::serve`, which hands the handler a genuine `OnUpgrade`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use common::Harness;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio_tungstenite::tungstenite::Message;

/// A WebSocket client connected to the gateway.
type GatewaySocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Starts the gear's router on a real port, returning its address.
async fn serve_gateway(harness: &Harness) -> (String, u16) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("the gateway binds");
    let address = listener.local_addr().expect("address");
    let app = harness.router.clone();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("the gateway serves");
    });
    (address.ip().to_string(), address.port())
}

/// Starts an echoing WebSocket upstream.
///
/// It answers the handshake, echoes the first frame it receives and then pushes
/// one frame of its own, so both directions of the relay are exercised.
async fn serve_upstream(closed: Arc<AtomicBool>) -> (String, u16) {
    let app = axum::Router::new()
        .route("/v1/socket", axum::routing::get(socket_handler))
        .with_state(closed);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("the upstream binds");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("the upstream serves");
    });
    (address.ip().to_string(), address.port())
}

/// The upgrade-capable upstream handler.
async fn socket_handler(
    axum::extract::State(closed): axum::extract::State<Arc<AtomicBool>>,
    upgrade: axum::extract::ws::WebSocketUpgrade,
) -> axum::response::Response {
    upgrade.on_upgrade(move |socket| async move {
        let (mut sink, mut stream) = socket.split();
        if let Some(Ok(message)) = stream.next().await
            && sink.send(message).await.is_err()
        {
            return;
        }
        if sink
            .send(axum::extract::ws::Message::text("goodbye"))
            .await
            .is_err()
        {
            return;
        }
        drop(sink);
        closed.store(true, Ordering::SeqCst);
    })
}

/// Seeds the upstream, the route and the gateway, and connects a client.
async fn connected() -> (Harness, GatewaySocket) {
    let harness = Harness::without_mock();
    let closed = Arc::new(AtomicBool::new(false));
    let (upstream_host, upstream_port) = serve_upstream(Arc::clone(&closed)).await;
    let (status, body) = harness
        .json(
            "POST",
            "/oagw/v1/upstreams",
            Some(json!({
                "alias": "ws.example.com",
                "protocol": oagw::gts_helpers::PROTOCOL_HTTP,
                "server": {"endpoints": [
                    {"scheme": "http", "host": upstream_host, "port": upstream_port}
                ]}
            })),
        )
        .await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().unwrap().to_owned();
    let (status, body) = harness
        .json(
            "POST",
            &format!("/oagw/v1/upstreams/{id}/routes"),
            Some(json!({
                "match": {"http": {"path": "/v1/socket", "methods": ["GET"]}}
            })),
        )
        .await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "{body}");

    let (_host, port) = serve_gateway(&harness).await;
    let (socket, response) = tokio_tungstenite::connect_async(format!(
        "ws://127.0.0.1:{port}/oagw/v1/proxy/ws.example.com/v1/socket"
    ))
    .await
    .expect("the handshake completes");
    assert_eq!(
        response.status(),
        http::StatusCode::SWITCHING_PROTOCOLS,
        "{response:?}"
    );
    (harness, socket)
}

#[tokio::test]
async fn a_websocket_handshake_is_answered_and_frames_are_relayed_both_ways() {
    let (_harness, mut socket) = connected().await;

    assert_eq!(
        socket
            .send(Message::text("hello"))
            .await
            .map_err(|error| error.to_string()),
        Ok(()),
        "the client frame went out"
    );
    let echoed = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
        .await
        .expect("the echo arrives")
        .expect("the stream continues");
    assert_eq!(
        echoed.unwrap(),
        Message::text("hello"),
        "the upstream echoed"
    );

    let pushed = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
        .await
        .expect("the pushed frame arrives")
        .expect("the stream continues");
    assert_eq!(
        pushed.unwrap(),
        Message::text("goodbye"),
        "the upstream pushed one"
    );
}

#[tokio::test]
async fn the_upgrade_answer_carries_the_upstream_error_source() {
    let harness = Harness::without_mock();
    let closed = Arc::new(AtomicBool::new(false));
    let (upstream_host, upstream_port) = serve_upstream(closed).await;
    let (status, body) = harness
        .json(
            "POST",
            "/oagw/v1/upstreams",
            Some(json!({
                "alias": "ws.example.com",
                "protocol": oagw::gts_helpers::PROTOCOL_HTTP,
                "server": {"endpoints": [
                    {"scheme": "http", "host": upstream_host, "port": upstream_port}
                ]}
            })),
        )
        .await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().unwrap().to_owned();
    let (status, body) = harness
        .json(
            "POST",
            &format!("/oagw/v1/upstreams/{id}/routes"),
            Some(json!({
                "match": {"http": {"path": "/v1/socket", "methods": ["GET"]}}
            })),
        )
        .await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "{body}");
    let (_host, port) = serve_gateway(&harness).await;

    let (_, response) = tokio_tungstenite::connect_async(format!(
        "ws://127.0.0.1:{port}/oagw/v1/proxy/ws.example.com/v1/socket"
    ))
    .await
    .expect("the handshake completes");

    assert_eq!(
        response
            .headers()
            .get("x-oagw-error-source")
            .and_then(|value| value.to_str().ok()),
        Some("upstream"),
        "the tunnel was opened by the upstream"
    );
    assert_eq!(
        harness.metrics.snapshot().websocket_tunnels,
        1,
        "the tunnel is accounted for"
    );
}

#[tokio::test]
async fn an_upgrade_to_an_unknown_alias_is_a_problem_document() {
    let harness = Harness::without_mock();
    let (_host, port) = serve_gateway(&harness).await;

    let error = tokio_tungstenite::connect_async(format!(
        "ws://127.0.0.1:{port}/oagw/v1/proxy/no-such-alias/v1/socket"
    ))
    .await
    .expect_err("an unknown alias is refused");

    let status = match error {
        tokio_tungstenite::tungstenite::Error::Http(response) => response.status(),
        other => panic!("the refusal was an HTTP answer, not {other:?}"),
    };
    assert_eq!(status, http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_client_that_leaves_closes_the_upstream_socket() {
    let harness = Harness::without_mock();
    let closed = Arc::new(AtomicBool::new(false));
    let (upstream_host, upstream_port) = serve_upstream(Arc::clone(&closed)).await;
    let (status, body) = harness
        .json(
            "POST",
            "/oagw/v1/upstreams",
            Some(json!({
                "alias": "ws.example.com",
                "protocol": oagw::gts_helpers::PROTOCOL_HTTP,
                "server": {"endpoints": [
                    {"scheme": "http", "host": upstream_host, "port": upstream_port}
                ]}
            })),
        )
        .await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().unwrap().to_owned();
    let (status, body) = harness
        .json(
            "POST",
            &format!("/oagw/v1/upstreams/{id}/routes"),
            Some(json!({
                "match": {"http": {"path": "/v1/socket", "methods": ["GET"]}}
            })),
        )
        .await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "{body}");
    let (_host, port) = serve_gateway(&harness).await;

    let (mut socket, _) = tokio_tungstenite::connect_async(format!(
        "ws://127.0.0.1:{port}/oagw/v1/proxy/ws.example.com/v1/socket"
    ))
    .await
    .expect("the handshake completes");
    socket
        .send(Message::text("hello"))
        .await
        .expect("the frame goes out");
    // Dropping the client socket tears the relay down with it.
    drop(socket);

    for _ in 0..50 {
        if closed.load(Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("the upstream socket was never closed");
}
