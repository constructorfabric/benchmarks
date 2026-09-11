//! Router-level tests for streaming: server-sent events and WebSocket.
//!
//! The proxy must not buffer a stream to forward it. Both tests here observe
//! that the bytes arrive as the upstream produces them.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use futures_util::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use axum::http::StatusCode;
use common::{Harness, JsonConfig, record, request, request_with};

/// A gateway whose single upstream serves `/sse` and `/ws`.
async fn gateway() -> Harness {
    let (harness, _) = Harness::build(&JsonConfig::new(true, 1024 * 1024), Vec::new()).await;
    harness.simple_upstream("stream", None).await;
    harness
}

#[tokio::test]
async fn an_event_stream_is_forwarded_in_full() {
    let harness = gateway().await;
    let response = record(
        harness
            .serve(request("GET", "/oagw/v1/proxy/stream/sse", None))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        response.header("content-type"),
        Some("text/event-stream"),
        "the upstream's content type is preserved"
    );
    assert_eq!(response.header("x-upstream-mark"), Some("streamed"));
    assert_eq!(
        response.source(),
        Some("upstream"),
        "a forwarded stream is the upstream's response, not the gateway's"
    );
    assert!(
        response.raw.contains("event: delta") && response.raw.contains("event: done"),
        "both the deltas and the terminating event are forwarded: {:?}",
        response.raw
    );
}

#[tokio::test]
async fn an_event_stream_arrives_before_the_upstream_finishes() {
    let harness = gateway().await;
    let response = harness
        .serve(request("GET", "/oagw/v1/proxy/stream/sse", None))
        .await;
    assert_eq!(response.status(), StatusCode::OK);

    let mut stream = http_body_util::BodyStream::new(response.into_body());

    // The first event must be readable on its own; a buffering proxy would
    // return nothing until the upstream closed the stream.
    let first = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
        .await
        .expect("the first chunk arrives without waiting for the stream to end")
        .expect("the stream yields a first chunk");
    let first = match first {
        Ok(chunk) => match chunk.into_data() {
            Ok(data) => String::from_utf8_lossy(&data).into_owned(),
            Err(frame) => panic!("the stream yields data, not a frame: {frame:?}"),
        },
        Err(error) => panic!("the stream yields a frame, not an error: {error}"),
    };
    assert!(first.contains("event: delta"), "the first chunk is the first event: {first}");
}

#[tokio::test]
async fn a_websocket_upgrade_is_tunneled() {
    let harness = gateway().await;
    let request = request_with(
        "GET",
        "/oagw/v1/proxy/stream/ws",
        &[
            ("host", "127.0.0.1"),
            ("connection", "Upgrade"),
            ("upgrade", "websocket"),
            ("sec-websocket-version", "13"),
            ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
        ],
    );
    let response = record(harness.serve(request).await).await;
    assert_eq!(
        response.status,
        StatusCode::SWITCHING_PROTOCOLS,
        "the upgrade is negotiated, not refused: {}",
        response.raw
    );
    assert_eq!(
        response.header("sec-websocket-accept"),
        Some("s3pPLMBiTxaQ9kYGzzhZRbK+xOo="),
        "the accept key is derived as the WebSocket handshake requires"
    );
    assert_eq!(
        response.source(),
        Some(common::UPSTREAM_SOURCE),
        "a streamed protocol carries the error-source header too"
    );
}

#[tokio::test]
async fn a_websocket_upgrade_without_the_handshake_headers_is_not_tunneled() {
    let harness = gateway().await;
    let response = record(
        harness
            .serve(request("GET", "/oagw/v1/proxy/stream/ws", None))
            .await,
    )
    .await;
    // Without `upgrade: websocket` the upstream answers with a plain HTTP
    // response rather than switching protocols; either way the gateway does not
    // pretend to tunnel.
    assert_ne!(response.status, StatusCode::SWITCHING_PROTOCOLS);
}

#[tokio::test]
async fn the_end_of_the_upstream_stream_ends_the_forwarded_response() {
    let harness = gateway().await;
    let response = harness
        .serve(request("GET", "/oagw/v1/proxy/stream/sse", None))
        .await;
    assert_eq!(response.status(), StatusCode::OK);

    let mut stream = http_body_util::BodyStream::new(response.into_body());

    let mut saw_done = false;
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(5), stream.next()).await {
            Err(elapsed) => panic!("the stream did not end on its own: {elapsed}"),
            Ok(None) => break,
            Ok(Some(Ok(frame))) => {
                let Ok(data) = frame.into_data() else { break };
                saw_done |= String::from_utf8_lossy(&data).contains("event: done");
            }
            Ok(Some(Err(error))) => panic!("the stream errored rather than ending: {error}"),
        }
    }
    assert!(
        saw_done,
        "the terminating event was forwarded and the body ended when the upstream closed it"
    );
}

/// A WebSocket client speaking just enough of RFC 6455 to prove frames cross
/// the gateway in both directions.
struct Client {
    socket: tokio::net::TcpStream,
}

/// The mask a client frame carries: the value is irrelevant, its presence is
/// not, since RFC 6455 requires every client frame to be masked.
const MASK: [u8; 4] = [0x11, 0x22, 0x33, 0x44];

impl Client {
    /// Dials the gateway and completes the opening handshake.
    ///
    /// # Panics
    /// Panics when the gateway refuses the upgrade.
    async fn connect(address: std::net::SocketAddr, path: &str) -> Self {
        let mut socket =
            tokio::net::TcpStream::connect(address).await.expect("the gateway is listening");
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: {address}\r\nUpgrade: websocket\r\n\
             Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n"
        );
        socket.write_all(request.as_bytes()).await.expect("handshake is written");
        let head = read_head(&mut socket).await;
        assert!(
            head.starts_with("HTTP/1.1 101"),
            "the upgrade is negotiated, not refused: {head}"
        );
        assert!(
            head.contains("sec-websocket-accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo="),
            "the accept key is derived by the upstream, not invented by the gateway: {head}"
        );
        Self { socket }
    }

    /// Sends one masked text frame.
    ///
    /// # Panics
    /// Panics when the frame cannot be written.
    async fn send_text(&mut self, text: &str) {
        let payload = text.as_bytes();
        let mut frame = vec![0x81, u8::try_from(payload.len()).expect("a short frame") | 0x80];
        frame.extend_from_slice(&MASK);
        frame.extend(
            payload
                .iter()
                .zip(MASK.iter().cycle())
                .map(|(byte, mask)| byte ^ mask),
        );
        self.socket.write_all(&frame).await.expect("the frame is written");
        self.socket.flush().await.expect("the frame is flushed");
    }

    /// Reads one frame and returns its payload, insisting it is text.
    ///
    /// # Panics
    /// Panics when the frame is not text.
    async fn recv_text(&mut self) -> String {
        let mut head = [0_u8; 2];
        self.socket.read_exact(&mut head).await.expect("a frame header arrives");
        assert_eq!(head[0] & 0x0f, 1, "the echo is a text frame, not {:#x}", head[0]);
        let length = usize::from(head[1] & 0x7f);
        let mut payload = vec![0_u8; length];
        self.socket.read_exact(&mut payload).await.expect("a frame body arrives");
        String::from_utf8(payload).expect("the echo is utf-8")
    }

    /// Sends a close frame.
    ///
    /// # Panics
    /// Panics when the frame cannot be written.
    async fn send_close(&mut self) {
        let mut frame = vec![0x88, 0x80];
        frame.extend_from_slice(&MASK);
        self.socket.write_all(&frame).await.expect("the close is written");
        self.socket.flush().await.expect("the close is flushed");
    }

    /// Reads until the peer hangs up, expecting no further frames.
    ///
    /// # Panics
    /// Panics when the tunnel keeps the connection open past the close.
    async fn assert_ends_after_close(&mut self) {
        let mut trailing = Vec::new();
        loop {
            let mut chunk = [0_u8; 256];
            match self.socket.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(read) => trailing.extend_from_slice(&chunk[..read]),
            }
        }
        // A peer that answers the close with its own close frame is correct;
        // anything else still open is not.
        assert!(
            trailing.iter().all(|byte| *byte == 0x88 || *byte == 0),
            "nothing but a close frame follows the close: {trailing:?}"
        );
    }
}

/// Reads an HTTP response head, byte by byte, so the socket keeps the rest.
///
/// # Panics
/// Panics when the response head cannot be read.
async fn read_head(socket: &mut tokio::net::TcpStream) -> String {
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut byte = [0_u8; 1];
        let read = socket.read(&mut byte).await.expect("the response head is readable");
        assert!(read > 0, "the connection closed before the head ended");
        head.push(byte[0]);
    }
    String::from_utf8_lossy(&head).into_owned()
}

#[tokio::test]
async fn websocket_frames_cross_the_gateway_in_both_directions() {
    let harness = gateway().await;
    let address = harness.serve_tcp().await;
    let mut client = Client::connect(address, "/oagw/v1/proxy/stream/ws").await;

    // One frame each way, then a second pair on the same tunnel: the pipe has
    // to stay open and keep its direction, not hand one message across.
    for round in ["client to upstream", "and back again"] {
        client.send_text(round).await;
        let echo = client.recv_text().await;
        assert_eq!(echo, round, "the upstream's echo comes back over the tunnel");
    }

    client.send_close().await;
    client.assert_ends_after_close().await;
}

#[tokio::test]
async fn a_client_that_stops_reading_does_not_hang_the_gateway() {
    let harness = gateway().await;
    let response = harness
        .serve(request("GET", "/oagw/v1/proxy/stream/sse/long", None))
        .await;
    assert_eq!(response.status(), StatusCode::OK);

    let mut stream = http_body_util::BodyStream::new(response.into_body());

    let first = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
        .await
        .expect("the first chunk arrives")
        .expect("the stream yields a first chunk")
        .expect("the first chunk is data");
    assert!(first.into_data().is_ok());

    // Dropping the body is a client disconnect: the gateway must stop reading
    // the upstream rather than keep pumping a stream nobody is consuming. The
    // upstream counts its live generators, so the release is observable.
    drop(stream);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        assert!(
            std::time::Instant::now() < deadline,
            "the upstream generator is never released"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let response = record(
            harness
                .serve(request("GET", "/oagw/v1/proxy/stream/sse/active", None))
                .await,
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.raw);
        if response.body["active"].as_u64() == Some(0) {
            break;
        }
    }
}
