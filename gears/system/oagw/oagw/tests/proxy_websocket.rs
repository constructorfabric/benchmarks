//! Protocol upgrades end to end.
//!
//! These tests bind a real listener rather than driving the router in-process:
//! an upgrade only exists once hyper owns the connection, so `Router::oneshot`
//! cannot exercise it.

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use common::{Fixture, MockBehavior, MockUpstream};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Serve `fixture`'s router on an ephemeral port and return its address.
async fn serve(fixture: &Fixture) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind gateway");
    let addr = listener.local_addr().expect("gateway addr");
    let router = fixture.router();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router.into_make_service()).await;
    });
    addr
}

/// Read from `stream` until the end of an HTTP head, returning
/// `(head, leftover)`.
async fn read_head(stream: &mut TcpStream) -> (String, Vec<u8>) {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let read = stream.read(&mut chunk).await.expect("read head");
        assert_ne!(read, 0, "the gateway closed before answering");
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            let end = position + 4;
            return (
                String::from_utf8_lossy(&buffer[..end]).into_owned(),
                buffer[end..].to_vec(),
            );
        }
    }
}

/// Send an upgrade request for `path` and return the connection plus the
/// response head.
async fn open_upgrade(addr: SocketAddr, path: &str) -> (TcpStream, String, Vec<u8>) {
    let mut stream = TcpStream::connect(addr).await.expect("connect gateway");
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write upgrade request");
    let (head, leftover) = read_head(&mut stream).await;
    (stream, head, leftover)
}

async fn wire(fixture: &Fixture, alias: &str, port: u16) {
    fixture.wire_upstream(alias, port, json!(["GET"])).await;
}

#[tokio::test]
async fn an_upgrade_is_relayed_and_the_connection_spliced_both_ways() {
    let upstream = MockUpstream::start(MockBehavior::WebSocketEcho).await;
    let fixture = Fixture::new();
    wire(&fixture, "ws", upstream.port()).await;
    let addr = serve(&fixture).await;

    let (mut stream, head, leftover) = open_upgrade(addr, "/oagw/v1/proxy/ws/socket").await;

    assert!(head.starts_with("HTTP/1.1 101 "), "{head}");
    let lower = head.to_ascii_lowercase();
    assert!(lower.contains("upgrade: websocket"), "{head}");
    assert!(lower.contains("connection: upgrade"), "{head}");
    // The upstream's handshake answer is relayed, not synthesized.
    assert!(lower.contains("sec-websocket-accept:"), "{head}");
    assert!(leftover.is_empty(), "no payload should precede the frames");

    // Bytes flow in both directions without the gateway interpreting them.
    stream.write_all(b"opaque-frame").await.expect("write frame");
    stream.flush().await.expect("flush");

    let mut echoed = vec![0_u8; 12];
    tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut echoed))
        .await
        .expect("the echo should arrive")
        .expect("read echo");
    assert_eq!(&echoed, b"opaque-frame");

    let seen = upstream.last_request().await;
    assert_eq!(seen.request_line(), "GET /socket HTTP/1.1");
    // The handshake headers are hop-by-hop, but an upgrade is exactly what is
    // being relayed, so they must survive.
    assert_eq!(
        seen.header("sec-websocket-key").as_deref(),
        Some("dGhlIHNhbXBsZSBub25jZQ==")
    );
    assert_eq!(seen.header("upgrade").as_deref(), Some("websocket"));
    assert!(
        seen.header("connection")
            .is_some_and(|value| value.to_ascii_lowercase().contains("upgrade"))
    );
}

#[tokio::test]
async fn a_refused_upgrade_is_relayed_as_an_ordinary_response() {
    let upstream = MockUpstream::start(MockBehavior::WebSocketRefused).await;
    let fixture = Fixture::new();
    wire(&fixture, "ws", upstream.port()).await;
    let addr = serve(&fixture).await;

    let (_stream, head, leftover) = open_upgrade(addr, "/oagw/v1/proxy/ws/socket").await;

    assert!(head.starts_with("HTTP/1.1 426 "), "{head}");
    assert!(
        head.to_ascii_lowercase().contains("x-oagw-error-source: upstream"),
        "{head}"
    );
    let body = String::from_utf8_lossy(&leftover);
    assert!(body.contains("upgrade refused"), "{body}");
}

#[tokio::test]
async fn an_upgrade_to_an_unknown_alias_is_a_gateway_error() {
    let fixture = Fixture::new();
    let addr = serve(&fixture).await;

    let (_stream, head, leftover) = open_upgrade(addr, "/oagw/v1/proxy/nope/socket").await;

    assert!(head.starts_with("HTTP/1.1 404 "), "{head}");
    assert!(
        head.to_ascii_lowercase().contains("x-oagw-error-source: gateway"),
        "{head}"
    );
    let body = String::from_utf8_lossy(&leftover);
    assert!(body.contains("cf.oagw.route.not_found.v1"), "{body}");
}

#[tokio::test]
async fn an_ordinary_request_still_works_on_the_same_listener() {
    // Guards against an upgrade path that accidentally captures every request.
    let upstream = MockUpstream::start(MockBehavior::Fixed {
        status: 200,
        content_type: "application/json",
        body: r#"{"ok":true}"#.to_owned(),
    })
    .await;
    let fixture = Fixture::new();
    wire(&fixture, "ws", upstream.port()).await;
    let addr = serve(&fixture).await;

    let mut stream = TcpStream::connect(addr).await.expect("connect gateway");
    stream
        .write_all(format!("GET /oagw/v1/proxy/ws/plain HTTP/1.1\r\nHost: {addr}\r\n\r\n").as_bytes())
        .await
        .expect("write request");
    let (head, _) = read_head(&mut stream).await;
    assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
}
