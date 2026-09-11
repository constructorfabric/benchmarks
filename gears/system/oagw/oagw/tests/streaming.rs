//! Streaming: server-sent events and WebSocket upgrades, through a real
//! listener so the upgrade has something driving the socket.

mod common;

use common::*;
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

/// Point an upstream at the mock and route `/v1` to it, returning the address
/// of a live gateway serving the whole thing.
async fn wired(mock: &MockUpstream) -> std::net::SocketAddr {
    let mut app = app();
    let id = app
        .create_upstream(loopback_upstream("vendor", mock.port()))
        .await;
    app.create_route(route_body(&id, "/v1")).await;
    app.serve().await
}

#[tokio::test]
async fn an_event_stream_is_relayed_chunk_by_chunk() {
    let events: Vec<Vec<u8>> = ["event: alpha\n\n", "event: beta\n\n", "event: gamma\n\n"]
        .iter()
        .map(|e| e.as_bytes().to_vec())
        .collect();
    let mock = MockUpstream::start(move |_| MockResponse {
        status: 200,
        headers: vec![("content-type", "text/event-stream")],
        body: MockBody::Chunks(events.clone()),
    })
    .await;
    let addr = wired(&mock).await;

    let mut socket = TcpStream::connect(addr).await.expect("connect");
    let request = format!(
        "GET /oagw/v1/proxy/vendor/v1/stream HTTP/1.1\r\nhost: {addr}\r\nconnection: close\r\n\r\n"
    );
    socket.write_all(request.as_bytes()).await.expect("write");

    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        let read = socket.read(&mut byte).await.expect("head");
        assert!(read > 0, "connection closed before the response head");
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert!(
        head.contains("content-type: text/event-stream"),
        "the upstream content type is preserved: {head}"
    );
    assert!(
        head.contains("transfer-encoding: chunked"),
        "the stream is forwarded as a stream: {head}"
    );

    // Three flushes upstream become three separate reads downstream: the body
    // was not buffered to completion before being forwarded.
    let mut seen = 0_usize;
    let mut body = Vec::new();
    let mut chunk = [0_u8; 256];
    while seen < 3 {
        let read = tokio::time::timeout(std::time::Duration::from_secs(2), socket.read(&mut chunk))
            .await
            .expect("no read timeout")
            .expect("read");
        assert!(read > 0, "stream closed after {seen} events");
        body.extend_from_slice(&chunk[..read]);
        seen = body.windows(2).filter(|w| w == b"\n\n").count();
    }
    let body = String::from_utf8_lossy(&body).into_owned();
    assert!(body.contains("event: alpha"), "{body}");
    assert!(body.contains("event: gamma"), "{body}");
}

#[tokio::test]
async fn a_websocket_upgrade_is_proxied_bidirectionally() {
    let port = ws_echo_upstream().await;
    let mut app = app();
    let id = app.create_upstream(loopback_upstream("vendor", port)).await;
    app.create_route(route_body(&id, "/v1")).await;
    let addr = app.serve().await;

    let url = format!("ws://{addr}/oagw/v1/proxy/vendor/v1/ws");
    let request = url.as_str().into_client_request().expect("request");
    let (mut stream, response) = tokio_tungstenite::connect_async(request)
        .await
        .expect("upgrade");
    assert_eq!(response.status(), http::StatusCode::SWITCHING_PROTOCOLS);
    assert_eq!(
        response
            .headers()
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some("upstream")
    );

    stream
        .send(tokio_tungstenite::tungstenite::Message::text("ping"))
        .await
        .expect("send");
    let echoed = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
        .await
        .expect("no timeout")
        .expect("a message")
        .expect("no ws error");
    assert_eq!(echoed.to_text().expect("text"), "ping");

    stream
        .send(tokio_tungstenite::tungstenite::Message::Close(None))
        .await
        .expect("close");
}

#[tokio::test]
async fn a_websocket_upgrade_to_a_dead_upstream_answers_a_problem() {
    let mut app = app();
    // Nothing is listening on this port.
    let id = app.create_upstream(loopback_upstream("vendor", 1)).await;
    app.create_route(route_body(&id, "/v1")).await;
    let addr = app.serve().await;

    let url = format!("ws://{addr}/oagw/v1/proxy/vendor/v1/ws");
    let request = url.as_str().into_client_request().expect("request");
    let error = tokio_tungstenite::connect_async(request).await;
    assert!(error.is_err(), "the upgrade should have been refused");
}
