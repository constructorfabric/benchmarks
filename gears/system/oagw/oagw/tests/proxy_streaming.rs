//! Streaming proxy paths: server-sent events and protocol upgrades.
//!
//! `cpt-cf-oagw-fr-streaming` is not satisfied by plain request/response
//! proxying, so both are exercised against a live upstream: SSE through the
//! in-process router (the body is a stream either way) and the WebSocket
//! upgrade through a real listener, because `oneshot` has no socket to hand
//! back on a `101`.

use std::time::Duration;

use futures_util::StreamExt;
use oagw::domain::gts_helpers as gts;
use oagw::test_support::{Harness, MockUpstream, context_for, empty_request, read_json};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

/// Register an upstream and a `/v1` route pointing at `mock`.
async fn wire(harness: &Harness, ctx: &toolkit_security::SecurityContext, mock: &MockUpstream) {
    let created = read_json(
        harness
            .post_json(
                ctx,
                "/oagw/v1/upstreams",
                &json!({
                    "server": { "endpoints": [
                        { "scheme": "http", "host": mock.host(), "port": mock.port() }
                    ] },
                    "protocol": gts::PROTOCOL_HTTP,
                    "alias": "mock-upstream"
                }),
            )
            .await,
    )
    .await;
    let route = harness
        .post_json(
            ctx,
            "/oagw/v1/routes",
            &json!({
                "upstream_id": created["id"],
                "match": { "http": { "methods": ["GET", "POST"], "path": "/v1" } }
            }),
        )
        .await;
    assert_eq!(route.status(), 201, "route create must succeed");
}

#[tokio::test]
async fn an_sse_stream_is_proxied_event_by_event() {
    let mock = MockUpstream::start().await;
    let harness = Harness::new();
    let ctx = context_for(Uuid::new_v4());
    wire(&harness, &ctx, &mock).await;

    let response = harness
        .get(&ctx, "/oagw/v1/proxy/mock-upstream/v1/sse")
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()[http::header::CONTENT_TYPE],
        "text/event-stream",
        "the upstream media type is passed through, not rewritten"
    );
    assert_eq!(response.headers()["x-oagw-error-source"], "upstream");

    // The upstream sleeps between events, so receiving the first chunk before
    // the stream ends is what proves the body was not buffered.
    let mut stream = response.into_body().into_data_stream();
    let first = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("the first event arrives before the stream completes")
        .expect("a chunk")
        .expect("a readable chunk");
    let first = String::from_utf8_lossy(&first).into_owned();
    assert!(first.contains("event: tick"), "got {first:?}");

    let mut collected = first;
    while let Some(chunk) = stream.next().await {
        collected.push_str(&String::from_utf8_lossy(&chunk.expect("chunk")));
    }
    for expected in ["data: 0", "data: 1", "data: 2", "event: done"] {
        assert!(
            collected.contains(expected),
            "the whole stream must arrive; missing {expected:?} in {collected:?}"
        );
    }
}

#[tokio::test]
async fn an_sse_stream_carries_the_upstream_cache_control() {
    let mock = MockUpstream::start().await;
    let harness = Harness::new();
    let ctx = context_for(Uuid::new_v4());
    wire(&harness, &ctx, &mock).await;
    let response = harness
        .get(&ctx, "/oagw/v1/proxy/mock-upstream/v1/sse")
        .await;
    assert_eq!(response.headers()[http::header::CACHE_CONTROL], "no-cache");
}

// ---------------------------------------------------------------------------
// WebSocket
// ---------------------------------------------------------------------------

/// Minimal client-side WebSocket framing — enough to prove that an upgrade
/// negotiated through the gateway carries application data both ways.
mod ws {
    /// Encode a masked client text frame (RFC 6455 §5.2). Payloads in these
    /// tests are short, so only the 7-bit length form is needed.
    pub fn text_frame(payload: &str) -> Vec<u8> {
        let bytes = payload.as_bytes();
        assert!(bytes.len() < 126, "test payloads stay in the short form");
        let mask = [0x37, 0xfa, 0x21, 0x3d];
        let mut frame = vec![
            0x81,
            0x80 | u8::try_from(bytes.len()).expect("short payload"),
        ];
        frame.extend_from_slice(&mask);
        for (index, byte) in bytes.iter().enumerate() {
            frame.push(byte ^ mask[index % 4]);
        }
        frame
    }

    /// Decode an unmasked server text frame, returning its payload.
    pub fn decode_text(frame: &[u8]) -> Option<String> {
        if frame.len() < 2 {
            return None;
        }
        let opcode = frame[0] & 0x0f;
        if opcode != 0x1 {
            return None;
        }
        let masked = frame[1] & 0x80 != 0;
        let length = usize::from(frame[1] & 0x7f);
        if masked || length >= 126 || frame.len() < 2 + length {
            return None;
        }
        String::from_utf8(frame[2..2 + length].to_vec()).ok()
    }
}

#[tokio::test]
async fn a_websocket_upgrade_is_proxied_and_carries_frames_both_ways() {
    let mock = MockUpstream::start().await;
    let harness = Harness::new();
    let ctx = context_for(Uuid::new_v4());
    wire(&harness, &ctx, &mock).await;
    let gateway = harness.serve(&ctx).await;

    let mut socket = tokio::net::TcpStream::connect(gateway)
        .await
        .expect("connect to the gateway");
    let handshake = concat!(
        "GET /oagw/v1/proxy/mock-upstream/v1/ws HTTP/1.1\r\n",
        "Host: gateway.local\r\n",
        "Connection: Upgrade\r\n",
        "Upgrade: websocket\r\n",
        "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n",
        "Sec-WebSocket-Version: 13\r\n",
        "\r\n"
    );
    socket
        .write_all(handshake.as_bytes())
        .await
        .expect("send the handshake");

    // Read the response head. The 101 arrives on its own, before any frame.
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        let read = tokio::time::timeout(Duration::from_secs(5), socket.read(&mut byte))
            .await
            .expect("the gateway answers the handshake")
            .expect("readable");
        assert_ne!(read, 0, "the gateway closed the connection: {head:?}");
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    assert!(
        head.starts_with("HTTP/1.1 101"),
        "expected a protocol upgrade, got: {head}"
    );
    let lower = head.to_ascii_lowercase();
    assert!(lower.contains("upgrade: websocket"), "{head}");
    assert!(lower.contains("connection: upgrade"), "{head}");
    assert!(
        lower.contains("sec-websocket-accept:"),
        "the upstream's handshake answer must be passed through: {head}"
    );
    assert!(
        lower.contains("x-oagw-error-source: upstream"),
        "the 101 is attributed to the upstream: {head}"
    );

    // The upstream echoes `echo:<text>`; getting it back proves the raw
    // transport was bridged in both directions.
    socket
        .write_all(&ws::text_frame("hello"))
        .await
        .expect("send a frame");
    let mut buffer = [0u8; 64];
    let read = tokio::time::timeout(Duration::from_secs(5), socket.read(&mut buffer))
        .await
        .expect("the upstream echo arrives")
        .expect("readable");
    assert_ne!(read, 0, "the bridged connection closed early");
    assert_eq!(
        ws::decode_text(&buffer[..read]).as_deref(),
        Some("echo:hello")
    );
}

#[tokio::test]
async fn a_websocket_upgrade_to_an_unrouted_path_is_refused_without_upgrading() {
    let mock = MockUpstream::start().await;
    let harness = Harness::new();
    let ctx = context_for(Uuid::new_v4());
    wire(&harness, &ctx, &mock).await;

    let mut request = empty_request(http::Method::GET, "/oagw/v1/proxy/mock-upstream/v2/ws");
    request.headers_mut().insert(
        http::header::CONNECTION,
        http::HeaderValue::from_static("Upgrade"),
    );
    request.headers_mut().insert(
        http::header::UPGRADE,
        http::HeaderValue::from_static("websocket"),
    );
    let response = harness.send(&ctx, request).await;
    assert_eq!(response.status(), 404);
    assert_eq!(response.headers()["x-oagw-error-source"], "gateway");
}

#[tokio::test]
async fn the_upgrade_handshake_headers_reach_the_upstream_despite_the_default_passthrough() {
    // `passthrough` defaults to `none`, but the handshake headers *are* the
    // protocol negotiation, so they must travel regardless.
    let mock = MockUpstream::start().await;
    let harness = Harness::new();
    let ctx = context_for(Uuid::new_v4());
    wire(&harness, &ctx, &mock).await;
    let gateway = harness.serve(&ctx).await;

    let mut socket = tokio::net::TcpStream::connect(gateway)
        .await
        .expect("connect");
    socket
        .write_all(
            concat!(
                // `/v1/echo` is the recording fallback, so the upstream reports
                // exactly which headers it received.
                "GET /oagw/v1/proxy/mock-upstream/v1/echo HTTP/1.1\r\n",
                "Host: gateway.local\r\n",
                "Connection: Upgrade\r\n",
                "Upgrade: websocket\r\n",
                "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n",
                "Sec-WebSocket-Version: 13\r\n",
                "\r\n"
            )
            .as_bytes(),
        )
        .await
        .expect("send");
    // Give the exchange a moment to reach the upstream.
    let mut buffer = [0u8; 4096];
    let _ = tokio::time::timeout(Duration::from_secs(5), socket.read(&mut buffer)).await;

    let seen = mock
        .requests()
        .into_iter()
        .find(|request| request.uri == "/v1/echo")
        .expect("the upstream saw the upgrade request");
    assert_eq!(seen.header("upgrade"), Some("websocket"));
    assert_eq!(seen.header("sec-websocket-version"), Some("13"));
    assert_eq!(
        seen.header("sec-websocket-key"),
        Some("dGhlIHNhbXBsZSBub25jZQ==")
    );
}
