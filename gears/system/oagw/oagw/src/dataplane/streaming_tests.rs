// Created: 2026-09-04 by Constructor Tech
//! Integration tests of the streaming pass-through
//! (`docs/PRD.md` §5.4, `cpt-cf-oagw-fr-streaming`).
//!
//! Server-sent-event answers are read from a router harness, exactly like the
//! proxy tests: a streamed body is observable through the axum `Body` alone.
//! WebSocket upgrades need a real HTTP/1.1 connection on both ends — an
//! in-process `oneshot` never completes an upgrade — so those tests run a
//! throwaway `axum::serve` on an ephemeral port and speak the handshake and
//! the frames over a raw `TcpStream`, with no WebSocket dependency.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures_util::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tower::ServiceExt;
use uuid::Uuid;

use super::*;
use crate::config::OagwConfig;
use crate::config::SsrfPolicy;
use crate::controlplane::{service::ControlPlaneService, store::ControlPlaneStore};
use crate::dataplane::headers;
use crate::dataplane::proxy::PROXY_ALIAS_PREFIX;
use crate::domain::plugin::TRANSFORM_REQUEST_ID;
use crate::domain::{
    Alias, Endpoint, EndpointScheme, HttpMatch, HttpMethod, PathSuffixMode, PluginChain,
    PluginKind, PluginRef, Protocol, RouteMatch, RouteSpec, ServerConfig, SharingMode,
    UpstreamSpec,
};
use crate::error::{ERROR_SOURCE_GATEWAY, ERROR_SOURCE_HEADER, ERROR_SOURCE_UPSTREAM};

/// Fixed tenant of every request in this module.
fn tenant_id() -> Uuid {
    Uuid::from_u128(0x0BEA)
}

/// A `SecurityContext` bound to [`tenant_id`], as the auth middleware would
/// inject it.
fn security_context() -> toolkit_security::SecurityContext {
    toolkit_security::SecurityContext::builder()
        .subject_id(Uuid::from_u128(0x5EB))
        .subject_tenant_id(tenant_id())
        .build()
        .unwrap()
}

/// The gear configuration of the tests.
fn config(proxy_timeout_secs: u64, allow_http: bool) -> OagwConfig {
    OagwConfig {
        proxy_timeout_secs,
        allow_http_upstream: allow_http,
        ssrf_policy: SsrfPolicy::default(),
        max_body_bytes: 1024 * 1024,
    }
}

/// The router, the control plane and the data plane of one test.
struct Harness {
    router: Router,
    svc: Arc<ControlPlaneService>,
}

use crate::dataplane::{DataPlane, never_cancelled, register_proxy_routes};

fn harness(config: OagwConfig) -> Harness {
    let svc = Arc::new(ControlPlaneService::new(Arc::new(ControlPlaneStore::new())));
    let plane = Arc::new(DataPlane::new(
        Arc::clone(&svc),
        Arc::new(config),
        never_cancelled(),
    ));
    let router = register_proxy_routes(Router::new(), Arc::clone(&plane));
    Harness { router, svc }
}

/// Registers a plaintext upstream of the tests under an explicit alias.
fn registered_upstream(h: &Harness, alias: &str, port: u16) -> Uuid {
    registered_upstream_with(h, alias, port, None)
}

/// Registers a plaintext upstream carrying a plugin chain.
fn registered_upstream_with(
    h: &Harness,
    alias: &str,
    port: u16,
    plugins: Option<PluginChain>,
) -> Uuid {
    let spec = UpstreamSpec {
        tenant_id: tenant_id(),
        alias: Some(Alias::parse(alias).unwrap()),
        protocol: Protocol::Http,
        enabled: true,
        server: ServerConfig::new(vec![
            Endpoint::new(EndpointScheme::Http, "127.0.0.1", Some(port)).unwrap(),
        ])
        .unwrap(),
        auth: None,
        headers: None,
        plugins,
        rate_limit: None,
        cors: None,
        tags: Vec::new(),
    };
    h.svc.create_upstream(&spec).unwrap().id
}

/// A route of `upstream` matching `path` for `GET`.
fn registered_route(h: &Harness, upstream_id: Uuid, path: &str) {
    let spec = RouteSpec {
        tenant_id: tenant_id(),
        upstream_id,
        r#match: RouteMatch::Http(
            HttpMatch::new(
                vec![HttpMethod::parse("GET").unwrap()],
                path.to_owned(),
                Vec::new(),
                PathSuffixMode::Append,
            )
            .unwrap(),
        ),
        plugins: None,
        rate_limit: None,
        cors: None,
        enabled: true,
        tags: Vec::new(),
    };
    h.svc.create_route(&spec).unwrap();
}

/// Sends a proxied request through the mounted router.
async fn call(router: &Router, uri: &str, headers: &[(&str, &str)]) -> axum::response::Response {
    let mut builder = Request::builder().method("GET").uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let mut request = builder.body(Body::empty()).unwrap();
    request.extensions_mut().insert(security_context());
    router.clone().oneshot(request).await.unwrap()
}

/// The whole body of a response.
async fn body_of(response: &mut axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(
        std::mem::replace(response.body_mut(), Body::empty()),
        1 << 20,
    )
    .await
    .unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

// ------------------------------------------------------------- HTTP message

/// The head of an HTTP/1.1 message read from a raw connection.
#[derive(Debug, Clone)]
struct Head {
    /// Request or status line, verbatim.
    line: String,
    /// Header lines, names lowercased.
    headers: Vec<(String, String)>,
}

impl Head {
    /// Status of a response head.
    fn status(&self) -> u16 {
        self.line
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or(0)
    }

    /// Method of a request head.
    fn method(&self) -> &str {
        self.line.split_whitespace().next().unwrap_or_default()
    }

    /// Target of a request head.
    fn target(&self) -> &str {
        self.line.split_whitespace().nth(1).unwrap_or_default()
    }

    /// Value of a header, `None` when absent.
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// Reads an HTTP/1.1 message head, once it is complete, with whatever of its
/// body arrived in the same read.
async fn read_message(stream: &mut TcpStream) -> Option<Incoming> {
    let mut raw = Vec::new();
    let mut chunk = [0_u8; 1024];
    while !raw.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        raw.extend_from_slice(&chunk[..read]);
    }
    let (head, body) = std::str::from_utf8(&raw)
        .ok()?
        .split_once("\r\n\r\n")
        .map(|(head, body)| (head.to_owned(), body.as_bytes().to_vec()))?;
    let mut lines = head.split("\r\n");
    let line = lines.next()?.to_owned();
    let headers = lines
        .filter_map(|entry| entry.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    Some(Incoming {
        head: Head { line, headers },
        body,
    })
}

/// The head of an HTTP/1.1 message and whatever of its body arrived with it.
#[derive(Debug, Clone)]
struct Incoming {
    /// The head of the message.
    head: Head,
    /// Bytes of the body that arrived in the same read as the head.
    body: Vec<u8>,
}

impl Incoming {
    /// The whole body of the message, as its `content-length` declares.
    ///
    /// The gear keeps a refused connection alive, so the body is read by its
    /// declared length instead of waiting for an end of stream.
    async fn body(mut self, stream: &mut TcpStream) -> String {
        let length: usize = self
            .head
            .header("content-length")
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        while self.body.len() < length {
            let mut chunk = [0_u8; 1024];
            let read = stream.read(&mut chunk).await.unwrap();
            assert!(read > 0, "the connection closed before the whole body");
            self.body.extend_from_slice(&chunk[..read]);
        }
        self.body.truncate(length);
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

// ------------------------------------------------------- event-stream mock

/// Behaviour of the event-stream mock.
#[derive(Clone, Copy)]
enum EventStream {
    /// Two events, the second after `pause` milliseconds.
    TwoEvents(u64),
    /// A `500` answer carrying events.
    Failed,
    /// A declared body that is never completed: the upstream hangs up first.
    Aborted,
}

/// A throwaway event-stream upstream bound to an ephemeral port.
struct MockUpstream {
    addr: SocketAddr,
    worker: Option<JoinHandle<()>>,
}

impl Drop for MockUpstream {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.abort();
        }
    }
}

impl MockUpstream {
    /// Starts a mock upstream answering every connection with `behaviour`.
    async fn start(behaviour: EventStream) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let worker = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move { serve_event_stream(stream, behaviour).await });
            }
        });
        Self {
            addr,
            worker: Some(worker),
        }
    }
}

/// Reads the request head, then answers with `behaviour`.
async fn serve_event_stream(mut stream: TcpStream, behaviour: EventStream) {
    if read_message(&mut stream).await.is_none() {
        return;
    }
    match behaviour {
        EventStream::TwoEvents(pause) => {
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: \
                      no-cache\r\n\r\n",
                )
                .await
                .unwrap();
            stream.write_all(b"data: {\"n\":1}\n\n").await.unwrap();
            stream.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(pause)).await;
            stream.write_all(b"data: {\"n\":2}\n\n").await.unwrap();
            stream.flush().await.unwrap();
        }
        EventStream::Failed => {
            stream
                .write_all(
                    b"HTTP/1.1 500 Internal Server Error\r\ncontent-type: \
                      text/event-stream\r\n\r\n",
                )
                .await
                .unwrap();
            stream
                .write_all(b"data: {\"error\":true}\n\n")
                .await
                .unwrap();
            stream.flush().await.unwrap();
        }
        EventStream::Aborted => {
            // A chunked body whose terminator never arrives, then a hangup:
            // the upstream answer is interrupted mid-stream. The pause makes
            // sure the complete chunk reaches the gateway before the hangup.
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: \
                      chunked\r\n\r\n",
                )
                .await
                .unwrap();
            let event = b"data: {\"n\":1}\n\n";
            stream
                .write_all(format!("{:x}\r\n", event.len()).as_bytes())
                .await
                .unwrap();
            stream.write_all(event).await.unwrap();
            stream.write_all(b"\r\n").await.unwrap();
            stream.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
    let _ = stream.shutdown().await;
}

// ------------------------------------------------------------- event streams

/// A live event stream, with a pause between the events longer than the proxy
/// timeout of the test.
async fn event_setup(pause: u64, proxy_timeout_secs: u64) -> (Harness, MockUpstream) {
    let mock = MockUpstream::start(EventStream::TwoEvents(pause)).await;
    let harness = harness(config(proxy_timeout_secs, true));
    let upstream = registered_upstream(&harness, "mock.internal", mock.addr.port());
    registered_route(&harness, upstream, "/events");
    (harness, mock)
}

/// The proxied URI of the event-stream route.
fn events_uri() -> String {
    format!("{PROXY_ALIAS_PREFIX}mock.internal/events")
}

#[tokio::test]
async fn an_event_stream_is_forwarded_as_it_arrives() {
    let (harness, _mock) = event_setup(1_500, 1).await;
    let mut response = call(&harness.router, &events_uri(), &[]).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/event-stream"
    );
    assert_eq!(response.headers().get("cache-control").unwrap(), "no-cache");
    assert_eq!(
        response.headers().get(ERROR_SOURCE_HEADER).unwrap(),
        ERROR_SOURCE_UPSTREAM
    );

    let started = Instant::now();
    let mut stream = std::mem::replace(response.body_mut(), Body::empty()).into_data_stream();
    let first = stream.next().await.unwrap().unwrap();
    let first_at = started.elapsed();
    assert!(
        first_at < Duration::from_millis(900),
        "the first event is observed before the upstream completes: {first_at:?}"
    );
    let second = stream.next().await.unwrap().unwrap();
    let total = started.elapsed();
    assert_eq!(std::str::from_utf8(&first).unwrap(), "data: {\"n\":1}\n\n");
    assert_eq!(std::str::from_utf8(&second).unwrap(), "data: {\"n\":2}\n\n");
    assert!(
        total >= Duration::from_millis(1_400),
        "the stream outlives the proxy timeout: {total:?} (first event at {first_at:?})"
    );
    // The upstream is closed: nothing is buffered behind the last event.
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn an_event_stream_with_a_failed_status_is_passed_through() {
    let mock = MockUpstream::start(EventStream::Failed).await;
    let harness = harness(config(2, true));
    let upstream = registered_upstream(&harness, "mock.internal", mock.addr.port());
    registered_route(&harness, upstream, "/events");

    let mut response = call(&harness.router, &events_uri(), &[]).await;

    // An upstream answer is passed through untouched, whatever its status
    // (`docs/ADR/0007-error-source-distinction.md`).
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/event-stream"
    );
    assert_eq!(
        response.headers().get(ERROR_SOURCE_HEADER).unwrap(),
        ERROR_SOURCE_UPSTREAM
    );
    assert_eq!(body_of(&mut response).await, "data: {\"error\":true}\n\n");
}

#[tokio::test]
async fn an_event_stream_interrupted_mid_way_is_a_stream_abort() {
    let mock = MockUpstream::start(EventStream::Aborted).await;
    let harness = harness(config(5, true));
    let upstream = registered_upstream(&harness, "mock.internal", mock.addr.port());
    registered_route(&harness, upstream, "/events");

    let mut response = call(&harness.router, &events_uri(), &[]).await;

    assert_eq!(response.status(), StatusCode::OK);
    let mut stream = std::mem::replace(response.body_mut(), Body::empty()).into_data_stream();
    // The caller keeps what the upstream sent and then sees the stream end in
    // the documented `502 StreamAborted`: never a clean end, never a hang.
    let mut delivered = String::new();
    let aborted = loop {
        match stream.next().await {
            Some(Ok(bytes)) => delivered.push_str(&String::from_utf8_lossy(&bytes)),
            Some(Err(error)) => break error,
            None => panic!("the interrupted stream must not end cleanly"),
        }
    };
    // The error travels wrapped in the body stream's own `axum::Error`, so the
    // documented gateway error is reached through the source chain.
    let boxed = aborted.into_inner();
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(boxed.as_ref());
    let mapped = loop {
        match source.and_then(|error| error.downcast_ref::<OagwError>()) {
            Some(mapped) => break mapped,
            None => {
                source = source.and_then(std::error::Error::source);
                assert!(source.is_some(), "the abort error carries no gateway error");
            }
        }
    };
    assert_eq!(mapped.code().as_str(), "StreamAborted");
    assert!(
        delivered.is_empty() || b"data: {\"n\":1}\n\n".starts_with(delivered.as_bytes()),
        "only the partial event reaches the caller: {delivered:?}"
    );
}

// ------------------------------------------------------------ upgrade mock

/// The `Sec-WebSocket-Accept` the mock upstream answers with.
const MOCK_ACCEPT: &str = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";

/// The subprotocol the mock upstream negotiates.
const MOCK_PROTOCOL: &str = "chat";

/// A throwaway upstream that upgrades the connection and echoes frames.
struct MockWebSocket {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<Head>>>,
    frames: Arc<Mutex<Vec<String>>>,
    worker: Option<JoinHandle<()>>,
}

impl Drop for MockWebSocket {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.abort();
        }
    }
}

impl MockWebSocket {
    /// Starts a mock upstream that agrees to every upgrade, sends one frame
    /// of its own and then echoes what it receives.
    async fn start() -> Self {
        Self::spawn(true).await
    }

    /// Starts a mock upstream that refuses every upgrade with `200 OK`.
    async fn refusing() -> Self {
        Self::spawn(false).await
    }

    async fn spawn(agree: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let frames = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let received = Arc::clone(&frames);
        let worker = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let captured = Arc::clone(&captured);
                let received = Arc::clone(&received);
                tokio::spawn(async move {
                    serve_upgrade(stream, captured, received, agree).await;
                });
            }
        });
        Self {
            addr,
            requests,
            frames,
            worker: Some(worker),
        }
    }

    /// The upgrade requests the mock received so far.
    fn captured(&self) -> Vec<Head> {
        self.requests.lock().unwrap().clone()
    }

    /// The frames the mock received through the splice.
    fn frames(&self) -> Vec<String> {
        self.frames.lock().unwrap().clone()
    }
}

/// Answers one upgrade connection of the mock upstream.
async fn serve_upgrade(
    mut stream: TcpStream,
    requests: Arc<Mutex<Vec<Head>>>,
    frames: Arc<Mutex<Vec<String>>>,
    agree: bool,
) {
    let Some(message) = read_message(&mut stream).await else {
        return;
    };
    requests.lock().unwrap().push(message.head);
    if !agree {
        stream
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 15\r\n\r\nno upgrade here")
            .await
            .unwrap();
        let _ = stream.shutdown().await;
        return;
    }
    stream
        .write_all(
            format!(
                "HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\nconnection: \
                 Upgrade\r\nsec-websocket-accept: {MOCK_ACCEPT}\r\nsec-websocket-protocol: \
                 {MOCK_PROTOCOL}\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    stream.flush().await.unwrap();
    // The first frame the gateway forwards is stored, then the upstream sends
    // one of its own and echoes whatever comes next.
    let (_, payload) = read_frame(&mut stream).await;
    frames
        .lock()
        .unwrap()
        .push(String::from_utf8(payload).unwrap());
    write_text_frame(&mut stream, b"hello from upstream").await;
    let (_, echoed) = read_frame(&mut stream).await;
    write_text_frame(&mut stream, echoed.as_slice()).await;
    let _ = stream.shutdown().await;
}

// --------------------------------------------------------- WebSocket frames

/// Writes an unmasked text frame, as a server sends them.
async fn write_text_frame(stream: &mut TcpStream, payload: &[u8]) {
    let mut frame = vec![0x81_u8];
    push_length(&mut frame, payload.len(), 0x00);
    stream.write_all(&frame).await.unwrap();
    stream.write_all(payload).await.unwrap();
    stream.flush().await.unwrap();
}

/// Appends a payload length to `frame`, masked when `mask_bit` is `0x80`.
fn push_length(frame: &mut Vec<u8>, length: usize, mask: u8) {
    if length < 126 {
        frame.push(mask | u8::try_from(length).unwrap());
    } else if length < 65_536 {
        frame.push(mask | 126);
        frame.extend_from_slice(&u16::try_from(length).unwrap().to_be_bytes());
    } else {
        frame.push(mask | 127);
        frame.extend_from_slice(&u64::try_from(length).unwrap().to_be_bytes());
    }
}

/// Writes a masked text frame, as a WebSocket client must.
async fn send_frame(stream: &mut TcpStream, payload: &[u8]) {
    let mask = [0x2A_u8, 0x11, 0xC3, 0x7F];
    let mut frame = vec![0x81_u8];
    push_length(&mut frame, payload.len(), 0x80);
    frame.extend_from_slice(&mask);
    frame.extend(
        payload
            .iter()
            .enumerate()
            .map(|(index, byte)| byte ^ mask[index % 4]),
    );
    stream.write_all(&frame).await.unwrap();
    stream.flush().await.unwrap();
}

/// Reads one frame, unmasking its payload.
async fn read_frame(stream: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut head = [0_u8; 2];
    stream.read_exact(&mut head).await.unwrap();
    let opcode = head[0] & 0x0F;
    let mut length = usize::from(head[1] & 0x7F);
    if length == 126 {
        let mut extended = [0_u8; 2];
        stream.read_exact(&mut extended).await.unwrap();
        length = usize::from(u16::from_be_bytes(extended));
    } else if length == 127 {
        let mut extended = [0_u8; 8];
        stream.read_exact(&mut extended).await.unwrap();
        length = usize::try_from(u64::from_be_bytes(extended)).unwrap_or(0);
    }
    let mask = if head[1] & 0x80 != 0 {
        let mut mask = [0_u8; 4];
        stream.read_exact(&mut mask).await.unwrap();
        Some(mask)
    } else {
        None
    };
    let mut payload = vec![0_u8; length];
    stream.read_exact(&mut payload).await.unwrap();
    if let Some(mask) = mask {
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }
    }
    (opcode, payload)
}

// ---------------------------------------------------------- test HTTP server

/// A throwaway HTTP/1.1 server serving the proxy router with a peer address.
struct TestServer {
    addr: SocketAddr,
    worker: Option<JoinHandle<()>>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.abort();
        }
    }
}

impl TestServer {
    /// Serves the proxy router on an ephemeral port.
    async fn start(harness: Harness) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = harness.router.layer(axum::middleware::from_fn(
            |mut request: Request<Body>, next: axum::middleware::Next| async move {
                request.extensions_mut().insert(security_context());
                next.run(request).await
            },
        ));
        let worker = tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .expect("the test server stopped unexpectedly");
        });
        Self {
            addr,
            worker: Some(worker),
        }
    }
}

/// The gear-relative path of a proxied WebSocket route.
fn ws_path(alias: &str) -> String {
    format!("{PROXY_ALIAS_PREFIX}{alias}/ws")
}

/// The upgrade handshake of the test client.
///
/// The caller is expected to assert the answer before it speaks on the
/// spliced connection.
async fn connect_upgrade(addr: SocketAddr, path: &str) -> (TcpStream, Incoming) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let request = format!(
        "GET {path} HTTP/1.1\r\nhost: {addr}\r\nconnection: Upgrade\r\nupgrade: \
         websocket\r\nsec-websocket-key: dGhlIHNhbXBsZSBub25jZQ==\r\nsec-websocket-version: \
         13\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();
    let message = read_message(&mut stream).await.expect("an answer");
    (stream, message)
}

/// Asserts `head` opens a gateway problem of `status`.
fn assert_gateway_problem(head: &Head, status: u16) {
    assert_eq!(head.status(), status, "{:?}", head.line);
    assert_eq!(head.header(ERROR_SOURCE_HEADER), Some(ERROR_SOURCE_GATEWAY));
    assert_eq!(
        head.header("content-type"),
        Some("application/problem+json")
    );
}

/// A WebSocket upstream registered with the proxy route of the tests.
async fn upgrade_setup(mock: &MockWebSocket, proxy_timeout_secs: u64, allow_http: bool) -> Harness {
    let harness = harness(config(proxy_timeout_secs, allow_http));
    let upstream = registered_upstream(&harness, "mock.internal", mock.addr.port());
    registered_route(&harness, upstream, "/ws");
    harness
}

#[tokio::test]
async fn a_websocket_upgrade_is_spliced_between_the_caller_and_the_upstream() {
    let mock = MockWebSocket::start().await;
    let harness = upgrade_setup(&mock, 5, true).await;
    let server = TestServer::start(harness).await;

    let (mut client, message) = connect_upgrade(server.addr, &ws_path("mock.internal")).await;
    let head = &message.head;
    assert_eq!(head.status(), 101);
    assert_eq!(head.line, "HTTP/1.1 101 Switching Protocols");
    assert_eq!(head.header("sec-websocket-accept"), Some(MOCK_ACCEPT));
    assert_eq!(head.header("sec-websocket-protocol"), Some(MOCK_PROTOCOL));
    assert_eq!(
        head.header(ERROR_SOURCE_HEADER),
        Some(ERROR_SOURCE_UPSTREAM)
    );

    // The client speaks first: the frame has to reach the upstream through
    // the splice.
    send_frame(&mut client, b"ping from client").await;
    let (opcode, payload) = read_frame(&mut client).await;
    assert_eq!(opcode, 0x1);
    assert_eq!(
        std::str::from_utf8(&payload).unwrap(),
        "hello from upstream"
    );

    // The second frame is echoed by the upstream, so the caller→upstream
    // direction is proven as well.
    send_frame(&mut client, b"second frame").await;
    let (_, echoed) = read_frame(&mut client).await;
    assert_eq!(std::str::from_utf8(&echoed).unwrap(), "second frame");
    drop(client);

    let captured = mock.captured();
    assert_eq!(captured.len(), 1, "the upstream saw exactly one handshake");
    assert_eq!(captured[0].method(), "GET");
    assert_eq!(captured[0].target(), "/ws");
    assert_eq!(captured[0].header("upgrade"), Some(WEBSOCKET_TOKEN));
    assert_eq!(captured[0].header("connection"), Some("Upgrade"));
    assert_eq!(
        captured[0].header("sec-websocket-key"),
        Some("dGhlIHNhbXBsZSBub25jZQ==")
    );
    assert_eq!(captured[0].header("sec-websocket-version"), Some("13"));
    assert_eq!(mock.frames(), vec![String::from("ping from client")]);
}

#[tokio::test]
async fn the_101_answer_echoes_the_headers_negotiated_by_the_upstream() {
    let mock = MockWebSocket::start().await;
    let harness = upgrade_setup(&mock, 5, true).await;
    let server = TestServer::start(harness).await;

    let mut stream = TcpStream::connect(server.addr).await.unwrap();
    let request = format!(
        "GET {} HTTP/1.1\r\nhost: {}\r\nconnection: Upgrade\r\nupgrade: websocket\r\n\
         sec-websocket-key: dGhlIHNhbXBsZSBub25jZQ==\r\nsec-websocket-version: 13\r\n\
         sec-websocket-protocol: {}\r\n\r\n",
        ws_path("mock.internal"),
        server.addr,
        MOCK_PROTOCOL
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let message = read_message(&mut stream).await.expect("an answer");
    let head = &message.head;

    assert_eq!(head.status(), 101);
    assert_eq!(head.header("sec-websocket-accept"), Some(MOCK_ACCEPT));
    assert_eq!(head.header("sec-websocket-protocol"), Some(MOCK_PROTOCOL));
    // The handshake the caller offered reaches the upstream unchanged.
    assert_eq!(
        mock.captured()[0].header("sec-websocket-protocol"),
        Some(MOCK_PROTOCOL)
    );
}

#[tokio::test]
async fn the_plugin_stages_run_on_the_head_of_an_upgrade() {
    // `docs/ADR/0002-plugin-system.md` "Execution Order": the response stage
    // sees the head of the upgrade answer, the request stage sees the
    // handshake, and the spliced bytes stay out of every plugin's reach.
    let mock = MockWebSocket::start().await;
    let harness = harness(config(5, true));
    let chain = PluginChain {
        sharing: SharingMode::Inherit,
        items: vec![PluginRef::parse(PluginKind::Transform, TRANSFORM_REQUEST_ID).unwrap()],
    };
    let upstream =
        registered_upstream_with(&harness, "mock.internal", mock.addr.port(), Some(chain));
    registered_route(&harness, upstream, "/ws");
    let server = TestServer::start(harness).await;

    let (mut client, message) = connect_upgrade(server.addr, &ws_path("mock.internal")).await;
    let head = &message.head;
    assert_eq!(head.status(), 101);
    assert!(
        head.header("x-request-id").is_some(),
        "the transform stage never saw the upgrade head: {head:?}"
    );
    // The spliced connection still carries frames in both directions.
    send_frame(&mut client, b"ping").await;
    let (_, payload) = read_frame(&mut client).await;
    assert_eq!(
        std::str::from_utf8(&payload).unwrap(),
        "hello from upstream"
    );
    let captured = mock.captured();
    assert!(
        captured[0].header("x-request-id").is_some(),
        "the request transform stage never saw the handshake: {:?}",
        captured[0]
    );
}

#[tokio::test]
async fn an_upstream_that_refuses_the_upgrade_is_a_gateway_problem() {
    let mock = MockWebSocket::refusing().await;
    let harness = upgrade_setup(&mock, 5, true).await;
    let server = TestServer::start(harness).await;

    let (mut client, message) = connect_upgrade(server.addr, &ws_path("mock.internal")).await;
    assert_gateway_problem(&message.head, 502);

    // The body of the problem, read by its declared length: the gear keeps
    // the connection alive after a refusal.
    let body = message.body(&mut client).await;
    let problem: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(problem["status"], 502);
    assert_eq!(problem["error_code"], "ProtocolError");
    assert!(!problem["title"].as_str().unwrap_or_default().is_empty());
    // A 502 travels as the canonical internal projection, whose detail is
    // sanitized, so the refusal is identified by its GTS type identifier.
    assert!(
        problem["type"]
            .as_str()
            .unwrap_or_default()
            .contains("protocol.error"),
        "{body}"
    );
    assert_eq!(mock.captured().len(), 1, "the upstream was dialed once");
}

#[tokio::test]
async fn a_blocked_plaintext_upstream_refuses_the_upgrade_without_dialing() {
    let mock = MockWebSocket::start().await;
    // `allow_http_upstream: false` is the egress gate of
    // `cpt-cf-oagw-nfr-ssrf-protection`: the handshake never leaves the gear.
    let harness = upgrade_setup(&mock, 5, false).await;
    let server = TestServer::start(harness).await;

    let (mut client, message) = connect_upgrade(server.addr, &ws_path("mock.internal")).await;
    assert_gateway_problem(&message.head, 503);

    // The body of the problem, read by its declared length: the gear keeps
    // the connection alive after a refusal.
    let body = message.body(&mut client).await;
    let problem: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(problem["status"], 503);
    assert_eq!(problem["error_code"], "LinkUnavailable");
    assert!(body.contains("allow_http_upstream"), "{body}");
    assert!(
        mock.captured().is_empty(),
        "the upstream was dialed despite the closed egress gate"
    );
}

#[tokio::test]
async fn an_upgrade_the_connection_cannot_carry_is_refused_before_dialing() {
    // Through the router harness there is no HTTP/1.1 connection to upgrade:
    // the gateway has to refuse the handshake before dialing the upstream.
    let mock = MockWebSocket::start().await;
    let harness = upgrade_setup(&mock, 5, true).await;
    let mut response = call(
        &harness.router,
        &ws_path("mock.internal"),
        &[
            ("connection", "Upgrade"),
            ("upgrade", "websocket"),
            ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
            ("sec-websocket-version", "13"),
        ],
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response.headers().get(ERROR_SOURCE_HEADER).unwrap(),
        ERROR_SOURCE_GATEWAY
    );
    assert!(body_of(&mut response).await.contains("does not support"));
    assert!(
        mock.captured().is_empty(),
        "the upstream was dialed without an upgradable connection"
    );
}

// ------------------------------------------------------------ classification

#[test]
fn an_upgrade_request_is_recognized_by_its_headers() {
    let mut headers = HeaderMap::new();
    headers.insert("connection", "Upgrade".parse().unwrap());
    headers.insert("upgrade", "websocket".parse().unwrap());
    assert!(is_upgrade_request(&headers));
    // The token may be listed among others.
    headers.insert("connection", "keep-alive, Upgrade".parse().unwrap());
    assert!(is_upgrade_request(&headers));
    // Without the `Connection` token there is no upgrade.
    let mut headers = HeaderMap::new();
    headers.insert("upgrade", "websocket".parse().unwrap());
    assert!(!is_upgrade_request(&headers));
    // … and without an `Upgrade` header there is nothing to upgrade.
    let mut headers = HeaderMap::new();
    headers.insert("connection", "upgrade".parse().unwrap());
    assert!(!is_upgrade_request(&headers));
    // An unrelated token is still an upgrade request.
    let mut headers = HeaderMap::new();
    headers.insert("connection", "Upgrade".parse().unwrap());
    headers.insert("upgrade", "h2c".parse().unwrap());
    assert!(is_upgrade_request(&headers));
}

#[test]
fn an_event_stream_is_recognized_by_its_media_type() {
    let mut headers = HeaderMap::new();
    headers.insert(
        "content-type",
        "text/event-stream; charset=utf-8".parse().unwrap(),
    );
    assert!(is_event_stream(&headers));
    headers.insert("content-type", "application/json".parse().unwrap());
    assert!(!is_event_stream(&headers));
    headers.remove("content-type");
    assert!(!is_event_stream(&headers));
}

#[test]
fn only_a_live_stream_is_forwarded_without_a_frame_timeout() {
    let mut headers = HeaderMap::new();
    headers.insert("content-type", "text/event-stream".parse().unwrap());
    assert_eq!(
        idle_timeout(&headers, Duration::from_secs(2)),
        None,
        "an event stream carries no timeout at all"
    );
    headers.insert("content-type", "application/json".parse().unwrap());
    assert_eq!(
        idle_timeout(&headers, Duration::from_secs(2)),
        Some(Duration::from_secs(2))
    );
}

#[test]
fn the_upgrade_headers_survive_the_hop_by_hop_strip() {
    let mut inbound = HeaderMap::new();
    inbound.insert("connection", "Upgrade".parse().unwrap());
    inbound.insert("upgrade", "websocket".parse().unwrap());
    inbound.insert("sec-websocket-key", "a2V5".parse().unwrap());
    inbound.insert("sec-websocket-version", "13".parse().unwrap());
    inbound.insert("x-request-id", "abc".parse().unwrap());
    let mut outbound = headers::outbound_request_headers(&inbound, None, "127.0.0.1:8080");
    assert!(!outbound.contains_key("connection"));
    assert!(!outbound.contains_key("upgrade"));
    restore_upgrade_headers(&inbound, &mut outbound);
    assert_eq!(outbound.get("connection").unwrap(), "Upgrade");
    assert_eq!(outbound.get("upgrade").unwrap(), "websocket");
    assert_eq!(outbound.get("sec-websocket-key").unwrap(), "a2V5");
    assert_eq!(outbound.get("sec-websocket-version").unwrap(), "13");
    // `X-Request-ID` is the gateway's own: the strip removes it and the
    // pipeline re-inserts it from the request context.
    assert!(!outbound.contains_key("x-request-id"));
}

#[test]
fn switching_protocols_headers_echo_the_upstream_answer() {
    let mut upstream = HeaderMap::new();
    upstream.insert("upgrade", "websocket".parse().unwrap());
    upstream.insert("sec-websocket-accept", MOCK_ACCEPT.parse().unwrap());
    upstream.insert("sec-websocket-protocol", MOCK_PROTOCOL.parse().unwrap());
    upstream.insert(
        "sec-websocket-extensions",
        "permessage-deflate".parse().unwrap(),
    );
    upstream.insert("content-length", "0".parse().unwrap());
    let headers = switching_protocols_headers(&upstream, &HeaderMap::new());
    assert_eq!(headers.get("connection").unwrap(), "upgrade");
    assert_eq!(headers.get("upgrade").unwrap(), "websocket");
    assert_eq!(headers.get("sec-websocket-accept").unwrap(), MOCK_ACCEPT);
    assert_eq!(
        headers.get("sec-websocket-protocol").unwrap(),
        MOCK_PROTOCOL
    );
    assert_eq!(
        headers.get("sec-websocket-extensions").unwrap(),
        "permessage-deflate"
    );
    assert!(!headers.contains_key("content-length"));
    // A token the upstream omitted is echoed from the caller's request.
    let mut inbound = HeaderMap::new();
    inbound.insert("upgrade", "h2c".parse().unwrap());
    let headers = switching_protocols_headers(&HeaderMap::new(), &inbound);
    assert_eq!(headers.get("upgrade").unwrap(), "h2c");
}

#[test]
fn an_inbound_upgrade_without_a_handle_reports_itself_unavailable() {
    let captured = InboundUpgrade::default();
    assert!(!captured.is_available());
    assert!(format!("{captured:?}").contains("available: false"));
}
