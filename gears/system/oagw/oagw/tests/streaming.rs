//! Streaming coverage (PRD `cpt-cf-oagw-fr-streaming`, DESIGN "Proxy
//! Endpoint"): an `text/event-stream` body is relayed chunk-by-chunk as the
//! upstream produces it, and a WebSocket handshake is relayed and then
//! tunnelled byte-for-byte.
//!
//! Both sides are real sockets. `tower::ServiceExt::oneshot` cannot carry a
//! protocol upgrade, so the WebSocket test dials the gateway over a real
//! listener ([`common::served`]) and the upstream is a hand-rolled HTTP/1.1
//! peer, which also gives exact control over chunk boundaries.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::{Duration, Instant};

use axum::http::{Method, StatusCode};
use http_body_util::BodyExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::common::{Harness, catch_all_route, create_upstream};

/// The RFC 6455 §4.2.2 example key and the accept value it must produce.
const WS_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";
/// `base64(sha1(key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"))` (RFC 6455 §1.3).
const WS_ACCEPT: &str = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";

/// Register an enabled catch-all upstream + route for `port` under `alias`.
async fn route_to(h: &Harness, port: u16, alias: &str) -> String {
    let (status, body) =
        create_upstream(h, common::http_upstream("127.0.0.1", port, Some(alias))).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let upstream_id = body["id"].as_str().expect("upstream id").to_owned();
    let (status, body) = common::create_route(h, catch_all_route(&upstream_id, "/")).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    alias.to_owned()
}

/// Read from `stream` until a blank line terminates the header block.
async fn read_head(stream: &mut TcpStream) -> String {
    let mut buffer = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let read = stream.read(&mut byte).await.expect("read head");
        assert!(read > 0, "the peer closed before sending a full head");
        buffer.push(byte[0]);
        if buffer.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8(buffer).expect("utf-8 head")
}

/// Read an HTTP response head (terminated by `\r\n\r\n`) from the socket.
async fn read_response_head(stream: &mut TcpStream) -> String {
    read_head(stream).await
}

// ------------------------------------------------------------------- SSE

/// An upstream that streams two SSE events, 400 ms apart, over chunked framing.
///
/// Returns `(port, is_done)`; `is_done` resolves once the second event has been
/// written, so the test can prove it read the first one before that.
async fn sse_upstream() -> (u16, tokio::sync::oneshot::Receiver<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind sse upstream");
    let port = listener.local_addr().expect("sse local addr").port();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept sse client");
        let _head = read_head(&mut socket).await;
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                  cache-control: no-cache\r\ntransfer-encoding: chunked\r\n\r\n",
            )
            .await
            .expect("write sse head");

        let write_event = |text: &str| -> Vec<u8> {
            let payload = format!("data: {text}\n\n");
            let mut frame = format!("{:x}\r\n", payload.len()).into_bytes();
            frame.extend_from_slice(payload.as_bytes());
            frame.extend_from_slice(b"\r\n");
            frame
        };

        socket
            .write_all(&write_event("one"))
            .await
            .expect("write one");
        socket.flush().await.expect("flush one");

        // The second event is deliberately late: a buffering gateway would
        // deliver both frames at once, 400 ms after the first.
        tokio::time::sleep(Duration::from_millis(400)).await;
        socket
            .write_all(&write_event("two"))
            .await
            .expect("write two");
        socket
            .write_all(b"0\r\n\r\n")
            .await
            .expect("write terminal");
        socket.flush().await.expect("flush two");
        let _ = done_tx.send(());
        let mut drain = [0u8; 512];
        while socket.read(&mut drain).await.unwrap_or(0) > 0 {}
    });
    (port, done_rx)
}

/// Poll the next body frame, returning its bytes.
async fn next_frame(body: &mut axum::body::Body) -> Vec<u8> {
    let frame = body
        .frame()
        .await
        .expect("a body frame")
        .expect("a body frame result");
    frame
        .into_data()
        .map(|bytes| bytes.to_vec())
        .unwrap_or_default()
}

#[tokio::test]
async fn an_sse_body_is_relayed_incrementally_not_buffered() {
    let (port, done) = sse_upstream().await;
    let h = common::harness();
    let alias = route_to(&h, port, "events").await;

    let response = common::proxy(
        &h,
        Method::GET,
        &alias,
        "/v1/stream",
        &[("accept", "text/event-stream")],
        None,
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{:?}",
        response.headers()
    );
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream"),
        "the upstream content type survives the hop"
    );

    let mut body = response.into_body();
    let started = Instant::now();
    let first = next_frame(&mut body).await;
    let first_at = started.elapsed();
    assert_eq!(
        String::from_utf8(first.clone()).expect("utf-8 frame"),
        "data: one\n\n",
        "the first event arrives alone"
    );

    let second = next_frame(&mut body).await;
    let second_at = started.elapsed();
    assert_eq!(
        String::from_utf8(second).expect("utf-8 frame"),
        "data: two\n\n",
        "the second event follows"
    );
    // The gateway is a pipe: the second frame is only available after the
    // upstream produced it, i.e. it was not buffered and replayed at once.
    let gap = second_at.saturating_sub(first_at);
    assert!(
        gap >= Duration::from_millis(200),
        "the frames must be spaced by the upstream's own pacing, got {gap:?}"
    );
    assert!(
        try_frame(&mut body).await.is_none(),
        "the body ends after the upstream's terminal chunk"
    );
    assert!(done.await.is_ok(), "the upstream finished its exchange");
}

/// The next body frame's bytes, or `None` at end of stream.
async fn try_frame(body: &mut axum::body::Body) -> Option<Vec<u8>> {
    let frame = body.frame().await?;
    Some(
        frame
            .expect("a body frame result")
            .into_data()
            .map(|bytes| bytes.to_vec())
            .unwrap_or_default(),
    )
}

#[tokio::test]
async fn an_sse_exchange_is_relayed_verbatim_end_to_end() {
    let (port, _) = sse_upstream().await;
    let h = common::harness();
    let alias = route_to(&h, port, "all").await;

    let response = common::proxy(&h, Method::GET, &alias, "/v1/events", &[], None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let text = common::body_string(response.into_body()).await;
    assert_eq!(
        text, "data: one\n\ndata: two\n\n",
        "the relayed bytes are exactly the bytes the upstream wrote"
    );
}

// ------------------------------------------------------------------- websocket

/// A WebSocket upstream: answers the handshake with `101`, then echoes frames.
///
/// The accept value is the RFC 6455 §1.3 one, because the test always dials
/// with [`WS_KEY`].
async fn websocket_upstream() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ws upstream");
    let port = listener.local_addr().expect("ws local addr").port();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept ws client");
        let head = read_head(&mut socket).await;
        let lower = head.to_ascii_lowercase();
        assert!(
            lower.contains("upgrade: websocket"),
            "the gateway must forward the `Upgrade` header: {head}"
        );
        assert!(
            lower.contains("connection: upgrade"),
            "the gateway must forward the `Connection` token: {head}"
        );
        assert!(
            lower.contains(&format!(
                "sec-websocket-key: {}",
                WS_KEY.to_ascii_lowercase()
            )),
            "the gateway must forward the client's `Sec-WebSocket-Key`: {head}"
        );

        let accept = format!(
            "HTTP/1.1 101 Switching Protocols\r\n\
             upgrade: websocket\r\n\
             connection: Upgrade\r\n\
             sec-websocket-accept: {WS_ACCEPT}\r\n\r\n"
        );
        socket
            .write_all(accept.as_bytes())
            .await
            .expect("write 101");
        socket.flush().await.expect("flush 101");

        // Echo every text frame back, server-side (unmasked).
        let mut buffer = [0u8; 4096];
        loop {
            let read = match socket.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(read) => read,
            };
            let payload = decode_frame(&buffer[..read]);
            let Some(payload) = payload else { continue };
            // `decode_frame` unmasks, so what is echoed back is the *decoded*
            // payload: the tunnel has to have carried it through untouched.
            let _ = socket.write_all(&server_frame(&payload)).await;
            let _ = socket.flush().await;
        }
    });
    port
}

/// A masked client→server text frame carrying `payload`.
fn client_frame(payload: &[u8]) -> Vec<u8> {
    let mask = [0x2au8, 0x5bu8, 0x1fu8, 0x7du8];
    let mut frame = vec![0x81u8];
    if payload.len() < 126 {
        frame.push(0x80 | payload.len() as u8);
    } else {
        frame.push(0x80 | 126);
        frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    }
    frame.extend_from_slice(&mask);
    for (index, byte) in payload.iter().enumerate() {
        frame.push(byte ^ mask[index % 4]);
    }
    frame
}

/// An unmasked server→client text frame carrying `payload`.
fn server_frame(payload: &[u8]) -> Vec<u8> {
    let mut frame = vec![0x81u8];
    if payload.len() < 126 {
        frame.push(payload.len() as u8);
    } else {
        frame.push(126);
        frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    }
    frame.extend_from_slice(payload);
    frame
}

/// The payload of the first complete frame in `bytes`, unmasked.
fn decode_frame(bytes: &[u8]) -> Option<Vec<u8>> {
    if bytes.len() < 2 {
        return None;
    }
    let masked = bytes[1] & 0x80 != 0;
    let mut length = usize::from(bytes[1] & 0x7f);
    let mut offset = 2;
    if length == 126 {
        if bytes.len() < 4 {
            return None;
        }
        length = u16::from_be_bytes([bytes[2], bytes[3]]).into();
        offset = 4;
    }
    let mask_len = usize::from(masked) * 4;
    let end = offset + mask_len + length;
    if bytes.len() < end {
        return None;
    }
    let payload = bytes[offset + mask_len..end].to_vec();
    if masked {
        return Some(unmask(&payload, &bytes[offset..offset + 4]));
    }
    Some(payload)
}

/// Apply a four-byte masking key (RFC 6455 §5.3).
fn unmask(payload: &[u8], mask: &[u8]) -> Vec<u8> {
    payload
        .iter()
        .enumerate()
        .map(|(index, byte)| byte ^ mask[index % 4])
        .collect()
}

#[tokio::test]
async fn a_websocket_handshake_is_relayed_and_the_tunnel_is_bidirectional() {
    let port = websocket_upstream().await;
    let h = common::harness();
    let alias = route_to(&h, port, "ws").await;
    let gateway = common::served(&h).await;

    let mut client = tokio::net::TcpStream::connect(gateway)
        .await
        .expect("dial the gateway");
    let request = format!(
        "GET /oagw/v1/proxy/{alias}/ws HTTP/1.1\r\n\
         host: {gateway}\r\n\
         upgrade: websocket\r\n\
         connection: Upgrade\r\n\
         sec-websocket-key: {WS_KEY}\r\n\
         sec-websocket-version: 13\r\n\r\n"
    );
    client
        .write_all(request.as_bytes())
        .await
        .expect("handshake");
    client.flush().await.expect("flush handshake");

    let head = read_response_head(&mut client).await;
    assert!(
        head.starts_with("HTTP/1.1 101"),
        "the gateway relays the upstream's 101, got: {head}"
    );
    assert!(
        head.to_ascii_lowercase().contains(&format!(
            "sec-websocket-accept: {}",
            WS_ACCEPT.to_ascii_lowercase()
        )),
        "the accept header is the RFC 6455 value for the client key: {head}"
    );

    // Frames flow both ways without the gateway interpreting them.
    let payload = b"hello gateway";
    let outbound = client_frame(payload);
    client.write_all(&outbound).await.expect("write frame");
    client.flush().await.expect("flush frame");

    let expected = 2 + payload.len();
    let mut buffer = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while buffer.len() < expected && Instant::now() < deadline {
        let mut chunk = [0u8; 256];
        let read = tokio::time::timeout_at(deadline.into(), client.read(&mut chunk))
            .await
            .expect("read within the deadline")
            .expect("read echoed frame");
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk);
    }
    let echoed = decode_frame(&buffer).expect("a complete echoed frame");
    assert_eq!(
        String::from_utf8(echoed).expect("utf-8 echo"),
        "hello gateway",
        "the tunnel carries the payload unmodified in both directions"
    );
}

#[tokio::test]
async fn a_refused_handshake_is_relayed_as_an_ordinary_response() {
    // An upstream that declines the handshake with a plain 404.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind refusing upstream");
    let port = listener.local_addr().expect("refusing local addr").port();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let _ = read_head(&mut socket).await;
        socket
            .write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n")
            .await
            .expect("write 404");
        socket.flush().await.expect("flush 404");
    });

    let h = common::harness();
    let alias = route_to(&h, port, "refused").await;
    let gateway = common::served(&h).await;

    let mut client = tokio::net::TcpStream::connect(gateway)
        .await
        .expect("dial the gateway");
    let request = format!(
        "GET /oagw/v1/proxy/{alias}/ws HTTP/1.1\r\n\
         host: {gateway}\r\n\
         upgrade: websocket\r\n\
         connection: Upgrade\r\n\
         sec-websocket-key: {WS_KEY}\r\n\
         sec-websocket-version: 13\r\n\r\n"
    );
    client
        .write_all(request.as_bytes())
        .await
        .expect("handshake");
    let head = read_response_head(&mut client).await;
    assert!(
        head.starts_with("HTTP/1.1 404"),
        "a refused handshake is relayed verbatim, got: {head}"
    );
}
