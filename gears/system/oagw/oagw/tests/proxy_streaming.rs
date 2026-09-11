//! Integration tests for the streaming data plane: server-sent events and
//! `WebSocket` relaying.

// The raw-socket upstream helpers below are not themselves `#[test]`
// functions, so clippy's `allow-expect-in-tests` heuristic does not reach
// them even though they only ever run as test fixtures; see the note in
// `tests/common/mod.rs`.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use common::{base_config, config_with_timeout, create, empty_request};
use futures_util::{SinkExt, StreamExt};
use http::{Method, StatusCode};
use http_body_util::BodyExt;
use oagw::domain::model::PROTOCOL_HTTP;
use serde_json::json;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Stand up a bare HTTP/1.1 server that writes the response head immediately,
/// then dribbles out `count` SSE events, `delay` apart, chunk-encoded — a raw
/// socket is the only reliable way to control inter-chunk timing, since
/// `httpmock` cannot delay between chunks.
async fn spawn_delayed_sse_upstream(count: usize, delay: Duration) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let port = listener.local_addr().expect("local addr").port();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept one connection");
        // Drain the request until the header terminator; we don't need to
        // parse it, only stop reading before it would block.
        let mut buf = [0u8; 4096];
        let mut seen = Vec::new();
        loop {
            let n = socket.read(&mut buf).await.expect("read request");
            if n == 0 {
                return;
            }
            seen.extend_from_slice(&buf[..n]);
            if seen.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n";
        socket.write_all(head.as_bytes()).await.expect("write head");
        socket.flush().await.expect("flush head");
        for i in 0..count {
            tokio::time::sleep(delay).await;
            let payload = format!("data: event-{i}\n\n");
            let framed = format!("{:x}\r\n{payload}\r\n", payload.len());
            socket
                .write_all(framed.as_bytes())
                .await
                .expect("write chunk");
            socket.flush().await.expect("flush chunk");
        }
        socket
            .write_all(b"0\r\n\r\n")
            .await
            .expect("write terminator");
        socket.flush().await.expect("flush terminator");
    });
    port
}

#[tokio::test]
async fn sse_events_arrive_incrementally_and_outlive_a_one_second_proxy_timeout() {
    let delay = Duration::from_millis(300);
    let event_count = 4;
    let port = spawn_delayed_sse_upstream(event_count, delay).await;

    // A 1-second proxy_timeout_secs must bound only reaching the response
    // head; the four delayed chunks below take ~1.2s to fully arrive.
    let (router, _state) = common::build_router(config_with_timeout(1));
    let upstream = json!({
        "alias": "svc-sse",
        "server": {"endpoints": [{"scheme": "http", "host": "127.0.0.1", "port": port}]},
        "protocol": PROTOCOL_HTTP,
    });
    let upstream = create(&router, "/oagw/v1/upstreams", &upstream).await;
    let route = json!({
        "upstream_id": upstream["uuid"],
        "match": {"http": {"methods": ["GET"], "path": "/events"}},
    });
    create(&router, "/oagw/v1/routes", &route).await;

    let start = tokio::time::Instant::now();
    let response = tower::ServiceExt::oneshot(
        router.clone(),
        empty_request(Method::GET, "/oagw/v1/proxy/svc-sse/events"),
    )
    .await
    .expect("router is infallible");
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the upstream head must be relayed"
    );
    assert_eq!(
        response
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream"),
        "content-type must be relayed unchanged"
    );

    let mut body = response.into_body();
    let mut arrivals = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.expect("frame reads without a transport error");
        if let Some(data) = frame.data_ref()
            && !data.is_empty()
        {
            arrivals.push(tokio::time::Instant::now());
        }
    }

    assert_eq!(
        arrivals.len(),
        event_count,
        "all {event_count} events must have arrived as separate data frames"
    );
    let spread = *arrivals.last().expect("at least one arrival") - arrivals[0];
    let event_count_u32 = u32::try_from(event_count).expect("small test event count fits u32");
    assert!(
        spread >= delay * (event_count_u32 - 1) / 2,
        "events must arrive with a measurable spread, not all at once: {spread:?}"
    );
    let total = tokio::time::Instant::now() - start;
    assert!(
        total > Duration::from_secs(1),
        "the stream must outlive the 1s proxy_timeout_secs, took only {total:?}"
    );
}

#[tokio::test]
async fn websocket_handshake_echoes_a_frame_and_propagates_close() {
    use tokio_tungstenite::tungstenite::Message;

    // ---- the upstream: a bare tokio-tungstenite echo server --------------
    let ws_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ws upstream");
    let ws_port = ws_listener.local_addr().expect("local addr").port();
    tokio::spawn(async move {
        if let Ok((stream, _)) = ws_listener.accept().await
            && let Ok(ws) = tokio_tungstenite::accept_async(stream).await
        {
            let (mut tx, mut rx) = ws.split();
            while let Some(Ok(message)) = rx.next().await {
                let closing = message.is_close();
                if tx.send(message).await.is_err() {
                    break;
                }
                if closing {
                    break;
                }
            }
        }
    });

    // ---- the gateway itself, bound on a real ephemeral port ---------------
    let (router, _state) = common::build_router(base_config());
    let upstream = json!({
        "alias": "svc-ws",
        "server": {"endpoints": [{"scheme": "ws", "host": "127.0.0.1", "port": ws_port}]},
        "protocol": PROTOCOL_HTTP,
    });
    let upstream = create(&router, "/oagw/v1/upstreams", &upstream).await;
    let route = json!({
        "upstream_id": upstream["uuid"],
        "match": {"http": {"methods": ["GET"], "path": "/chat"}},
    });
    create(&router, "/oagw/v1/routes", &route).await;

    let gateway_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind gateway");
    let gateway_port = gateway_listener.local_addr().expect("local addr").port();
    tokio::spawn(async move {
        axum::serve(gateway_listener, router)
            .await
            .expect("gateway serve");
    });

    // ---- the client: a real WebSocket handshake through the proxy ---------
    let url = format!("ws://127.0.0.1:{gateway_port}/oagw/v1/proxy/svc-ws/chat");
    let (mut client, handshake_response) = tokio::time::timeout(
        Duration::from_secs(5),
        tokio_tungstenite::connect_async(url),
    )
    .await
    .expect("handshake did not hang")
    .expect("handshake succeeds");
    assert_eq!(
        handshake_response.status(),
        101,
        "the handshake must reach a 101 Switching Protocols reply"
    );

    client
        .send(Message::text("ping-through-the-gateway"))
        .await
        .expect("send text frame");
    let echoed = tokio::time::timeout(Duration::from_secs(5), client.next())
        .await
        .expect("echo did not hang")
        .expect("stream yields a message")
        .expect("message reads without error");
    assert_eq!(
        echoed,
        Message::text("ping-through-the-gateway"),
        "the text frame must echo back through both legs unchanged"
    );

    client.send(Message::Close(None)).await.expect("send close");
    let after_close = tokio::time::timeout(Duration::from_secs(5), client.next())
        .await
        .expect("close propagation did not hang");
    match after_close {
        Some(Ok(Message::Close(_))) | None => {}
        other => panic!("expected the close to propagate back, got {other:?}"),
    }
}
