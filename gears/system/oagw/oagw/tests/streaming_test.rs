//! Streaming and protocol upgrades (T039, T040).
//!
//! These run the gear's router behind a **real** hyper server on a loopback
//! socket: incremental body delivery and RFC 6455 upgrades only exist on a
//! live connection, so an in-memory `oneshot` cannot witness them. The
//! upstream side is a raw `TcpListener` stub, which is the only way to control
//! when bytes leave and when the connection dies.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// ---------------------------------------------------------------------------
// SSE (T039)
// ---------------------------------------------------------------------------

/// What an SSE stub saw.
#[derive(Debug, Default, Clone)]
struct StreamRecord {
    /// Whether an event write failed (the peer had gone).
    write_failed: bool,
    /// The instant the connection was closed by the peer.
    closed_at: Option<Instant>,
    /// How many events were written in total.
    written: usize,
}

/// Serves `count` SSE events, `gap` apart, then closes.
async fn serve_events(record: Arc<Mutex<StreamRecord>>, count: usize, gap: Duration) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bindable");
    let addr = listener.local_addr().expect("address");
    tokio::spawn(async move {
        let (sock, _) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(_) => return,
        };
        // A watcher that notices the peer going away: the gateway dropping the
        // streamed response closes the connection, and that is the observable
        // a client disconnect has to produce. The socket is split so the
        // watcher owns the reading side while the stub writes on its own; the
        // head is read first, from the side that owns reading. The halves are
        // the *owned* ones, because dropping the writing half has to half-close
        // the connection rather than keep it open for the watcher.
        let (mut reader, mut writer) = sock.into_split();
        let mut head = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            let read = match reader.read(&mut buffer).await {
                Ok(0) | Err(_) => return,
                Ok(read) => read,
            };
            head.extend_from_slice(&buffer[..read]);
            if head.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let response =
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
        if writer.write_all(response).await.is_err() {
            return;
        }
        let _ = writer.flush().await;
        // From here the reading side belongs to the watcher.
        let closed = record.clone();
        tokio::spawn(async move {
            let mut scratch = [0u8; 64];
            loop {
                match reader.read(&mut scratch).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => continue,
                }
            }
            closed.lock().expect("record").closed_at = Some(Instant::now());
        });
        for index in 0..count {
            tokio::time::sleep(gap).await;
            let event = format!("event: message\ndata: {index}\n\n");
            if writer.write_all(event.as_bytes()).await.is_err() {
                record.lock().expect("record").write_failed = true;
                return;
            }
            let _ = writer.flush().await;
            record.lock().expect("record").written = index + 1;
        }
        // Dropping the writing half closes the socket; the client should see
        // the end.
        drop(writer);
    });
    addr
}

/// A gear with a `GET /events` route over a plaintext upstream.
async fn event_gear(stub: SocketAddr) -> Harness {
    let harness = Harness::plaintext_gear();
    let upstream_id = create_upstream(&harness, "stream", "127.0.0.1", stub.port(), "http").await;
    create_route(&harness, &upstream_id, "/events", &["GET"]).await;
    harness
}

#[tokio::test(flavor = "multi_thread")]
async fn an_event_stream_arrives_incrementally_and_preserves_its_content_type() {
    let record = Arc::new(Mutex::new(StreamRecord::default()));
    let stub = serve_events(record.clone(), 4, Duration::from_millis(120)).await;
    let harness = event_gear(stub).await;
    let addr = harness.served_on_socket().await;

    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    stream
        .write_all(b"GET /oagw/v1/proxy/stream/events HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .expect("request sent");

    // Read the whole exchange to quiet, timing the arrivals.
    let mut received = Vec::new();
    let mut buffer = [0u8; 2048];
    let started = Instant::now();
    let mut arrivals: Vec<(Duration, usize)> = Vec::new();
    loop {
        let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buffer))
            .await
            .expect("the stream finishes")
            .expect("readable");
        if read == 0 {
            break;
        }
        received.extend_from_slice(&buffer[..read]);
        let text = String::from_utf8_lossy(&received);
        while arrivals.len() < text.matches("data:").count() {
            arrivals.push((started.elapsed(), arrivals.len() + 1));
        }
    }

    let head = String::from_utf8_lossy(&received).to_lowercase();
    assert!(
        head.contains("content-type: text/event-stream"),
        "the streaming content type is preserved, got: {head}"
    );

    assert_eq!(
        arrivals.len(),
        4,
        "every event was relayed: {arrivals:?} of {}",
        String::from_utf8_lossy(&received)
    );
    let first = arrivals[0].0;
    let last = arrivals[3].0;
    assert!(
        last - first >= Duration::from_millis(200),
        "the events were spread over time, not buffered until the body ended: {arrivals:?}"
    );
    assert!(
        first < Duration::from_millis(500),
        "the first event was not held back: {arrivals:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_upstream_close_ends_the_client_stream() {
    let record = Arc::new(Mutex::new(StreamRecord::default()));
    let stub = serve_events(record.clone(), 2, Duration::from_millis(60)).await;
    let harness = event_gear(stub).await;
    let addr = harness.served_on_socket().await;

    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    stream
        .write_all(b"GET /oagw/v1/proxy/stream/events HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .expect("request sent");

    let mut received = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut received))
        .await
        .expect("the stream ends when the upstream closes")
        .expect("readable");
    let body = String::from_utf8_lossy(&received).to_lowercase();
    assert!(body.contains("data: 1"), "the second event was relayed: {body}");
    assert!(
        !body.contains("data: 2"),
        "the stub only ever sent two events: {body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_disconnect_closes_the_upstream() {
    let record = Arc::new(Mutex::new(StreamRecord::default()));
    // Six events, a second apart: the stub would need ~6s to finish, far
    // longer than the client stays interested.
    let stub = serve_events(record.clone(), 6, Duration::from_millis(400)).await;
    let harness = event_gear(stub).await;
    let addr = harness.served_on_socket().await;

    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    stream
        .write_all(b"GET /oagw/v1/proxy/stream/events HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .expect("request sent");

    // Read the first event, then walk away.
    let mut received = Vec::new();
    let mut buffer = [0u8; 2048];
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        while !String::from_utf8_lossy(&received).contains("data: 0") {
            let read = stream.read(&mut buffer).await.expect("readable");
            if read == 0 {
                break;
            }
            received.extend_from_slice(&buffer[..read]);
        }
    })
    .await;
    drop(stream);

    // The stub must notice: the gateway drops the upstream connection, so the
    // watcher sees the connection close well before the stub has finished.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let snapshot = record.lock().expect("record").clone();
        if snapshot.write_failed || snapshot.closed_at.is_some() {
            break;
        }
        assert!(Instant::now() < deadline, "the upstream was never told the client left");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let snapshot = record.lock().expect("record").clone();
    assert!(
        snapshot.closed_at.is_some() || snapshot.write_failed,
        "the gateway closed the upstream connection: {snapshot:?}"
    );
    assert!(
        snapshot.written < 6 || snapshot.write_failed,
        "the stub streamed to nobody: {snapshot:?}"
    );
}

// ---------------------------------------------------------------------------
// WebSocket (T040)
// ---------------------------------------------------------------------------

/// What the echo stub saw on the opening handshake.
#[derive(Debug, Default, Clone)]
struct HandshakeRecord {
    upgrade: bool,
    connection: bool,
    key: Option<String>,
    version: Option<String>,
}

/// An RFC 6455 echo server: it validates the handshake it is handed and then
/// reflects text frames back, unmasked.
async fn serve_echo(record: Arc<Mutex<HandshakeRecord>>) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bindable");
    let addr = listener.local_addr().expect("address");
    tokio::spawn(async move {
        let (mut sock, _) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(_) => return,
        };
        let mut head = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            let read = match sock.read(&mut buffer).await {
                Ok(0) | Err(_) => return,
                Ok(read) => read,
            };
            head.extend_from_slice(&buffer[..read]);
            if head.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        // The key keeps its case: it is the input to the accept derivation.
        let raw_head = String::from_utf8_lossy(&head).to_string();
        let lowered = raw_head.to_lowercase();
        let key = header_of(&raw_head, "sec-websocket-key").unwrap_or_default();
        {
            let mut seen = record.lock().expect("record");
            seen.upgrade = lowered.contains("upgrade: websocket");
            seen.connection = lowered.contains("connection: upgrade");
            seen.key = Some(key.clone());
            seen.version = header_of(&lowered, "sec-websocket-version");
        }
        let accept = oagw::infra::proxy::websocket::accept_key(&key);
        let reply = format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
        );
        if sock.write_all(reply.as_bytes()).await.is_err() {
            return;
        }
        let _ = sock.flush().await;

        // Echo frames, unmasking first the way a real server must.
        let mut buffer = [0u8; 4096];
        loop {
            let read = match sock.read(&mut buffer).await {
                Ok(0) | Err(_) => return,
                Ok(read) => read,
            };
            if read < 6 {
                continue;
            }
            let mask = &buffer[2..6];
            let payload = &buffer[6..read];
            let unmasked: Vec<u8> = payload
                .iter()
                .enumerate()
                .map(|(index, byte)| byte ^ mask[index % 4])
                .collect();
            let mut frame = vec![0x81, unmasked.len() as u8];
            frame.extend_from_slice(&unmasked);
            if sock.write_all(&frame).await.is_err() {
                return;
            }
            let _ = sock.flush().await;
        }
    });
    addr
}

/// The value of a header in a request head, matched without regard to case.
fn header_of(head: &str, name: &str) -> Option<String> {
    head.lines().find_map(|line| {
        let (header, value) = line.split_once(": ")?;
        header
            .eq_ignore_ascii_case(name)
            .then(|| value.trim().to_string())
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn an_upgrade_request_handshakes_and_then_relays_frames() {
    let record = Arc::new(Mutex::new(HandshakeRecord::default()));
    let stub = serve_echo(record.clone()).await;
    let harness = Harness::plaintext_gear();
    let upstream_id = create_upstream(&harness, "chat", "127.0.0.1", stub.port(), "http").await;
    create_route(&harness, &upstream_id, "/socket", &["GET"]).await;
    let addr = harness.served_on_socket().await;

    // The RFC 6455 example key; its accept value is specified by the RFC.
    let client_key = "dGhlIHNhbXBsZSBub25jZQ==";
    let handshake = format!(
        "GET /oagw/v1/proxy/chat/socket HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {client_key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    );

    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    stream.write_all(handshake.as_bytes()).await.expect("handshake sent");

    let mut reply = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buffer))
            .await
            .expect("the handshake is answered")
            .expect("readable");
        reply.extend_from_slice(&buffer[..read]);
        if reply.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let head = String::from_utf8_lossy(&reply).to_lowercase();
    assert!(
        head.starts_with("http/1.1 101"),
        "the upgrade is granted: {head}"
    );
    assert!(
        head.contains("sec-websocket-accept: s3pplmbitxaq9kygzzhzrbk+xoo="),
        "the accept value is derived from the client key: {head}"
    );

    // The upstream saw a handshake of its own, carrying the upgrade headers.
    let seen = record.lock().expect("record").clone();
    assert!(seen.upgrade, "`Upgrade: websocket` was forwarded");
    assert!(seen.connection, "`Connection: Upgrade` was forwarded");
    assert!(
        seen.key.is_some_and(|k| !k.is_empty()),
        "`Sec-WebSocket-Key` was forwarded"
    );
    assert_eq!(seen.version.as_deref(), Some("13"), "`Sec-WebSocket-Version` was forwarded");

    // A masked text frame round-trips through the gateway.
    let payload = b"hello gateway";
    let mut frame = vec![0x81, 0x80 | payload.len() as u8, 1, 2, 3, 4];
    frame.extend(
        payload
            .iter()
            .enumerate()
            .map(|(index, byte)| byte ^ [1u8, 2, 3, 4][index % 4]),
    );
    stream.write_all(&frame).await.expect("frame sent");

    // The echo: a two-byte header, then the payload it names.
    let mut head = [0u8; 2];
    tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut head))
        .await
        .expect("an echo arrives")
        .expect("readable");
    assert_eq!(head[0], 0x81, "a text frame comes back");
    let mut payload_back = vec![0u8; head[1] as usize];
    tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut payload_back))
        .await
        .expect("the payload arrives")
        .expect("readable");
    assert_eq!(
        payload_back, payload,
        "the message the client sent is echoed back"
    );
}
