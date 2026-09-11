//! AT-7: streaming (`contracts/proxy-api.md` § 3).
//!
//! Server-sent events are relayed incrementally — the client reads the first
//! event before the upstream has finished — and a WebSocket upgrade answers
//! `101` with frames flowing in both directions.
//!
//! [`tower::ServiceExt::oneshot`] buffers the whole exchange, so both halves
//! run over real loopback sockets: the upstream is a raw TCP listener the test
//! frames by hand, and the gear's router is served by the same hyper stack the
//! platform uses.

mod common;

use axum::http::StatusCode;
use common::net::{bind_upstream, header_of, read_request, serve, write_raw};
use common::{Caller, app, create_route, create_upstream, with_context};

/// Frames `payload` as one HTTP/1.1 chunk.
fn chunk(payload: &str) -> String {
    format!("{:x}\r\n{}\r\n", payload.len(), payload)
}

/// A GET route on `path` that appends the suffix.
fn route(upstream_id: &str, path: &str) -> String {
    serde_json::json!({
        "enabled": true,
        "upstream_id": upstream_id,
        "match": { "http": {
            "methods": ["GET"], "path": path,
            "query_allowlist": [], "path_suffix_mode": "append"
        }}
    })
    .to_string()
}

#[tokio::test]
async fn server_sent_events_are_relayed_before_the_upstream_finishes() {
    let (addr, listener) = bind_upstream().await;
    let alias = format!("sse-{addr}");
    let app = app();
    let caller = Caller::default();

    let upstream =
        create_upstream(&app, &caller, &common::upstream_body(&alias, addr.port())).await;
    let id = upstream["id"].as_str().expect("id").to_owned();
    let _ = create_route(&app, &caller, &route(&id, "/v1")).await;

    // The router is served over a real socket, so the client side of the
    // exchange is a plain TCP stream too.
    let router = serve(with_context(app, &caller)).await;
    let mut client = tokio::net::TcpStream::connect(router)
        .await
        .expect("connect");

    // The upstream holds the second event back until the test has read the
    // first one: the gateway forwards no client body on a GET, so a socket
    // handshake cannot signal it.
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let upstream_task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let _ = read_request(&mut stream).await;
        // Hand-framed chunked response: the first event is written, then the
        // test is given the chance to read it before the second is written.
        write_raw(
            &mut stream,
            concat!(
                "HTTP/1.1 200 OK\r\n",
                "content-type: text/event-stream\r\n",
                "transfer-encoding: chunked\r\n",
                "\r\n",
            )
            .as_bytes(),
        )
        .await;
        write_raw(&mut stream, chunk("data: first\n\n").as_bytes()).await;
        let Ok(()) = released.await else {
            return;
        };
        write_raw(&mut stream, chunk("data: second\n\n").as_bytes()).await;
        write_raw(&mut stream, b"0\r\n\r\n").await;
    });

    let request = format!(
        "GET /oagw/v1/proxy/{alias}/v1/stream HTTP/1.1\r\nhost: {router}\r\nconnection: close\r\n\r\n"
    );
    tokio::io::AsyncWriteExt::write_all(&mut client, request.as_bytes())
        .await
        .expect("write request");

    // Read the head, then the first event.
    let mut reader = Reader {
        stream: &mut client,
        buffer: Vec::new(),
    };
    let head = reader.head().await;
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert_eq!(
        header_of(&head, "content-type").as_deref(),
        Some("text/event-stream"),
        "{head}"
    );
    let first = reader.read_until(b"first").await;
    assert!(
        String::from_utf8_lossy(&first).contains("data: first"),
        "{first:?}"
    );

    // Only now does the upstream write the second event: the relay is
    // incremental, not buffered until completion.
    release.send(()).expect("release the second event");
    let second = reader.read_until(b"second").await;
    assert!(
        String::from_utf8_lossy(&second).contains("data: second"),
        "{second:?}"
    );

    upstream_task.await.expect("upstream task");
}

#[tokio::test]
async fn a_websocket_upgrade_answers_101_and_relays_frames_both_ways() {
    let (addr, listener) = bind_upstream().await;
    let alias = format!("ws-{addr}");
    let app = app();
    let caller = Caller::default();

    let upstream =
        create_upstream(&app, &caller, &common::upstream_body(&alias, addr.port())).await;
    let id = upstream["id"].as_str().expect("id").to_owned();
    let _ = create_route(&app, &caller, &route(&id, "/v1")).await;

    let router = serve(with_context(app, &caller)).await;
    let mut client = tokio::net::TcpStream::connect(router)
        .await
        .expect("connect");

    let upstream_task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let (head, _) = read_request(&mut stream).await;
        // The upgrade request is forwarded with its upgrade headers intact.
        assert_eq!(header_of(&head, "upgrade").as_deref(), Some("websocket"));
        assert_eq!(header_of(&head, "connection").as_deref(), Some("Upgrade"));
        assert!(header_of(&head, "sec-websocket-key").is_some());

        write_raw(
            &mut stream,
            concat!(
                "HTTP/1.1 101 Switching Protocols\r\n",
                "upgrade: websocket\r\n",
                "connection: Upgrade\r\n",
                "\r\n",
            )
            .as_bytes(),
        )
        .await;
        // Server → client frame: a masked-less text frame is enough to prove
        // the splice carries bytes in this direction.
        tokio::io::AsyncWriteExt::write_all(
            &mut stream,
            &[0x81, 0x05, b'h', b'e', b'l', b'l', b'o'],
        )
        .await
        .expect("server frame");
        // Client → server frame.
        let mut frame = [0_u8; 7];
        tokio::io::AsyncReadExt::read_exact(&mut stream, &mut frame)
            .await
            .expect("client frame");
        frame
    });

    let request = format!(
        "GET /oagw/v1/proxy/{alias}/v1/ws HTTP/1.1\r\nhost: {router}\r\nupgrade: websocket\r\nconnection: Upgrade\r\nsec-websocket-key: dGhlIHNhbXBsZSBub25jZQ==\r\nsec-websocket-version: 13\r\n\r\n"
    );
    tokio::io::AsyncWriteExt::write_all(&mut client, request.as_bytes())
        .await
        .expect("write request");

    let mut reader = Reader {
        stream: &mut client,
        buffer: Vec::new(),
    };
    let head = reader.head().await;
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    assert_eq!(
        header_of(&head, "upgrade").as_deref(),
        Some("websocket"),
        "{head}"
    );

    // Server → client: the frame the upstream wrote arrives on the client leg.
    let received = reader.read_exact(7).await;
    assert_eq!(&received[..2], &[0x81, 0x05]);
    assert_eq!(&received[2..], b"hello");

    // Client → server: a frame the client writes reaches the upstream.
    let frame = [0x81, 0x85, 0x01, 0x02, 0x03, 0x04, 0x05];
    tokio::io::AsyncWriteExt::write_all(&mut client, &frame)
        .await
        .expect("write frame");
    let echoed = upstream_task.await.expect("upstream task");
    assert_eq!(&echoed[..2], &[0x81, 0x85]);
}

/// A buffered reader over a stream, so the head and the events that follow it
/// can come out of the same TCP segment without the second being dropped.
struct Reader<'a> {
    stream: &'a mut tokio::net::TcpStream,
    buffer: Vec<u8>,
}

impl Reader<'_> {
    /// Reads the response head, up to and including the blank line; whatever
    /// follows it stays buffered for the next read.
    async fn head(&mut self) -> String {
        let end = loop {
            if let Some(index) = self.buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                break index;
            }
            self.fill().await;
        };
        let head = String::from_utf8_lossy(&self.buffer[..end]).to_string();
        // Consume the head and its terminator, so the next read starts on the
        // first byte of the body.
        self.buffer.drain(..end + 4);
        head
    }

    /// Reads until `needle` has been seen.
    async fn read_until(&mut self, needle: &[u8]) -> Vec<u8> {
        loop {
            if self.buffer.windows(needle.len()).any(|w| w == needle) {
                return std::mem::take(&mut self.buffer);
            }
            self.fill().await;
        }
    }

    /// Reads exactly `n` bytes.
    async fn read_exact(&mut self, n: usize) -> Vec<u8> {
        while self.buffer.len() < n {
            self.fill().await;
        }
        let rest = self.buffer.split_off(n);
        std::mem::replace(&mut self.buffer, rest)
    }

    async fn fill(&mut self) {
        let mut chunk = [0_u8; 512];
        let read = tokio::io::AsyncReadExt::read(self.stream, &mut chunk)
            .await
            .expect("read from the client leg");
        assert!(read > 0, "peer closed the client leg");
        self.buffer.extend_from_slice(&chunk[..read]);
    }
}

#[tokio::test]
async fn a_streamed_response_keeps_its_content_type_and_source_tag() {
    let (addr, listener) = bind_upstream().await;
    let alias = format!("tag-{addr}");
    let app = app();
    let caller = Caller::default();

    let upstream =
        create_upstream(&app, &caller, &common::upstream_body(&alias, addr.port())).await;
    let id = upstream["id"].as_str().expect("id").to_owned();
    let _ = create_route(&app, &caller, &route(&id, "/v1")).await;

    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let _ = read_request(&mut stream).await;
        let head = concat!(
            "HTTP/1.1 200 OK\r\n",
            "content-type: text/event-stream\r\n",
            "transfer-encoding: chunked\r\n",
            "\r\n",
        );
        write_raw(&mut stream, head.as_bytes()).await;
        write_raw(&mut stream, chunk("data: hello\n\n").as_bytes()).await;
        write_raw(&mut stream, b"0\r\n\r\n").await;
    });

    let response = common::request_with(
        &app,
        &caller,
        "GET",
        &format!("/oagw/v1/proxy/{alias}/v1/stream"),
        None,
    )
    .await;
    let (parts, body) = response;
    assert_eq!(parts.status, StatusCode::OK);
    assert_eq!(
        parts
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream")
    );
    assert_eq!(
        parts
            .headers
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some("upstream")
    );
    let text = String::from_utf8_lossy(&body).to_string();
    assert_eq!(text, "data: hello\n\n", "{text:?}");
    server.await.expect("upstream task");
}
