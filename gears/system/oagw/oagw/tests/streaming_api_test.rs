//! Integration tests for the streaming surfaces of the proxy data plane.
//!
//! The tests mount the proxy endpoint exactly as the gear does —
//! [`oagw::api::proxy::routes::register_proxy`] over a real [`DataPlane`] — and
//! talk to upstreams that do not answer in one piece:
//!
//! * a `text/event-stream` answer written in chunks with the connection held
//!   open between them reaches the caller frame by frame, which is what proves
//!   the gateway never buffers a streaming body;
//! * a WebSocket upgrade is bridged socket to socket, so the caller exchanges
//!   RFC 6455 frames with the upstream over a live connection;
//! * the same plaintext loopback endpoint that
//!   `a_plaintext_upstream_is_refused_when_http_is_not_allowed` (in
//!   `tests/proxy_api_test.rs`) refuses with `allow_http_upstream` off is
//!   dialled and passed through when it is on.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

use oagw::MemoryStore;
use oagw::api::proxy::routes::register_proxy;
use oagw::domain::model::{
    Endpoint, EndpointScheme, HttpMatch, PathSuffixMode, Protocol, Route, RouteMatcher,
    ServerConfig, Upstream,
};
use oagw::domain::repo::ConfigStore;
use oagw::infra::plugin::{PluginEngine, resolver_that_fails};
use oagw::infra::proxy::config::{ConfigSource, ResolverChain};
use oagw::infra::proxy::connector::UpstreamDialer;
use oagw::infra::proxy::{DataPlane, SsrfPolicy};

// ── Noop OpenAPI registry ───────────────────────────────────────────────

struct NoopRegistry;

impl toolkit::api::OpenApiRegistry for NoopRegistry {
    fn register_operation(&self, _spec: &toolkit::api::operation_builder::OperationSpec) {}

    fn ensure_schema_raw(
        &self,
        name: &str,
        _schemas: Vec<(
            String,
            utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>,
        )>,
    ) -> String {
        name.to_owned()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// ── Harness ─────────────────────────────────────────────────────────────

struct Fixture {
    router: Router,
}

fn endpoint(port: u16) -> Endpoint {
    Endpoint::new(EndpointScheme::Http, "127.0.0.1", Some(port)).expect("endpoint")
}

fn upstream(tenant: Uuid, alias: &str, port: u16) -> Upstream {
    Upstream {
        id: Uuid::new_v4(),
        tenant_id: tenant,
        alias: alias.to_owned(),
        enabled: true,
        tags: vec![],
        server: ServerConfig {
            endpoints: vec![endpoint(port)],
        },
        protocol: Protocol::Http,
        auth: None,
        headers: None,
        plugins: None,
        rate_limit: None,
        cors: None,
        created_at: 1,
        updated_at: 1,
    }
}

fn route(tenant: Uuid, upstream_id: Uuid, path: &str, methods: &[&str]) -> Route {
    Route {
        id: Uuid::new_v4(),
        tenant_id: tenant,
        upstream_id,
        name: None,
        tags: vec![],
        matcher: RouteMatcher::Http(HttpMatch {
            methods: methods.iter().map(|method| (*method).to_owned()).collect(),
            path: path.to_owned(),
            query_allowlist: vec![],
            path_suffix_mode: PathSuffixMode::Append,
        }),
        priority: 0,
        enabled: true,
        plugins: None,
        rate_limit: None,
        cors: None,
        created_at: 1,
        updated_at: 1,
    }
}

fn data_plane(store: Arc<MemoryStore>, allow_http: bool) -> DataPlane {
    let source = ConfigSource::new(store, Arc::new(ResolverChain::new(None)));
    let dialer = UpstreamDialer::new(
        Arc::new(pingora_core::connectors::TransportConnector::new(None)),
        SsrfPolicy::disabled(),
        allow_http,
    );
    let engine = PluginEngine::with_builtins(resolver_that_fails());
    DataPlane::new(source, dialer, engine, Duration::from_secs(5))
}

fn security_context(tenant: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_type("user")
        .subject_tenant_id(tenant)
        .build()
        .expect("security context")
}

async fn fixture(
    tenant: Uuid,
    store: Arc<MemoryStore>,
    owner: Upstream,
    matched: Route,
) -> Fixture {
    store.insert_upstream(&owner).expect("upstream");
    store.insert_route(&matched).expect("route");

    let plane = Arc::new(data_plane(store, true));
    let router = register_proxy(Router::new(), &NoopRegistry)
        .layer(axum::Extension(plane))
        .layer(axum::Extension(security_context(tenant)));
    Fixture { router }
}

fn proxy_request(method: &str, path: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(format!("http://gateway{path}"));
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(Body::empty()).expect("request")
}

/// A loopback server that answers every request with a raw HTTP/1.1 response.
async fn spawn_raw_server(response: &'static str) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener");
    let port = listener.local_addr().expect("address").port();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buffer = vec![0_u8; 4096];
                let _ = AsyncReadExt::read(&mut socket, &mut buffer).await;
                let _ = AsyncWriteExt::write_all(&mut socket, response.as_bytes()).await;
                let _ = AsyncWriteExt::shutdown(&mut socket).await;
            });
        }
    });
    port
}

/// A loopback server that streams an SSE answer in pieces, holding the
/// connection open between the pieces.
///
/// Unlike [`spawn_raw_server`], the head is written first and every chunked
/// frame is written (and flushed) on its own, a beat apart, so the gateway sees
/// three separate reads rather than one buffered blob.
async fn spawn_sse_server() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener");
    let port = listener.local_addr().expect("address").port();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buffer = vec![0_u8; 4096];
                let _ = AsyncReadExt::read(&mut socket, &mut buffer).await;
                let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                     transfer-encoding: chunked\r\n\r\n";
                write_piece(&mut socket, head.as_bytes()).await;
                for piece in ["data: alpha\n\n", "data: beta\n\n", "data: gamma\n\n"] {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    let frame = format!("{:x}\r\n{piece}\r\n", piece.len());
                    write_piece(&mut socket, frame.as_bytes()).await;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
                write_piece(&mut socket, b"0\r\n\r\n").await;
                let _ = AsyncWriteExt::shutdown(&mut socket).await;
            });
        }
    });
    port
}

/// A loopback server that completes the RFC 6455 opening handshake itself and
/// then echoes one text frame back as `echo:<payload>`.
///
/// Neither `sha1` nor `base64` is a dependency of this crate, so the handshake
/// answers with the very `sec-websocket-key` it received, in the
/// `sec-websocket-accept` position: hyper validates neither header, and the
/// test asserts on the status line, on the presence of `sec-websocket-accept`
/// and on the payload round-trip rather than on the cryptographic accept value.
async fn spawn_ws_server() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener");
    let port = listener.local_addr().expect("address").port();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let head = read_head(&mut socket).await;
                let key = header_of(&head, "sec-websocket-key").unwrap_or_default();
                let reply = format!(
                    "HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\n\
                     connection: Upgrade\r\nsec-websocket-accept: {key}\r\n\r\n"
                );
                write_piece(&mut socket, reply.as_bytes()).await;
                // The bridge is byte for byte, so the frame that arrives here is
                // the caller's own: a client-to-server frame is masked, a
                // server-to-client one may not be.
                if let Some(payload) = read_text_frame(&mut socket).await {
                    let mut answer = b"echo:".to_vec();
                    answer.extend_from_slice(&payload);
                    write_text_frame(&mut socket, &answer).await;
                }
                let _ = AsyncWriteExt::shutdown(&mut socket).await;
            });
        }
    });
    port
}

/// Write one piece of an exchange and push it onto the wire.
async fn write_piece(socket: &mut tokio::net::TcpStream, piece: &[u8]) {
    let _ = AsyncWriteExt::write_all(socket, piece).await;
    let _ = AsyncWriteExt::flush(socket).await;
}

/// Read until the end of an HTTP/1.1 head, as lossy UTF-8.
async fn read_head(socket: &mut tokio::net::TcpStream) -> String {
    let mut head: Vec<u8> = Vec::new();
    let mut buffer = [0_u8; 1024];
    while !head.ends_with(b"\r\n\r\n") && head.len() < 8192 {
        let read = AsyncReadExt::read(socket, &mut buffer).await.unwrap_or(0);
        if read == 0 {
            break;
        }
        head.extend_from_slice(&buffer[..read]);
    }
    String::from_utf8_lossy(&head).to_string()
}

/// The first value of a header of an HTTP/1.1 head.
fn header_of(head: &str, name: &str) -> Option<String> {
    head.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim()
            .eq_ignore_ascii_case(name)
            .then(|| value.trim().to_owned())
    })
}

/// Read one WebSocket frame and return its payload, unmasking a masked one.
async fn read_text_frame(socket: &mut tokio::net::TcpStream) -> Option<Vec<u8>> {
    let mut header = [0_u8; 2];
    AsyncReadExt::read_exact(socket, &mut header).await.ok()?;
    let masked = header[1] & 0x80 != 0;
    let length = usize::from(header[1] & 0x7f);
    let mut mask = [0_u8; 4];
    if masked {
        AsyncReadExt::read_exact(socket, &mut mask).await.ok()?;
    }
    let mut payload = vec![0_u8; length];
    if length > 0 {
        AsyncReadExt::read_exact(socket, &mut payload).await.ok()?;
    }
    if masked {
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }
    }
    Some(payload)
}

/// Write an unmasked text frame: a server must never mask.
async fn write_text_frame(socket: &mut tokio::net::TcpStream, payload: &[u8]) {
    let length = u8::try_from(payload.len()).expect("a frame this short");
    let mut frame = vec![0x81_u8, length];
    frame.extend_from_slice(payload);
    write_piece(socket, &frame).await;
}

/// A masked text frame, as a browser sends one: client-to-server frames are
/// always masked (RFC 6455 §5.1).
fn masked_text_frame(payload: &[u8]) -> Vec<u8> {
    let mask = [0x37_u8, 0xfa, 0x21, 0x3d];
    let length = u8::try_from(payload.len()).expect("a frame this short");
    let mut frame = vec![0x81_u8, length | 0x80];
    frame.extend_from_slice(&mask);
    for (index, byte) in payload.iter().enumerate() {
        frame.push(byte ^ mask[index % 4]);
    }
    frame
}

/// Read from a socket until `needle` arrives, bounded in time.
async fn read_until(socket: &mut tokio::net::TcpStream, needle: &[u8]) -> Vec<u8> {
    let mut bytes: Vec<u8> = Vec::new();
    let mut buffer = [0_u8; 256];
    loop {
        let read = tokio::time::timeout(
            Duration::from_secs(5),
            AsyncReadExt::read(socket, &mut buffer),
        )
        .await
        .expect("the reply arrives in time")
        .expect("the reply is readable");
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
        if bytes.windows(needle.len()).any(|window| window == needle) || bytes.len() > 1024 {
            break;
        }
    }
    bytes
}

/// Count the data frames of a body and concatenate their payloads.
async fn data_frames(body: Body) -> (usize, String) {
    let mut body = body;
    let mut count = 0;
    let mut text = String::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.expect("frame");
        if frame.is_data() {
            count += 1;
            let bytes = frame.into_data().expect("a data frame");
            text.push_str(&String::from_utf8_lossy(&bytes));
        }
    }
    (count, text)
}

// ── Tests ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_server_sent_event_stream_arrives_frame_by_frame() {
    let port = spawn_sse_server().await;
    let store = Arc::new(MemoryStore::new());
    let tenant = Uuid::new_v4();
    let owner = upstream(tenant, "payments", port);
    let matched = route(tenant, owner.id, "/v1/*", &["GET"]);
    let Fixture { router } = fixture(tenant, store, owner, matched).await;

    let response = router
        .oneshot(proxy_request(
            "GET",
            "/oagw/v1/proxy/payments/v1/stream",
            &[],
        ))
        .await
        .expect("infallible service");
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    assert_eq!(content_type.as_deref(), Some("text/event-stream"));

    // The body is consumed one frame at a time, never collected into a blob.
    let (count, text) = data_frames(response.into_body()).await;
    assert!(count > 1, "a stream must arrive in pieces, got {count}");
    assert!(
        count >= 3,
        "the three SSE events must each be their own frame, got {count}"
    );
    let (alpha, beta, gamma) = (
        text.find("data: alpha").expect("the alpha event"),
        text.find("data: beta").expect("the beta event"),
        text.find("data: gamma").expect("the gamma event"),
    );
    assert!(
        alpha < beta && beta < gamma,
        "the events must stay in order"
    );
}

#[tokio::test]
async fn a_websocket_upgrade_is_bridged_to_a_live_upstream_socket() {
    let port = spawn_ws_server().await;
    let store = Arc::new(MemoryStore::new());
    let tenant = Uuid::new_v4();
    let owner = upstream(tenant, "payments", port);
    // `/*` so the upgrade request itself is what the route matches.
    let matched = route(tenant, owner.id, "/*", &["GET"]);
    store.insert_upstream(&owner).expect("upstream");
    store.insert_route(&matched).expect("route");

    let plane = Arc::new(data_plane(store, true));
    let router = register_proxy(Router::new(), &NoopRegistry)
        .layer(axum::Extension(plane))
        .layer(axum::Extension(security_context(tenant)));

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
    listener
        .set_nonblocking(true)
        .expect("non-blocking listener");
    let listener = tokio::net::TcpListener::from_std(listener).expect("tokio listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move { axum::serve(listener, router).await.expect("server") });

    let mut socket = tokio::net::TcpStream::connect(address)
        .await
        .expect("the gateway is listening");
    // `dGhlIHNhbXBsZSBub25jZQ==` is the RFC 6455 example key: 16 bytes when
    // decoded, which is all the field has to be for the handshake to run.
    let request = format!(
        "GET /oagw/v1/proxy/payments/ws HTTP/1.1\r\nhost: {address}\r\n\
         upgrade: websocket\r\nconnection: Upgrade\r\n\
         sec-websocket-key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         sec-websocket-version: 13\r\n\r\n"
    );
    AsyncWriteExt::write_all(&mut socket, request.as_bytes())
        .await
        .expect("the handshake request is written");
    let reply = read_head(&mut socket).await;
    assert!(
        reply.starts_with("HTTP/1.1 101"),
        "the upgrade must be answered with a 101, got {reply:?}"
    );
    assert!(
        header_of(&reply, "sec-websocket-accept").is_some(),
        "the upstream's accept header must be passed through, got {reply:?}"
    );

    AsyncWriteExt::write_all(&mut socket, &masked_text_frame(b"hello"))
        .await
        .expect("the frame is written");
    let echoed = String::from_utf8_lossy(&read_until(&mut socket, b"echo:hello").await).to_string();
    assert!(
        echoed.contains("echo:hello"),
        "the payload must round-trip over the bridged socket, got {echoed:?}"
    );

    server.abort();
}

#[tokio::test]
async fn a_plaintext_upstream_is_dialled_when_http_is_allowed() {
    let port =
        spawn_raw_server("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\r\n{\"ok\":true}")
            .await;
    let store = Arc::new(MemoryStore::new());
    let tenant = Uuid::new_v4();
    let owner = upstream(tenant, "payments", port);
    let matched = route(tenant, owner.id, "/v1/*", &["GET"]);
    store.insert_upstream(&owner).expect("upstream");
    store.insert_route(&matched).expect("route");

    let plane = Arc::new(data_plane(store, true));
    let router = register_proxy(Router::new(), &NoopRegistry)
        .layer(axum::Extension(plane))
        .layer(axum::Extension(security_context(tenant)));

    let response = router
        .oneshot(proxy_request(
            "GET",
            "/oagw/v1/proxy/payments/v1/charges",
            &[],
        ))
        .await
        .expect("infallible service");
    assert_eq!(response.status(), StatusCode::OK);
    let (count, text) = data_frames(response.into_body()).await;
    assert_eq!(count, 1);
    assert_eq!(text, "{\"ok\":true}");
}
