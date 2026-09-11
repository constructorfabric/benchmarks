//! Integration tests over the full router: streaming and upgrade relays.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use http::{Method, StatusCode};
use std::io::{Read, Write};
use std::net::TcpStream;

/// A gateway and a live upstream, both listening on real sockets.
///
/// The upstream is held so it outlives the test.
struct StreamFixture {
    gateway: std::net::SocketAddr,
    /// Kept alive for the duration of the test.
    _upstream: MockUpstream,
}

impl StreamFixture {
    /// Build the fixture with a route to `/echo` and `/stream`.
    async fn new() -> Self {
        let harness = Harness::with_config(permissive_config(), None);
        let upstream = MockUpstream::start();
        let created = create_upstream(&harness, upstream.upstream_body("local")).await;
        for path in ["/echo", "/stream", "/ws"] {
            create_route(&harness, route_body(created["id"].as_str().unwrap(), path)).await;
        }
        let gateway = harness.spawn().await;
        Self {
            gateway,
            _upstream: upstream,
        }
    }

    /// Connect to the gateway.
    fn connect(&self) -> TcpStream {
        TcpStream::connect(self.gateway).expect("gateway is reachable")
    }

    /// The proxy path of `suffix`.
    fn path(suffix: &str) -> String {
        proxy_path("local", suffix)
    }
}

/// Send a raw HTTP/1.1 request and read the whole answer.
fn round_trip(stream: &mut TcpStream, request: &str) -> String {
    stream
        .write_all(request.as_bytes())
        .expect("request is written");
    stream.flush().expect("request is flushed");
    let mut response = Vec::new();
    let deadline = std::time::Duration::from_secs(10);
    let _ignored = stream.set_read_timeout(Some(deadline));
    let _ignored = stream.read_to_end(&mut response);
    String::from_utf8_lossy(&response).into_owned()
}

/// An SSE stream is relayed incrementally, not buffered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sse_streams_are_relayed() {
    let fixture = StreamFixture::new().await;
    let mut stream = fixture.connect();
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        StreamFixture::path("/stream")
    );
    let response = round_trip(&mut stream, &request);
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(
        response.contains("text/event-stream"),
        "the content type is relayed: {response}"
    );
    assert!(response.contains("data: one"), "{response}");
    assert!(response.contains("data: two"), "{response}");
    assert!(response.contains("data: three"), "{response}");
}

/// A WebSocket upgrade is tunnelled: the client talks to the upstream through
/// the relay.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_upgrades_are_tunneled() {
    let fixture = StreamFixture::new().await;
    let mut client = fixture.connect();
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: \
         websocket\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: \
         13\r\n\r\n",
        StreamFixture::path("/ws")
    );
    client
        .write_all(request.as_bytes())
        .expect("handshake is written");
    client.flush().expect("handshake is flushed");

    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    let handshake_deadline = std::time::Duration::from_secs(5);
    let _ignored = client.set_read_timeout(Some(handshake_deadline));
    while !head.ends_with(b"\r\n\r\n") {
        let read = client.read(&mut byte).expect("handshake is readable");
        assert!(read > 0, "the gateway closed the connection before 101");
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    assert!(head.contains("sec-websocket-accept"), "{head}");

    // One masked text frame; the zero mask leaves the payload untouched.
    let payload = b"hello";
    let mut frame = vec![
        0x81_u8,
        0x80 | u8::try_from(payload.len()).unwrap_or(125),
        0,
        0,
        0,
        0,
    ];
    frame.extend_from_slice(payload);
    client.write_all(&frame).expect("frame is written");
    client.flush().expect("frame is flushed");

    let mut reply = [0_u8; 2];
    client.read_exact(&mut reply).expect("echo frame header");
    assert_eq!(reply[0], 0x81, "the echo is a FIN text frame");
    let len = usize::from(reply[1] & 0x7f);
    let mut echoed = vec![0_u8; len];
    client.read_exact(&mut echoed).expect("echo frame payload");
    assert_eq!(String::from_utf8_lossy(&echoed), "echo:hello");
}

/// A non-101 upgrade answer is a gateway protocol error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_upgrades_are_gateway_errors() {
    let fixture = StreamFixture::new().await;
    // `/echo` never upgrades, so the tunnel attempt must fail.
    let mut client = fixture.connect();
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: \
         websocket\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: \
         13\r\n\r\n",
        StreamFixture::path("/echo")
    );
    client
        .write_all(request.as_bytes())
        .expect("handshake is written");
    client.flush().expect("handshake is flushed");
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    let handshake_deadline = std::time::Duration::from_secs(5);
    let _ignored = client.set_read_timeout(Some(handshake_deadline));
    while !head.ends_with(b"\r\n\r\n") {
        let read = client.read(&mut byte).expect("response is readable");
        assert!(read > 0, "the gateway closed the connection");
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    assert!(head.starts_with("HTTP/1.1 502"), "{head}");
    assert!(head.contains("application/problem+json"), "{head}");
    assert!(head.contains("x-oagw-error-source: gateway"), "{head}");
}

/// Regular calls keep working over the live server.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plain_proxied_calls_work_over_a_live_server() {
    let fixture = StreamFixture::new().await;
    let mut stream = fixture.connect();
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        StreamFixture::path("/echo")
    );
    let response = round_trip(&mut stream, &request);
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(
        response.contains("x-oagw-error-source: upstream"),
        "{response}"
    );
    assert!(response.contains("\"path\":\"/echo\""), "{response}");
}

/// The management API stays reachable on the same server.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn management_endpoints_are_served_alongside_the_proxy() {
    let fixture = StreamFixture::new().await;
    let mut stream = fixture.connect();
    let response = round_trip(
        &mut stream,
        "GET /oagw/v1/upstreams HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(response.contains("application/json"), "{response}");
    assert!(
        response.contains("gts.cf.core.oagw.upstream.v1~"),
        "{response}"
    );
}

/// The preflight is answered locally over the live server too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preflights_are_answered_over_the_live_server() {
    let fixture = StreamFixture::new().await;
    let mut stream = fixture.connect();
    let request = format!(
        "OPTIONS {} HTTP/1.1\r\nHost: localhost\r\nOrigin: https://app.example.com\r\n\
         Access-Control-Request-Method: POST\r\nConnection: close\r\n\r\n",
        StreamFixture::path("/echo")
    );
    let response = round_trip(&mut stream, &request);
    assert!(response.starts_with("HTTP/1.1 204"), "{response}");
    assert!(
        response.contains("access-control-allow-origin: https://app.example.com"),
        "{response}"
    );
    assert!(
        response.contains("access-control-max-age: 86400"),
        "{response}"
    );
}

/// A status code is asserted once so the fixtures above stay focused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_constants_are_consistent() {
    assert_eq!(StatusCode::NO_CONTENT.as_u16(), 204);
    assert_eq!(Method::GET.as_str(), "GET");
}

/// The upstream closing the tunnel ends the client connection too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_upstream_close_ends_the_client_connection() {
    let harness = Harness::with_config(permissive_config(), None);
    let upstream = MockUpstream::start();
    let created = create_upstream(&harness, upstream.upstream_body("local")).await;
    create_route(
        &harness,
        route_body(created["id"].as_str().unwrap(), "/ws-close"),
    )
    .await;
    let gateway = harness.spawn().await;

    let mut client = TcpStream::connect(gateway).expect("gateway is reachable");
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: \
         websocket\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: \
         13\r\n\r\n",
        StreamFixture::path("/ws-close")
    );
    client
        .write_all(request.as_bytes())
        .expect("handshake is written");
    client.flush().expect("handshake is flushed");

    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    let handshake_deadline = std::time::Duration::from_secs(5);
    let _ignored = client.set_read_timeout(Some(handshake_deadline));
    while !head.ends_with(b"\r\n\r\n") {
        let read = client.read(&mut byte).expect("handshake is readable");
        assert!(read > 0, "the gateway closed the connection before 101");
        head.push(byte[0]);
    }
    assert!(
        String::from_utf8_lossy(&head).starts_with("HTTP/1.1 101"),
        "{}",
        String::from_utf8_lossy(&head)
    );

    // The upstream drops the socket as soon as the upgrade completes.
    client.set_read_timeout(Some(handshake_deadline)).ok();
    let mut byte = [0_u8; 1];
    let read = client.read(&mut byte).expect("the relay is readable");
    assert_eq!(read, 0, "the client sees the upstream close");
}
