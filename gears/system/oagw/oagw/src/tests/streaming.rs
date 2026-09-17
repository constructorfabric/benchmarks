//! Streaming data-plane tests: the `text/event-stream` relay and the
//! WebSocket upgrade path.
//!
//! The buffered tests in [`crate::tests::data_plane`] drive the router with
//! `tower::ServiceExt::oneshot`, which cannot carry an upgraded connection, so
//! the WebSocket tests here bind a real listener on an ephemeral loopback port
//! and speak RFC 6455 to it by hand.

use std::time::{Duration, Instant};

use axum::http::Method;
use futures_util::StreamExt;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

use super::{
    ERROR_SOURCE_HEADER, Gateway, Origin, Reply, SOURCE_GATEWAY, SOURCE_UPSTREAM, body_text,
    post_json, route_json, send, status_of, upstream_json,
};
use crate::config::OagwConfig;

/// The tenant the streaming tests register their upstreams in.
fn tenant() -> Uuid {
    Uuid::from_u128(0xbeef)
}

/// Configuration that permits the plaintext loopback endpoints tests use.
fn relay_config() -> OagwConfig {
    OagwConfig {
        proxy_timeout_secs: 20,
        allow_http_upstream: true,
        ..OagwConfig::default()
    }
}

/// A gateway plus a JSON-echo origin registered under `alias` with a `path`
/// route for `methods`.
async fn relay_gateway(alias: &str, path: &str, methods: &[&str]) -> (Gateway, Origin) {
    let origin = Origin::spawn(Reply::Echo).await;
    let gateway = super::gateway_with(relay_config());
    let created = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant(),
        upstream_json(origin.host(), origin.port(), alias),
    )
    .await;
    assert_eq!(status_of(&created).as_u16(), 201, "upstream created");
    let route = post_json(
        &gateway,
        "/oagw/v1/routes",
        tenant(),
        route_json(first_upstream_id(&gateway), path, methods),
    )
    .await;
    assert_eq!(status_of(&route).as_u16(), 201, "route created");
    (gateway, origin)
}

/// The id of the single upstream the test registered.
fn first_upstream_id(gateway: &Gateway) -> Uuid {
    let upstreams = gateway.store.upstreams_of(tenant());
    assert_eq!(upstreams.len(), 1, "exactly one upstream is registered");
    upstreams[0].id
}

/// Send an anonymous proxied request.
async fn proxy(
    gateway: &Gateway,
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> axum::response::Response {
    send(
        &gateway.router,
        super::proxy_request(method, uri, headers, body, None),
    )
    .await
}

// ---------------------------------------------------------------------------
// Server-sent events
// ---------------------------------------------------------------------------

#[tokio::test]
async fn event_stream_frames_arrive_while_the_origin_is_still_writing() {
    let origin = Origin::spawn(Reply::Chunks {
        content_type: "text/event-stream",
        chunks: [
            b"data: first\n\n".as_slice(),
            b"data: second\n\n".as_slice(),
            b"data: third\n\n".as_slice(),
        ]
        .to_vec(),
        pause_ms: 1_500,
    })
    .await;
    let gateway = super::gateway_with(relay_config());
    let created = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant(),
        upstream_json(origin.host(), origin.port(), "events.local"),
    )
    .await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let started = Instant::now();
    let response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/events.local/v1/events",
        &[],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 200);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream"),
        "the upstream content type is preserved"
    );
    assert_eq!(
        response
            .headers()
            .get(ERROR_SOURCE_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some(SOURCE_UPSTREAM),
        "a relayed response is marked as coming from the upstream"
    );

    // The first frame must be visible before the origin has written its last
    // one: a gateway that buffered the whole body would only answer after
    // 2 x 1.5 s of pauses.
    let mut stream = response.into_body().into_data_stream();
    let first = stream.next().await.expect("the first frame arrives");
    let elapsed = started.elapsed();
    let first = String::from_utf8(first.expect("the first frame is readable").to_vec())
        .expect("the frame is UTF-8");
    assert_eq!(first, "data: first\n\n");
    assert!(
        elapsed < Duration::from_secs(1),
        "the first frame arrived after {elapsed:?}, so the response was buffered"
    );

    let mut relayed = first;
    while let Some(frame) = stream.next().await {
        relayed.push_str(core::str::from_utf8(&frame.expect("frame is readable")).expect("utf8"));
    }
    assert_eq!(
        relayed, "data: first\n\ndata: second\n\ndata: third\n\n",
        "every frame is relayed in order"
    );
}

#[tokio::test]
async fn streaming_content_types_are_relayed_incrementally() {
    for content_type in [
        "text/event-stream",
        "application/stream+json",
        "application/json-seq",
    ] {
        let origin = Origin::spawn(Reply::Fixed {
            status: 200,
            headers: vec![("content-type", content_type.to_owned())],
            body: b"chunk".to_vec(),
        })
        .await;
        let gateway = super::gateway_with(relay_config());
        let created = post_json(
            &gateway,
            "/oagw/v1/upstreams",
            tenant(),
            upstream_json(origin.host(), origin.port(), "stream-type.local"),
        )
        .await;
        assert_eq!(status_of(&created).as_u16(), 201);

        let mut response = proxy(
            &gateway,
            Method::GET,
            "/oagw/v1/proxy/stream-type.local/v1",
            &[],
            b"",
        )
        .await;
        assert_eq!(status_of(&response).as_u16(), 200, "{content_type}");
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some(content_type),
            "{content_type} is preserved"
        );
        assert_eq!(body_text(&mut response).await, "chunk", "{content_type}");
    }
}

/// Documented implementation behaviour: when the upstream drops the connection
/// mid-stream, the relayed body simply ends — the gateway does not synthesize
/// a problem document for a truncated stream.
#[tokio::test]
async fn a_stream_cut_midway_by_the_upstream_ends_the_body() {
    let origin = Origin::spawn(Reply::Cut {
        content_type: "text/event-stream",
        head: b"data: only\n\n",
    })
    .await;
    let gateway = super::gateway_with(relay_config());
    let created = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant(),
        upstream_json(origin.host(), origin.port(), "cut.local"),
    )
    .await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let mut response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/cut.local/v1/stream",
        &[],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 200);
    assert_eq!(
        body_text(&mut response).await,
        "data: only\n\n",
        "the frames written before the cut are relayed"
    );
}

// ---------------------------------------------------------------------------
// WebSocket upgrade
// ---------------------------------------------------------------------------
//
// `axum::serve` (rather than `ServiceExt::oneshot`) is used wherever the client
// side of the upgrade matters: a real connection is the only medium through
// which a protocol switch could ever be negotiated.

/// `axum::serve` on an ephemeral loopback port.
struct Served {
    address: std::net::SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Served {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Serve `router` on `127.0.0.1:0`.
async fn serve(router: axum::Router) -> Served {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("the loopback listener binds");
    let address = listener.local_addr().expect("the listener has an address");
    let task = tokio::spawn(async move {
        axum::serve(listener, router.into_make_service())
            .await
            .expect("the server serves");
    });
    Served { address, task }
}

/// An upstream that would accept an upgrade on `/v1/ws` and echo text frames
/// back: a genuine upgrade partner, so any failure below is the gateway's.
async fn websocket_upstream() -> Served {
    let app = axum::Router::new().route(
        "/v1/ws",
        axum::routing::any(|upgrade: axum::extract::ws::WebSocketUpgrade| async move {
            upgrade.on_upgrade(|mut socket| async move {
                use axum::extract::ws::Message;
                while let Some(Ok(message)) = socket.recv().await {
                    let reply = match message {
                        Message::Text(text) => Message::Text(format!("echo:{text}").into()),
                        Message::Close(_) => break,
                        other => other,
                    };
                    if socket.send(reply).await.is_err() {
                        break;
                    }
                }
            })
        }),
    );
    serve(app).await
}

/// An upstream that answers `404` on the upgrade path: it refuses the protocol
/// switch without ever negotiating one.
async fn refusing_upstream() -> Served {
    serve(axum::Router::new().route(
        "/v1/ws",
        axum::routing::get(|| async { (axum::http::StatusCode::NOT_FOUND, "not a socket") }),
    ))
    .await
}

/// The canonical RFC 6455 handshake key from the specification's examples.
const HANDSHAKE_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";

/// A well-formed RFC 6455 client handshake for `path`, carrying `extra` raw
/// header lines before the blank line.
fn handshake_with(address: std::net::SocketAddr, path: &str, extra: &str) -> String {
    format!(
        "GET {path} HTTP/1.1\r\nhost: {address}\r\nupgrade: websocket\r\nconnection: \
         Upgrade\r\nsec-websocket-key: {HANDSHAKE_KEY}\r\nsec-websocket-version: 13\r\n{extra}\r\n"
    )
}

/// A well-formed RFC 6455 client handshake for `path`.
fn handshake(address: std::net::SocketAddr, path: &str) -> String {
    handshake_with(address, path, "")
}

/// Read from `socket` until the head has been received in full.
async fn read_head(socket: &mut tokio::net::TcpStream) -> String {
    let mut head = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        let read = socket.read(&mut byte).await.expect("a response byte");
        assert!(
            read > 0,
            "the gateway closed the connection before answering"
        );
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
        assert!(head.len() < 8 * 1024, "the response head never ended");
    }
    String::from_utf8(head).expect("the head is ASCII")
}

/// Read the body declared by `head` (a `content-length` response).
async fn read_body(socket: &mut tokio::net::TcpStream, head: &str) -> String {
    let length = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length: "))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .expect("the response declares a length");
    let mut body = vec![0u8; length];
    socket
        .read_exact(&mut body)
        .await
        .expect("the body is readable");
    String::from_utf8_lossy(&body).into_owned()
}

/// Write one masked RFC 6455 text frame, as a client must.
async fn write_text_frame(socket: &mut tokio::net::TcpStream, text: &str) {
    let payload = text.as_bytes();
    let mask = [0x11u8, 0x22, 0x33, 0x44];
    let masked: Vec<u8> = payload
        .iter()
        .enumerate()
        .map(|(index, byte)| byte ^ mask[index % 4])
        .collect();
    let mut frame = vec![0x81u8];
    if payload.len() < 126 {
        // A short payload carries its length in the mask bit's byte; the
        // lengths are bounded by the branches, so the narrowing casts are
        // exact.
        let length = u8::try_from(payload.len()).unwrap_or(126);
        frame.push(0x80 | length);
    } else {
        frame.push(0x80 | 0x7e);
        let length = u16::try_from(payload.len()).unwrap_or(u16::MAX);
        frame.extend_from_slice(&length.to_be_bytes());
    }
    frame.extend_from_slice(&mask);
    frame.extend_from_slice(&masked);
    socket
        .write_all(&frame)
        .await
        .expect("the frame is written");
    socket.flush().await.expect("the frame is flushed");
}

/// Read one unmasked RFC 6455 text frame from the socket.
async fn read_text_frame(socket: &mut tokio::net::TcpStream) -> String {
    let mut head = [0u8; 2];
    socket
        .read_exact(&mut head)
        .await
        .expect("the frame head arrives");
    assert_eq!(
        head[0] & 0x0f,
        0x1,
        "the frame is a text frame, got opcode {}",
        head[0] & 0x0f
    );
    let length = match head[1] & 0x7f {
        126 => {
            let mut extended = [0u8; 2];
            socket
                .read_exact(&mut extended)
                .await
                .expect("the extended length arrives");
            u16::from_be_bytes(extended) as usize
        }
        127 => panic!("the test frames stay short"),
        length => length as usize,
    };
    let mut payload = vec![0u8; length];
    socket
        .read_exact(&mut payload)
        .await
        .expect("the payload arrives");
    String::from_utf8(payload).expect("the payload is UTF-8")
}

/// The upgrade is relayed end to end: the origin's `101` reaches the client
/// with its accept token, and the byte pump carries frames both ways. The test
/// speaks RFC 6455 to the gateway over a real socket so the outcome is not an
/// artefact of `oneshot`.
#[tokio::test]
async fn a_websocket_handshake_is_relayed_end_to_end() {
    let upstream = websocket_upstream().await;
    let gateway = super::gateway_with(relay_config());
    let created = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant(),
        upstream_json("127.0.0.1", upstream.address.port(), "socket.local"),
    )
    .await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let served = serve(gateway.router.clone()).await;
    let mut socket = tokio::net::TcpStream::connect(served.address)
        .await
        .expect("the gateway is reachable");
    let request = handshake(served.address, "/oagw/v1/proxy/socket.local/v1/ws");
    socket
        .write_all(request.as_bytes())
        .await
        .expect("the handshake is sent");

    let head = read_head(&mut socket).await;
    assert!(
        head.starts_with("HTTP/1.1 101"),
        "the gateway switches protocols, got: {head}"
    );
    // The RFC 6455 accept token for `HANDSHAKE_KEY`, relayed from the origin.
    assert!(
        head.to_ascii_lowercase()
            .contains("sec-websocket-accept: s3pplmbitxaq9kygzzhzrbk+xoo="),
        "the origin's accept token is relayed, got: {head}"
    );
    // RFC 6455 §4.2.2: the server's `101` must carry `upgrade` and
    // `connection` too. Both are hop-by-hop, so a relay that filters them loses
    // exactly the headers strict clients validate before accepting the socket.
    let lowered = head.to_ascii_lowercase();
    let upgrade = lowered
        .lines()
        .find_map(|line| line.strip_prefix("upgrade: "))
        .expect("the relayed 101 carries an `upgrade` header");
    assert_eq!(
        upgrade, "websocket",
        "the origin's upgrade token is relayed, got: {head}"
    );
    let connection = lowered
        .lines()
        .find_map(|line| line.strip_prefix("connection: "))
        .expect("the relayed 101 carries a `connection` header");
    assert_eq!(
        connection, "upgrade",
        "the origin's connection token is relayed, got: {head}"
    );

    // A frame written by the client reaches the origin, and the origin's echo
    // comes back: the relay pumps bytes in both directions.
    write_text_frame(&mut socket, "ping").await;
    assert_eq!(read_text_frame(&mut socket).await, "echo:ping");
    write_text_frame(&mut socket, "second").await;
    assert_eq!(read_text_frame(&mut socket).await, "echo:second");

    socket.shutdown().await.expect("the client socket closes");
}

/// Body framing is not validated on an upgrade: its body is always empty, so a
/// declaration it happens to carry cannot match anything and would make every
/// handshake fail. The same headers on a non-upgrade request are refused with a
/// `400` (see the data-plane framing tests), so reaching `101` here is the
/// proof that the check is skipped.
#[tokio::test]
async fn an_upgrade_skips_the_body_framing_check() {
    let upstream = websocket_upstream().await;
    let gateway = super::gateway_with(relay_config());
    let created = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant(),
        upstream_json("127.0.0.1", upstream.address.port(), "framed.local"),
    )
    .await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let served = serve(gateway.router.clone()).await;
    let mut socket = tokio::net::TcpStream::connect(served.address)
        .await
        .expect("the gateway is reachable");
    // A `content-length` that cannot match the (absent) body.
    let request = handshake_with(
        served.address,
        "/oagw/v1/proxy/framed.local/v1/ws",
        "content-length: 99\r\n",
    );
    socket
        .write_all(request.as_bytes())
        .await
        .expect("the handshake is sent");

    let head = read_head(&mut socket).await;
    assert!(
        head.starts_with("HTTP/1.1 101"),
        "a declared length never blocks an upgrade, got: {head}"
    );

    socket.shutdown().await.expect("the client socket closes");
}

/// An upgrade this gateway cannot speak is relayed as an ordinary request.
///
/// A bare `Upgrade` header is not a handshake: without `Connection: upgrade`
/// and a WebSocket key the request keeps its body, the hop-by-hop tokens are
/// stripped, and the origin answers normally instead of the gateway inventing
/// a `101` with nothing on the other end.
#[tokio::test]
async fn a_non_websocket_upgrade_is_relayed_with_its_body() {
    let (gateway, _origin) = relay_gateway("h2c.local", "/v1", &["POST"]).await;
    let mut response = proxy(
        &gateway,
        Method::POST,
        "/oagw/v1/proxy/h2c.local/v1/upload",
        &[
            ("upgrade", "h2c"),
            ("connection", "upgrade"),
            ("content-type", "text/plain"),
        ],
        b"h2c payload",
    )
    .await;

    assert_eq!(
        status_of(&response).as_u16(),
        200,
        "the origin answers, not a gateway-invented 101"
    );
    let echoed = body_text(&mut response).await;
    assert!(
        echoed.contains("h2c payload"),
        "the body survives a refused upgrade protocol, got: {echoed}"
    );
    assert!(
        !echoed.contains("\"upgrade\""),
        "the hop-by-hop token never reaches the upstream, got: {echoed}"
    );
}

/// Aborts a spawned task when dropped.
struct DropGuard(tokio::task::JoinHandle<()>);

impl Drop for DropGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Against an origin that only speaks plain HTTP the upgrade fails with
/// `protocol.error`, even though the origin received the tokens it would have
/// needed to accept it.
#[tokio::test]
async fn an_upgrade_against_a_plain_upstream_is_a_502() {
    let (gateway, origin) = relay_gateway("plain.local", "/v1", &["GET"]).await;
    let response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/plain.local/v1/ws",
        &[
            ("upgrade", "websocket"),
            ("connection", "Upgrade"),
            ("sec-websocket-key", HANDSHAKE_KEY),
        ],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 502);
    super::assert_problem(
        &response,
        crate::domain::error::ErrorKind::ProtocolError.gts_fragment(),
        502,
    );

    let captured = origin.only();
    assert_eq!(
        captured.header("upgrade"),
        Some("websocket"),
        "the upgrade token travels to the origin: {:?}",
        captured.headers
    );
    assert_eq!(
        captured.header("connection"),
        Some("Upgrade"),
        "the connection token travels to the origin"
    );
    assert_eq!(
        captured.header("sec-websocket-key"),
        Some(HANDSHAKE_KEY),
        "the handshake key travels to the origin"
    );
    assert!(
        captured.has_header("host"),
        "the origin still receives a well-formed request: {:?}",
        captured.headers
    );
}

/// An origin that explicitly refuses the protocol switch is reported as a
/// `protocol.error` problem document, still over a real socket.
#[tokio::test]
async fn an_origin_that_refuses_the_upgrade_is_a_502() {
    let upstream = refusing_upstream().await;
    let gateway = super::gateway_with(relay_config());
    let created = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant(),
        upstream_json("127.0.0.1", upstream.address.port(), "refusing.local"),
    )
    .await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let served = serve(gateway.router.clone()).await;
    let mut socket = tokio::net::TcpStream::connect(served.address)
        .await
        .expect("the gateway is reachable");
    let request = handshake(served.address, "/oagw/v1/proxy/refusing.local/v1/ws");
    socket
        .write_all(request.as_bytes())
        .await
        .expect("the handshake is sent");

    let head = read_head(&mut socket).await;
    assert!(
        head.starts_with("HTTP/1.1 502"),
        "the refused upgrade is reported as a gateway failure, got: {head}"
    );
    assert!(
        head.contains("content-type: application/problem+json"),
        "the refusal is a problem document, got: {head}"
    );
    assert!(
        head.contains(&format!("{ERROR_SOURCE_HEADER}: {SOURCE_GATEWAY}")),
        "the refusal is the gateway's own, got: {head}"
    );

    let body = read_body(&mut socket, &head).await;
    assert!(
        body.contains("did not accept the WebSocket upgrade") && body.contains("status 404"),
        "the problem document names the refused upgrade, got: {body}"
    );
}

#[tokio::test]
async fn a_websocket_request_to_a_disabled_transport_is_rejected() {
    let origin = Origin::spawn(Reply::Echo).await;
    let mut config = relay_config();
    config.allow_http_upstream = false;
    let gateway = super::gateway_with(config);
    let created = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant(),
        upstream_json(origin.host(), origin.port(), "gated.local"),
    )
    .await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/gated.local/v1/ws",
        &[("upgrade", "websocket"), ("connection", "Upgrade")],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 503);
    super::assert_problem(
        &response,
        crate::domain::error::ErrorKind::LinkUnavailable.gts_fragment(),
        503,
    );
}

#[tokio::test]
async fn a_websocket_request_to_a_tls_upstream_is_unavailable() {
    let origin = Origin::spawn(Reply::Echo).await;
    let gateway = super::gateway_with(relay_config());
    let mut upstream = upstream_json(origin.host(), origin.port(), "secure.local");
    upstream["server"]["endpoints"][0]["scheme"] = json!("https");
    let created = post_json(&gateway, "/oagw/v1/upstreams", tenant(), upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/secure.local/v1/ws",
        &[
            ("upgrade", "websocket"),
            ("connection", "Upgrade"),
            ("sec-websocket-key", HANDSHAKE_KEY),
            ("sec-websocket-version", "13"),
        ],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 503);
    super::assert_problem(
        &response,
        crate::domain::error::ErrorKind::LinkUnavailable.gts_fragment(),
        503,
    );
}

/// The upstream deadline also bounds the body phase: an origin that answers and
/// then goes silent mid-stream has its relay aborted at the deadline instead of
/// holding the caller's connection open forever.
#[tokio::test]
async fn a_stream_that_stalls_mid_body_is_aborted_by_the_deadline() {
    let origin = Origin::spawn(Reply::StallAfter {
        content_type: "text/event-stream",
        head: b"data: first\n\n",
    })
    .await;
    let mut config = relay_config();
    config.proxy_timeout_secs = 1;
    let gateway = super::gateway_with(config);
    let created = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant(),
        upstream_json(origin.host(), origin.port(), "stall-after.local"),
    )
    .await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/stall-after.local/v1/stream",
        &[],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 200);
    let mut stream = response.into_body().into_data_stream();
    let started = Instant::now();
    let first = stream
        .next()
        .await
        .expect("the frame written before the stall arrives")
        .expect("the first frame is readable");
    assert_eq!(
        String::from_utf8(first.to_vec()).expect("utf8"),
        "data: first\n\n",
        "the frames written before the stall are relayed"
    );

    // The next read can only end one way: the deadline expires, so the body
    // ends (possibly reported as an error) instead of waiting for the origin.
    let _ = stream.next().await;
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "the relay gave up after {elapsed:?}, not after the origin's own 30 s silence"
    );
}
