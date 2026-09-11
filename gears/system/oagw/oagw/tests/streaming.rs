#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Integration tests for the streamed and upgraded exchanges of the proxy path
//! (entry 2.6).
//!
//! In-crate integration tests only (DECOMPOSITION assumption 5): no e2e suite
//! is added under `testing/e2e/gears/oagw/`. The tests mount the router the
//! host api-gateway mounts, dial loopback stub upstreams that are written by
//! hand — a raw [`TcpListener`] thread per test, so a response head, a partial
//! body, a mid-body abort and a `101` can each be produced exactly — and assert
//! the incremental relay of `text/event-stream`, the bidirectional relay of an
//! established upgrade, the abort, refusal, disconnect and idle-window
//! outcomes of `cpt-cf-oagw-dod-stream-test-coverage`.
//!
//! The upgrade test is served over a real listener through `axum::serve`, so
//! the request extensions carry the upgrade handle hyper only produces on a
//! connection it owns: a `oneshot` call has no connection to upgrade.

// @cpt-begin:cpt-cf-oagw-dod-stream-test-coverage:p2:inst-full
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    Router,
    body::Body,
    extract::Request,
    http::StatusCode,
    middleware::{self, Next},
    response::Response as AxumResponse,
};
use futures_util::StreamExt;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

use oagw::api::rest::error::{ERROR_SOURCE_GATEWAY, ERROR_SOURCE_HEADER, ERROR_SOURCE_UPSTREAM};
use oagw::api::rest::routes::{MOUNT_ROOT, register_routes_with_config};
use oagw::config::OagwConfig;
use oagw::domain::model::PROTOCOL_HTTP;
use oagw::domain::sharing::FlatHierarchy;
use oagw::infra::obs::metrics::{LABEL_HOST, REQUESTS_IN_FLIGHT};
use oagw::infra::obs::{AUDIT_EVENT, EventClass, Observability, SUCCESS_SAMPLE_RATE};
use oagw::infra::storage::OagwStore;

const TENANT: Uuid = uuid::uuid!("00000000-0000-0000-0000-000000000060");

/// The alias the loopback stub upstream is stored under.
const ALIAS: &str = "streaming.internal";

/// The path the test routes match.
const ROUTE_PATH: &str = "/v1";

/// An address nothing in the test environment listens on, for the call that
/// must fail.
const DEAD_PORT: u16 = 1;

/// Every scope a management caller may need.
const ALL: &[&str] = &["*"];

/// The frame the relay test sends and expects back, byte for byte.
const FRAME: &[u8] = b"relay this frame\n";

/// The `Sec-WebSocket-Key` the RFC 6455 example handshake carries, whose accept
/// value is [`WS_ACCEPT`].
const WS_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";

/// The `Sec-WebSocket-Accept` of [`WS_KEY`], computed by the peer that accepts
/// the upgrade and relayed verbatim.
const WS_ACCEPT: &str = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";

/// Host OpenAPI registry double that records nothing.
#[derive(Default)]
struct NoopRegistry;

impl toolkit::api::OpenApiRegistry for NoopRegistry {
    fn register_operation(&self, _spec: &toolkit::api::operation_builder::OperationSpec) {}

    fn ensure_schema_raw(
        &self,
        name: &str,
        _schemas: Vec<(String, utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>)>,
    ) -> String {
        name.to_owned()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// The mounted router over a fresh store and the given configuration.
fn mounted_with(config: &OagwConfig) -> Router {
    register_routes_with_config(
        Router::new(),
        &NoopRegistry,
        Arc::new(OagwStore::new()),
        Arc::new(FlatHierarchy),
        config,
    )
    .expect("the test configuration builds the proxy client")
}

/// The plaintext-opting configuration the stub upstream needs.
///
/// `proxy_timeout_secs` is the idle window the relay arms, not a cap on the
/// whole exchange, so a short value keeps the timed-out tests quick.
fn proxy_config(timeout: u64) -> OagwConfig {
    OagwConfig {
        proxy_timeout_secs: timeout,
        allow_http_upstream: true,
        ..OagwConfig::default()
    }
}

/// A security context the host api-gateway would inject for `tenant`.
fn context(tenant: Uuid, scopes: &[&str]) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_type("user")
        .subject_tenant_id(tenant)
        .token_scopes(scopes.iter().map(|scope| (*scope).to_owned()).collect())
        .build()
        .expect("context builds")
}

/// Send a request of `method` with the given headers and body.
async fn call(
    router: Router,
    method: &str,
    uri: &str,
    caller: Option<SecurityContext>,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> axum::response::Response {
    let method = axum::http::Method::from_bytes(method.as_bytes()).expect("method");
    let mut builder = axum::http::Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    if let Some(caller) = caller {
        builder = builder.extension(caller);
    }
    router
        .oneshot(
            builder
                .body(Body::from(body.unwrap_or("").to_owned()))
                .expect("request builds"),
        )
        .await
        .expect("request serves")
}

/// The error-source header value of a response, if it carries one.
fn error_source(response: &axum::response::Response) -> Option<String> {
    response
        .headers()
        .get(ERROR_SOURCE_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// The value of a response header.
fn header_of(response: &axum::response::Response, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// The whole body of a response as a string.
async fn text(response: axum::response::Response) -> String {
    let bytes = Body::new(response)
        .collect()
        .await
        .expect("body collects")
        .to_bytes();
    String::from_utf8(bytes.to_vec()).expect("body is utf-8")
}

/// Assert the canonical problem contract of a gateway streaming failure.
async fn assert_problem(
    response: axum::response::Response,
    status: u16,
    type_suffix: &str,
    instance: &str,
) -> Value {
    // The whole response is read once, so a failing expectation reports the
    // document that was received rather than a moved-away response.
    let (parts, body) = response.into_parts();
    let bytes = Body::new(body)
        .collect()
        .await
        .expect("body collects")
        .to_bytes();
    let document: Value = serde_json::from_slice(&bytes).expect("body is a JSON document");
    assert_eq!(parts.status, StatusCode::from_u16(status).expect("status"), "{document}");
    assert_eq!(
        parts.headers.get(ERROR_SOURCE_HEADER).and_then(|value| value.to_str().ok()),
        Some(ERROR_SOURCE_GATEWAY),
        "{status} is classified as gateway-originated"
    );
    assert_eq!(
        document["type"],
        json!(format!("gts://gts.cf.core.errors.err.v1~cf.oagw.{type_suffix}")),
        "{document}"
    );
    assert_eq!(document["status"], json!(status), "{document}");
    assert_eq!(document["instance"], json!(instance), "{document}");
    document
}

/// The body of a plaintext stub upstream stored as `alias`.
fn stub_upstream(alias: &str, port: u16) -> Value {
    // `passthrough: all` is declared, because the schema default of
    // `headers.request.passthrough` is `none`: a gateway that declares no
    // header disposition forwards no client header at all.
    json!({
        "alias": alias,
        "server": {
            "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": port } ]
        },
        "protocol": PROTOCOL_HTTP,
        "headers": { "request": { "passthrough": "all" } }
    })
}

/// Create an upstream through the management API and return its identifier.
async fn create_upstream(router: &Router, body: Value) -> Uuid {
    let response = call(
        router.clone(),
        "POST",
        &format!("{MOUNT_ROOT}/upstreams"),
        Some(context(TENANT, ALL)),
        &[("content-type", "application/json")],
        Some(&body.to_string()),
    )
    .await;
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("the record is read");
    let record: Value = serde_json::from_slice(&bytes).expect("the record is a JSON document");
    Uuid::parse_str(record["id"].as_str().expect("the record carries an id")).expect("uuid")
}

/// The route body matching the forwarded method on `path`, with the `append`
/// suffix mode the proxy path forwards with.
fn route_body(upstream: Uuid, path: &str) -> Value {
    json!({
        "upstream_id": upstream,
        "match": { "http": { "methods": ["GET"], "path": path } }
    })
}

/// Create a route through the management API.
async fn create_route(router: &Router, body: Value) {
    let response = call(
        router.clone(),
        "POST",
        &format!("{MOUNT_ROOT}/routes"),
        Some(context(TENANT, ALL)),
        &[("content-type", "application/json")],
        Some(&body.to_string()),
    )
    .await;
    let status = response.status();
    assert_eq!(status, StatusCode::CREATED, "{}", text(response).await);
}

/// Seed the loopback stub and its route, and return the proxy path the route
/// serves.
///
/// The route matches the forwarded method on [`ROUTE_PATH`] with the `append`
/// suffix mode, so `/proxy/{alias}/v1/things` forwards `/v1/things`.
async fn stubbed(router: &Router, port: u16) -> String {
    let upstream = create_upstream(router, stub_upstream(ALIAS, port)).await;
    create_route(router, route_body(upstream, ROUTE_PATH)).await;
    format!("{MOUNT_ROOT}/proxy/{ALIAS}{ROUTE_PATH}")
}

/// Bind a loopback listener and return it with its port.
fn loopback() -> (TcpListener, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("the listener binds");
    let port = listener
        .local_addr()
        .expect("the listener has an address")
        .port();
    (listener, port)
}

/// Read the request head the gateway dialled, up to the blank line.
fn read_dial(stream: &mut std::net::TcpStream) -> String {
    let mut buffer = [0_u8; 8192];
    let mut head = Vec::new();
    loop {
        let taken = stream.read(&mut buffer).expect("the dial is readable");
        head.extend_from_slice(&buffer[..taken]);
        if head.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8(head).expect("the dial is utf-8")
}

/// Serve one `text/event-stream` response in two chunks, `gap` apart.
///
/// Returns the port the endpoint has to name; the thread answers the first
/// request it accepts and then exits.
fn event_stream_upstream(gap: u64) -> u16 {
    let (listener, port) = loopback();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("the gateway connects");
        read_dial(&mut stream);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                  cache-control: no-store\r\n\
                  transfer-encoding: chunked\r\n\r\n",
            )
            .expect("the head is written");
        for event in ["data: first\n\n", "data: second\n\n"] {
            let chunk = format!("{:x}\r\n{event}\r\n", event.len());
            stream
                .write_all(chunk.as_bytes())
                .expect("the chunk is written");
            stream.flush().expect("the chunk is flushed");
            std::thread::sleep(Duration::from_millis(gap));
        }
        stream.write_all(b"0\r\n\r\n").expect("the stream ends");
    });
    port
}

/// Serve a streamed response head and then close the connection without one
/// body byte, although the head declared a body of 128 bytes.
///
/// The declared length is what makes the truncation an error for the gateway
/// rather than a clean end of the body.
fn aborted_stream_upstream() -> u16 {
    let (listener, port) = loopback();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("the gateway connects");
        read_dial(&mut stream);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                  content-length: 128\r\n\r\n",
            )
            .expect("the head is written");
        stream.flush().expect("the head is flushed");
        drop(stream);
    });
    port
}


/// Serve a streamed response head and then stay silent.
///
/// The thread stops as soon as the gateway closes the connection, which the
/// idle window is what makes it do.
fn silent_stream_upstream() -> u16 {
    let (listener, port) = loopback();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("the gateway connects");
        read_dial(&mut stream);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                  transfer-encoding: chunked\r\n\r\n",
            )
            .expect("the head is written");
        stream.flush().expect("the head is flushed");
        let mut buffer = [0_u8; 512];
        while let Ok(taken) = stream.read(&mut buffer) {
            if taken == 0 {
                break;
            }
        }
    });
    port
}

/// Serve a streamed head and one chunk, and then stay silent.
///
/// The chunk is what commits the head: the gateway has a body byte in hand when
/// the silence begins, so the exchange is relayed and the idle window the relay
/// arms is what ends it.
fn dripping_then_silent_stream_upstream() -> u16 {
    let (listener, port) = loopback();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("the gateway connects");
        read_dial(&mut stream);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                  transfer-encoding: chunked\r\n\r\n",
            )
            .expect("the head is written");
        let event = "data: only\n\n";
        stream
            .write_all(format!("{:x}\r\n{event}\r\n", event.len()).as_bytes())
            .expect("the chunk is written");
        stream.flush().expect("the chunk is flushed");
        let mut buffer = [0_u8; 512];
        while let Ok(taken) = stream.read(&mut buffer) {
            if taken == 0 {
                break;
            }
        }
    });
    port
}

/// Serve streamed responses of two chunks `gap` apart, one after another.
///
/// The thread answers every request it accepts instead of one, because the close
/// a relay that ran to its clean end produces is a sampled line: a test that has
/// to read it drives the exchange more than once against the same listener.
fn dripping_stream_upstream(gap: u64) -> u16 {
    let (listener, port) = loopback();
    std::thread::spawn(move || {
        for accepted in listener.incoming() {
            let Ok(mut stream) = accepted else {
                break;
            };
            read_dial(&mut stream);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                      transfer-encoding: chunked\r\n\r\n",
                )
                .expect("the head is written");
            for event in ["data: first\n\n", "data: second\n\n"] {
                let chunk = format!("{:x}\r\n{event}\r\n", event.len());
                stream
                    .write_all(chunk.as_bytes())
                    .expect("the chunk is written");
                stream.flush().expect("the chunk is flushed");
                std::thread::sleep(Duration::from_millis(gap));
            }
            stream.write_all(b"0\r\n\r\n").expect("the stream ends");
        }
    });
    port
}

/// Serve a streamed head and one chunk, hold the exchange for `gap`
/// milliseconds and end it, tolerating a gateway that walked away.
///
/// The writes are allowed to fail, because the exchange this stub serves is
/// abandoned by its client before the upstream is done with it. Every accepted
/// connection is served on its own thread, because the hold is what this stub is
/// for: an exchange served while another one is still being held would wait for
/// it, and the interval its line reports would be the wait and not the client's
/// walk-away.
fn held_stream_upstream(gap: u64) -> u16 {
    let (listener, port) = loopback();
    std::thread::spawn(move || {
        for accepted in listener.incoming() {
            let Ok(mut stream) = accepted else {
                break;
            };
            std::thread::spawn(move || {
                read_dial(&mut stream);
                let head = b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                             transfer-encoding: chunked\r\n\r\n";
                let event = "data: first\n\n";
                let chunk = format!("{:x}\r\n{event}\r\n", event.len());
                let _ = stream.write_all(head).and_then(|()| stream.flush());
                let _ = stream
                    .write_all(chunk.as_bytes())
                    .and_then(|()| stream.flush());
                std::thread::sleep(Duration::from_millis(gap));
                let _ = stream.write_all(b"0\r\n\r\n");
            });
        }
    });
    port
}

/// Accept the upgrade with a `101` and then echo every byte the gateway relays.
fn echo_upstream() -> u16 {
    let (listener, port) = loopback();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("the gateway connects");
        read_dial(&mut stream);
        stream
            .write_all(
                format!(
                    "HTTP/1.1 101 Switching Protocols\r\n\
                     upgrade: websocket\r\n\
                     connection: Upgrade\r\n\
                     sec-websocket-accept: {WS_ACCEPT}\r\n\r\n"
                )
                .as_bytes(),
            )
            .expect("the 101 is written");
        stream.flush().expect("the 101 is flushed");
        let mut buffer = [0_u8; 1024];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(taken) => {
                    if stream.write_all(&buffer[..taken]).is_err() {
                        break;
                    }
                    let _ = stream.flush();
                }
            }
        }
    });
    port
}

/// Refuse the upgrade with a close-delimited `404` body.
fn refusing_upstream() -> u16 {
    let (listener, port) = loopback();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("the gateway connects");
        read_dial(&mut stream);
        stream
            .write_all(
                b"HTTP/1.1 404 Not Found\r\ncontent-type: application/json\r\n\r\n\
                  {\"error\":\"no route\"}",
            )
            .expect("the refusal is written");
    });
    port
}

/// Seed the loopback stub on a fresh router, serve that router on the loopback
/// interface with the security context the host api-gateway would inject, and
/// return the port the gateway listens on.
///
/// A real connection is what makes the request extensions carry the upgrade
/// handle: a `oneshot` call has no connection to upgrade.
async fn served_gateway(config: &OagwConfig, upstream_port: u16) -> u16 {
    let router = mounted_with(config).layer(middleware::from_fn(inject_context));
    stubbed(&router, upstream_port).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("the gateway listener binds");
    let port = listener
        .local_addr()
        .expect("the listener has an address")
        .port();
    tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("the gateway serves");
    });
    port
}

/// The middleware the host api-gateway stands for: it injects the resolved
/// security context the proxy handler reads from the request extensions.
async fn inject_context(request: Request, next: Next) -> AxumResponse {
    let (mut parts, body) = request.into_parts();
    parts.extensions.insert(context(TENANT, ALL));
    next.run(Request::from_parts(parts, body)).await
}

/// The raw upgrade request a WebSocket client sends, on the loopback gateway.
fn handshake_request(port: u16, path: &str) -> String {
    format!(
        "GET {path} HTTP/1.1\r\n\
         host: 127.0.0.1:{port}\r\n\
         upgrade: websocket\r\n\
         connection: Upgrade\r\n\
         sec-websocket-key: {WS_KEY}\r\n\
         sec-websocket-version: 13\r\n\r\n"
    )
}

/// Open a connection to the gateway, send `request` and read the response head.
async fn handshake(port: u16, request: &str) -> (tokio::net::TcpStream, String) {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("the gateway accepts");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("the request is written");
    let head = read_head(&mut stream).await;
    (stream, head)
}

/// Read a response head off the connection, up to the blank line.
async fn read_head<S: tokio::io::AsyncRead + Unpin>(stream: &mut S) -> String {
    let mut head = Vec::new();
    let mut scratch = [0_u8; 1024];
    loop {
        let taken = stream.read(&mut scratch).await.expect("the head is readable");
        head.extend_from_slice(&scratch[..taken]);
        if head.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8(head).expect("the head is utf-8")
}

// ---------- streamed response relay ----------

#[tokio::test]
async fn a_streamed_response_reaches_the_client_incrementally_and_unframed() {
    // Two events, 400 milliseconds apart: a gateway that buffers the body
    // would deliver the first frame only after the upstream closed the stream.
    let port = event_stream_upstream(400);
    let router = mounted_with(&proxy_config(10));
    let path = stubbed(&router, port).await;

    let started = Instant::now();
    let response = call(
        router,
        "GET",
        &path,
        Some(context(TENANT, ALL)),
        &[("accept", "text/event-stream")],
        None,
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header_of(&response, "content-type").as_deref(),
        Some("text/event-stream"),
        "the upstream content type is relayed, not recomputed"
    );
    assert_eq!(
        header_of(&response, "cache-control").as_deref(),
        Some("no-store"),
        "the upstream cache directive is preserved verbatim"
    );
    assert!(
        header_of(&response, "content-length").is_none(),
        "a streamed response carries no content length"
    );
    assert_eq!(error_source(&response).as_deref(), Some(ERROR_SOURCE_UPSTREAM));

    let mut events = response.into_body().into_data_stream();
    let first = events
        .next()
        .await
        .expect("the first frame arrives")
        .expect("the frame is readable");
    let first_after = started.elapsed();
    assert!(
        first_after < Duration::from_millis(250),
        "the first event arrived after {first_after:?}, so the gateway buffered"
    );
    let drained = async {
        while let Some(frame) = events.next().await {
            let _ = frame.expect("the frame is readable");
        }
    };
    tokio::time::timeout(Duration::from_secs(5), drained)
        .await
        .expect("the stream ends");
    assert!(
        started.elapsed() > Duration::from_millis(350),
        "the second event came from the open upstream exchange"
    );
    assert_eq!(
        String::from_utf8(first.to_vec()).expect("the frame is utf-8"),
        "data: first\n\n",
        "the chunk boundary the upstream wrote is preserved"
    );
}

#[tokio::test]
async fn an_upstream_that_dies_before_the_first_byte_becomes_an_aborted_problem() {
    // The head declares 128 bytes and the connection is closed without one of
    // them: the head was never committed, so the gateway answers with the
    // problem document and not with a truncated stream.
    let port = aborted_stream_upstream();
    let router = mounted_with(&proxy_config(10));
    let path = stubbed(&router, port).await;

    let response = call(
        router,
        "GET",
        &path,
        Some(context(TENANT, ALL)),
        &[("accept", "text/event-stream")],
        None,
    )
    .await;

    assert_problem(response, 502, "stream.aborted.v1", &path).await;
}

#[tokio::test]
async fn a_silent_upstream_ends_the_exchange_at_the_idle_window() {
    // The window the configuration carries is the idle window the relay arms:
    // one second here, so the exchange ends on the silence and not on a cap
    // over the whole request.
    let port = silent_stream_upstream();
    let router = mounted_with(&proxy_config(1));
    let path = stubbed(&router, port).await;

    let started = Instant::now();
    let response = call(
        router,
        "GET",
        &path,
        Some(context(TENANT, ALL)),
        &[("accept", "text/event-stream")],
        None,
    )
    .await;
    let answered = started.elapsed();

    assert_problem(response, 504, "timeout.idle.v1", &path).await;
    assert!(
        answered >= Duration::from_millis(900),
        "the window was not shortened: {answered:?}"
    );
    assert!(
        answered < Duration::from_secs(3),
        "the exchange ended after {answered:?}, which is no idle window"
    );
}

// ---------- upgrade relay ----------

#[tokio::test]
async fn an_established_upgrade_is_relayed_in_both_directions() {
    // The upstream accepts the handshake and echoes every byte back, so the
    // frames the client sends have to cross the relay twice to come back.
    let upstream_port = echo_upstream();
    let gateway_port = served_gateway(&proxy_config(10), upstream_port).await;
    let path = format!("{MOUNT_ROOT}/proxy/{ALIAS}{ROUTE_PATH}");

    let (mut client, head) = handshake(gateway_port, &handshake_request(gateway_port, &path)).await;

    assert!(
        head.starts_with("HTTP/1.1 101"),
        "the upgrade was not relayed: {head}"
    );
    assert!(
        head.to_ascii_lowercase()
            .contains(&format!("sec-websocket-accept: {}", WS_ACCEPT.to_ascii_lowercase())),
        "the accept value the upstream computed is relayed verbatim: {head}"
    );
    assert!(
        !head.to_ascii_lowercase().contains("content-length"),
        "the 101 head carries no framing of this gateway's own: {head}"
    );

    client
        .write_all(FRAME)
        .await
        .expect("the frame is written");
    let mut echoed = vec![0_u8; FRAME.len()];
    client
        .read_exact(&mut echoed)
        .await
        .expect("the echo arrives");
    assert_eq!(echoed, FRAME, "the relay moved the frame in both directions");

    // Half-closing the client is the disconnect of one side: the relay shuts
    // the other side down and the exchange ends without an error response.
    client.shutdown().await.expect("the client half closes");
    let mut rest = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut rest))
        .await
        .expect("the relay ends when the sides close")
        .expect("the relay drains");
}

#[tokio::test]
async fn an_upstream_refusal_of_the_upgrade_is_passed_through_untouched() {
    let port = refusing_upstream();
    let router = mounted_with(&proxy_config(10));
    let path = stubbed(&router, port).await;

    let response = call(
        router,
        "GET",
        &path,
        Some(context(TENANT, ALL)),
        &[
            ("upgrade", "websocket"),
            ("connection", "Upgrade"),
            ("sec-websocket-key", WS_KEY),
            ("sec-websocket-version", "13"),
        ],
        None,
    )
    .await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        error_source(&response).as_deref(),
        Some(ERROR_SOURCE_UPSTREAM),
        "a refusal is the upstream's answer, not the gateway's"
    );
    assert_eq!(
        header_of(&response, "content-type").as_deref(),
        Some("application/json"),
        "the refusal head is relayed as received"
    );
    assert_eq!(
        text(response).await,
        "{\"error\":\"no route\"}",
        "the refusal body is passed through"
    );
}

#[tokio::test]
async fn an_upgrade_that_cannot_be_dialled_is_mapped_onto_the_problem_document() {
    // Port 1 refuses every connection on the loopback interface, so the dial
    // itself fails before any status line is available to relay.
    let router = mounted_with(&proxy_config(10));
    let path = stubbed(&router, DEAD_PORT).await;

    let response = call(
        router,
        "GET",
        &path,
        Some(context(TENANT, ALL)),
        &[
            ("upgrade", "websocket"),
            ("connection", "Upgrade"),
            ("sec-websocket-key", WS_KEY),
            ("sec-websocket-version", "13"),
        ],
        None,
    )
    .await;

    let document = assert_problem(response, 502, "downstream.error.v1", &path).await;
    assert!(
        document["detail"].as_str().is_some_and(|detail| !detail.is_empty()),
        "the failure carries the mapped detail: {document}"
    );
}

#[tokio::test]
async fn a_streamed_request_whose_response_is_not_a_stream_stays_buffered() {
    // The two facts are independent: a client that asked for a stream gets the
    // buffered passthrough of a response that is not one.
    let (listener, port) = loopback();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("the gateway connects");
        read_dial(&mut stream);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                  content-length: 16\r\n\r\n{\"stream\":false}",
            )
            .expect("the response is written");
    });
    let router = mounted_with(&proxy_config(10));
    let path = stubbed(&router, port).await;

    let response = call(
        router,
        "GET",
        &path,
        Some(context(TENANT, ALL)),
        &[("accept", "text/event-stream")],
        None,
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header_of(&response, "content-length").as_deref(),
        Some("16"),
        "the buffered framing the upstream declared is relayed"
    );
    assert_eq!(text(response).await, "{\"stream\":false}");
}
// @cpt-end:cpt-cf-oagw-dod-stream-test-coverage:p2:inst-full


// ---------- the emission a streamed exchange closes with ----------

/// A streamed exchange is one emitted line, at the close the relay reports:
/// never one per chunk, and never before the stream ends.
///
/// The exchange read here is closed by the idle window before the upstream
/// produced a body byte, so its line is a failure class and is emitted
/// unsampled: one exchange is enough to read it, and the byte counts the line
/// carries are the ones the relay counted, read from the record the relay
/// advanced and not from a hand-built outcome.
#[tokio::test]
async fn a_streamed_exchange_emits_one_line_with_the_relayed_bytes() {
    let shared = Observability::shared();
    let ring = shared.writer().enable_capture();

    let port = silent_stream_upstream();
    let router = mounted_with(&proxy_config(1));
    // An alias of this test's own: the capture ring is process-global, so the
    // lines of the other streaming tests that run alongside it are in it too.
    let alias = "streamed-line.internal";
    let upstream = create_upstream(&router, stub_upstream(alias, port)).await;
    create_route(&router, route_body(upstream, ROUTE_PATH)).await;
    let path = format!("{MOUNT_ROOT}/proxy/{alias}{ROUTE_PATH}");
    // The seeding calls are answered before the exchange is read, so the ring
    // is emptied here and holds only the lines the streamed exchange produces.
    ring.lock().clear();

    let response = call(
        router,
        "GET",
        &path,
        Some(context(TENANT, ALL)),
        &[("accept", "text/event-stream")],
        None,
    )
    .await;
    assert_problem(response, 504, "timeout.idle.v1", &path).await;

    let started = Instant::now();
    let line = loop {
        let captured: Vec<String> = ring.lock().iter().cloned().collect();
        if let Some(line) = captured.iter().find(|line| line.contains(alias)) {
            break line.clone();
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "no line was emitted for the streamed exchange: {captured:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let line: Value = serde_json::from_str(&line).expect("the line is a JSON document");
    assert_eq!(line["event"], json!(AUDIT_EVENT), "{line}");
    assert_eq!(line["status"], json!(504), "{line}");
    assert_eq!(line["level"], json!("ERROR"), "the idle close is an error: {line}");
    assert_eq!(
        line["error_type"],
        json!("gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1"),
        "the close reason the relay recorded is the type the line reports: {line}"
    );
    // The relayed byte counts are the record's own: no body byte crossed in
    // either direction before the window fired, so both are the zero the relay
    // counted, and neither is the declared length of a head that was never
    // committed.
    assert_eq!(line["request_size"], json!(0), "{line}");
    assert_eq!(line["response_size"], json!(0), "{line}");
    // The window the relay armed is the interval the exchange took.
    assert!(
        line["duration_ms"].as_u64().unwrap_or_default() >= 900,
        "the duration is the idle window the relay armed: {line}"
    );
    assert_eq!(line["host"], json!(alias), "{line}");
    assert_eq!(line["path"], json!(ROUTE_PATH), "{line}");

    let captured: Vec<String> = ring.lock().iter().cloned().collect();
    let lines_of_the_exchange: Vec<String> = captured
        .iter()
        .filter(|l| l.contains(alias))
        .cloned()
        .collect();
    assert_eq!(
        lines_of_the_exchange.len(), 1,
        "a streamed exchange emits exactly one line: {lines_of_the_exchange:?}"
    );
}

/// The alias this test's stub upstream is stored under: the capture ring is
/// process-global, so the lines of the other streaming tests that run alongside
/// are in it too.
const DRIPPED_ALIAS: &str = "streamed-relay.internal";

/// The bytes the two dripped chunks carry, in relay order.
const DRIPPED_BYTES: usize = "data: first\n\n".len() + "data: second\n\n".len();

/// The number of exchanges the sampled slot is waited out over.
///
/// The fixed rate samples one successful line in a hundred, and the counter it
/// advances is the process-wide one, shared with the streaming tests that run
/// alongside. The counter is moved onto the emitting slot before every attempt,
/// so the first one is the line that is read; the remaining attempts are only
/// for a classification a concurrent test landed on the same counter.
const SAMPLED_ATTEMPTS: usize = 4;

/// Move the process-wide success counter onto the slot the fixed rate emits, so
/// the next successful close is the line a test reads.
///
/// The counter is advanced through the sampler the way the emission classifies a
/// success, so the counts the sampling arithmetic reads stay consistent: a clean
/// close is an emitted line one time in a hundred, and a relay that ran to its
/// end is a clean close.
fn advance_to_the_sampled_slot(shared: &Arc<Observability>) {
    let seen = shared.sampler().success_seen();
    let ahead = SUCCESS_SAMPLE_RATE - (seen % SUCCESS_SAMPLE_RATE);
    for _ in 0..ahead.saturating_sub(1) {
        let _ = shared.sampler().decide(EventClass::Success);
    }
}

/// The capture ring the writer hands out, as a test reads it.
type Ring = Arc<parking_lot::Mutex<std::collections::VecDeque<String>>>;

/// The lines the ring holds for `alias`.
fn lines_of(ring: &Ring, alias: &str) -> Vec<String> {
    ring.lock()
        .iter()
        .filter(|line| line.contains(alias))
        .cloned()
        .collect()
}

/// The first captured line `alias` produced, polled from the ring.
fn line_of(ring: &Ring, alias: &str) -> Option<String> {
    lines_of(ring, alias).into_iter().next()
}

/// Drive `exchange` until the sampled slot emits the line `alias` produced, and
/// return that line with the count of lines the exchange that produced it added.
///
/// The fixed rate samples one successful line in a hundred, and the counter it
/// advances is the process-wide one, shared with the tests that run alongside:
/// the counter is moved onto the emitting slot before every attempt, so the
/// first attempt is the line that is read and the following ones only cover a
/// classification a concurrent test landed on the same counter. The count is the
/// one exchange's own — the lines that appeared while it ran — so a second close
/// of the same exchange would read as two, and an attempt whose slot a
/// concurrent test took adds nothing at all.
async fn sampled_exchange<F, Fut>(
    shared: &Arc<Observability>,
    ring: &Ring,
    alias: &str,
    exchange: F,
) -> (String, usize)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    for _ in 0..SAMPLED_ATTEMPTS {
        advance_to_the_sampled_slot(shared);
        let before = lines_of(ring, alias).len();
        exchange().await;
        let lines = lines_of(ring, alias);
        if lines.len() > before {
            return (
                lines.last().cloned().unwrap_or_default(),
                lines.len() - before,
            );
        }
    }
    panic!(
        "the close the exchange produced was never emitted: {:?}",
        lines_of(ring, alias)
    );
}

// ---------- the close a relayed exchange is emitted at ----------

/// A relay whose head was committed runs to its end after the handoff, so its
/// line is emitted there and not at the handoff: one line, whose duration and
/// byte counts are the whole relay's and whose error type is the clean close's.
#[tokio::test]
async fn a_streamed_exchange_that_relays_to_the_upstreams_end_emits_one_line_at_that_close() {
    let shared = Observability::shared();
    let ring = shared.writer().enable_capture();

    let port = dripping_stream_upstream(40);
    let router = mounted_with(&proxy_config(10));
    let upstream = create_upstream(&router, stub_upstream(DRIPPED_ALIAS, port)).await;
    create_route(&router, route_body(upstream, ROUTE_PATH)).await;
    let path = format!("{MOUNT_ROOT}/proxy/{DRIPPED_ALIAS}{ROUTE_PATH}");
    // The seeding calls are answered before the exchange is read, so the ring
    // is emptied here and holds only the lines the streamed exchanges produce.
    ring.lock().clear();

    // The head is committed once the first chunk is in hand, so the body that
    // arrives here is the relay running: it is read to the end, which is what
    // lets the relay reach the close it records.
    let (line, lines) = sampled_exchange(&shared, &ring, DRIPPED_ALIAS, || async {
        let response = call(
            router.clone(),
            "GET",
            &path,
            Some(context(TENANT, ALL)),
            &[("accept", "text/event-stream")],
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = text(response).await;
        assert_eq!(body.len(), DRIPPED_BYTES, "both chunks were relayed: {body}");
    })
    .await;
    assert_eq!(
        lines, 1,
        "the exchange that closed added one line, and not a second one for the handoff"
    );

    let line: Value = serde_json::from_str(&line).expect("the line is a JSON document");
    assert_eq!(line["event"], json!(AUDIT_EVENT), "{line}");
    assert_eq!(line["status"], json!(200), "{line}");
    assert_eq!(
        line["level"], json!("INFO"),
        "an exchange the upstream ended is served: {line}"
    );
    assert_eq!(
        line["error_type"], Value::Null,
        "a clean close carries no error type: {line}"
    );
    // The byte count is the relay's own: both chunks, and not the handoff's
    // unknown size and not a chunk the head declared.
    assert_eq!(line["response_size"], json!(DRIPPED_BYTES), "{line}");
    // Two chunks 40 milliseconds apart is the interval the relay ran, so the
    // duration the line reports covers it and not the handoff alone.
    assert!(
        line["duration_ms"].as_u64().unwrap_or_default() >= 80,
        "the duration is the whole relay: {line}"
    );
    assert_eq!(line["host"], json!(DRIPPED_ALIAS), "{line}");
}

/// An upstream that goes silent after the head ends the exchange on the idle
/// window the relay arms, and the line the relay emits there carries the close
/// reason, the bytes it relayed before the silence and the whole interval.
#[tokio::test]
async fn an_upstream_that_goes_silent_after_the_head_ends_the_exchange_at_the_idle_window() {
    let shared = Observability::shared();
    let ring = shared.writer().enable_capture();

    let port = dripping_then_silent_stream_upstream();
    let router = mounted_with(&proxy_config(1));
    // An alias of this test's own: the ring is process-global.
    let alias = "streamed-idle.internal";
    let upstream = create_upstream(&router, stub_upstream(alias, port)).await;
    create_route(&router, route_body(upstream, ROUTE_PATH)).await;
    let path = format!("{MOUNT_ROOT}/proxy/{alias}{ROUTE_PATH}");
    ring.lock().clear();

    let started = Instant::now();
    let response = call(
        router,
        "GET",
        &path,
        Some(context(TENANT, ALL)),
        &[("accept", "text/event-stream")],
        None,
    )
    .await;
    let head_at = started.elapsed();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the head was committed before the silence began"
    );
    assert_eq!(error_source(&response).as_deref(), Some(ERROR_SOURCE_UPSTREAM));
    let mut events = response.into_body().into_data_stream();
    let first = events
        .next()
        .await
        .expect("the chunk that committed the head arrives")
        .expect("the chunk is readable");
    assert_eq!(
        String::from_utf8(first.to_vec()).expect("the chunk is utf-8"),
        "data: only\n\n"
    );
    assert!(
        head_at < Duration::from_millis(900),
        "the head arrived before the idle window fired: {head_at:?}"
    );
    let drained = async {
        while let Some(frame) = events.next().await {
            let _ = frame.expect("the frame is readable");
        }
    };
    tokio::time::timeout(Duration::from_secs(5), drained)
        .await
        .expect("the body ends on the idle window");

    let line = loop {
        if let Some(line) = line_of(&ring, alias) {
            break line;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "no line was emitted for the relayed exchange: {:?}",
            ring.lock(),
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let line: Value = serde_json::from_str(&line).expect("the line is a JSON document");
    assert_eq!(line["status"], json!(200), "the head the client holds: {line}");
    assert_eq!(
        line["level"], json!("ERROR"),
        "the idle close is an error: {line}"
    );
    assert_eq!(
        line["error_type"],
        json!("gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1"),
        "the close reason the relay recorded is the type the line reports: {line}"
    );
    // The chunk the relay forwarded before the silence is what the line counts.
    assert_eq!(line["response_size"], json!("data: only\n\n".len()), "{line}");
    // The interval is the relay's: the head was committed first and the window
    // ran out after it.
    assert!(
        line["duration_ms"].as_u64().unwrap_or_default() >= 900,
        "the duration is the whole relay, the idle window included: {line}"
    );
    assert_eq!(line["host"], json!(alias), "{line}");

    let lines_of_the_exchange: Vec<String> = ring
        .lock()
        .iter()
        .filter(|l| l.contains(alias))
        .cloned()
        .collect();
    assert_eq!(
        lines_of_the_exchange.len(), 1,
        "a streamed exchange emits exactly one line: {lines_of_the_exchange:?}"
    );
}

/// The in-flight accounting follows the relay and not the handoff: an exchange
/// whose body is still streaming is open, and it is returned when the relay ends.
#[tokio::test]
async fn an_exchange_that_is_being_relayed_is_in_flight_until_the_relay_ends() {
    let shared = Observability::shared();
    let metrics = Arc::clone(shared.metrics());

    // A gap the test can hold the relay open across: the second chunk is still
    // on its way while the in-flight series is read.
    let port = dripping_stream_upstream(300);
    let router = mounted_with(&proxy_config(10));
    let alias = "streamed-inflight.internal";
    let upstream = create_upstream(&router, stub_upstream(alias, port)).await;
    create_route(&router, route_body(upstream, ROUTE_PATH)).await;
    let path = format!("{MOUNT_ROOT}/proxy/{alias}{ROUTE_PATH}");
    let open = [(LABEL_HOST, alias)];

    let started = Instant::now();
    let response = call(
        router,
        "GET",
        &path,
        Some(context(TENANT, ALL)),
        &[("accept", "text/event-stream")],
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut events = response.into_body().into_data_stream();
    let first = events
        .next()
        .await
        .expect("the first chunk arrives")
        .expect("the chunk is readable");
    assert_eq!(
        String::from_utf8(first.to_vec()).expect("the chunk is utf-8"),
        "data: first\n\n"
    );
    assert!(
        started.elapsed() < Duration::from_millis(250),
        "the first chunk arrived before the second one: {:?}",
        started.elapsed(),
    );

    // The handler returned the head, the upstream exchange is still open and the
    // relay is holding it: the gauge says so.
    let mid_flight = metrics.gauge(REQUESTS_IN_FLIGHT, &open).expect("the series exists");
    assert!(
        (mid_flight - 1.0).abs() < 1e-9,
        "a relayed exchange is in flight while its body streams: {mid_flight}"
    );

    let drained = async {
        while let Some(frame) = events.next().await {
            let _ = frame.expect("the frame is readable");
        }
    };
    tokio::time::timeout(Duration::from_secs(5), drained)
        .await
        .expect("the relay ends");

    let ended = metrics.gauge(REQUESTS_IN_FLIGHT, &open).expect("the series survives");
    assert!(
        (ended).abs() < 1e-9,
        "the in-flight series returns to zero once the relay ended: {ended}"
    );
}

/// The exchange one client exchange runs: the head, the in-flight reading while
/// the relay still holds the exchange, then the client walking away, which drops
/// the body and with it the relay that owed the close.
async fn abandon_a_relayed_exchange(
    router: &Router,
    path: &str,
    shared: &Arc<Observability>,
    alias: &str,
) {
    let metrics = Arc::clone(shared.metrics());
    let open = [(LABEL_HOST, alias)];

    let response = call(
        router.clone(),
        "GET",
        path,
        Some(context(TENANT, ALL)),
        &[("accept", "text/event-stream")],
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut events = response.into_body().into_data_stream();
    let first = events
        .next()
        .await
        .expect("the first chunk arrives")
        .expect("the chunk is readable");
    assert_eq!(
        String::from_utf8(first.to_vec()).expect("the chunk is utf-8"),
        "data: first\n\n"
    );
    let mid_flight = metrics.gauge(REQUESTS_IN_FLIGHT, &open).expect("the series exists");
    assert!(
        (mid_flight - 1.0).abs() < 1e-9,
        "the exchange is still open while the relay holds it: {mid_flight}"
    );

    // The client is gone: the body is dropped, and with it the relay that owed
    // the close.
    drop(events);
}

/// A client that walks away from a relayed exchange ends it on the relay's drop,
/// and that close is the exchange's one line, with the in-flight accounting
/// returned with it.
#[tokio::test]
async fn a_client_that_walks_away_from_a_relayed_exchange_still_leaves_one_line() {
    let shared = Observability::shared();
    let ring = shared.writer().enable_capture();
    let metrics = Arc::clone(shared.metrics());

    // The upstream holds the exchange for a second after the first chunk, so the
    // close the line reports is the client's and not the upstream's end.
    let port = held_stream_upstream(1_000);
    let router = mounted_with(&proxy_config(10));
    let alias = "streamed-gone.internal";
    let upstream = create_upstream(&router, stub_upstream(alias, port)).await;
    create_route(&router, route_body(upstream, ROUTE_PATH)).await;
    let path = format!("{MOUNT_ROOT}/proxy/{alias}{ROUTE_PATH}");
    let open = [(LABEL_HOST, alias)];
    ring.lock().clear();

    // A disconnect is a served exchange to the audit line, so its close is a
    // success the fixed rate samples: the attempts read below cover a slot a
    // concurrent test consumed.
    let (line, lines) = sampled_exchange(&shared, &ring, alias, || async {
        abandon_a_relayed_exchange(&router, &path, &shared, alias).await;
    })
    .await;
    assert_eq!(
        lines, 1,
        "the abandoned exchange added one line, and not a second one for the handoff"
    );

    let line: Value = serde_json::from_str(&line).expect("the line is a JSON document");
    assert_eq!(line["status"], json!(200), "the head the client holds: {line}");
    assert_eq!(line["level"], json!("INFO"), "the disconnect is not an upstream failure: {line}");
    assert_eq!(
        line["error_type"], Value::Null,
        "a client that walked away carries no error type: {line}"
    );
    assert_eq!(line["response_size"], json!("data: first\n\n".len()), "{line}");
    assert!(
        line["duration_ms"].as_u64().unwrap_or_default() < 1_000,
        "the close is the client's, not the upstream's end: {line}"
    );
    assert_eq!(line["host"], json!(alias), "{line}");

    let released = metrics.gauge(REQUESTS_IN_FLIGHT, &open).expect("the series survives");
    assert!(
        (released).abs() < 1e-9,
        "the relay's drop returned the in-flight accounting: {released}"
    );
}
