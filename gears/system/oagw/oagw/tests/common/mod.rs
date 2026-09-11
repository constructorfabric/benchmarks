//! Shared fixtures for the OAGW integration tests.
//!
//! The mock upstream is a raw `tokio` listener rather than a framework: the
//! tests need to exercise chunked bodies, server-sent events and a protocol
//! upgrade, and a hand-written responder is the only way to control the bytes
//! on the wire that precisely.

#![allow(dead_code, reason = "each integration test binary uses a subset")]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::response::Response;
use http::{Request, StatusCode};
use oagw::test_utils::{HarnessBuilder, TestHarness, security_context};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tower::ServiceExt;
use uuid::Uuid;

pub const HTTP_PROTOCOL: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

/// One recorded inbound request at the mock upstream.
#[derive(Debug, Clone, Default)]
pub struct RecordedRequest {
    pub head: String,
    pub body: String,
}

impl RecordedRequest {
    /// Value of `name`, matched case-insensitively.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<String> {
        self.head
            .lines()
            .skip(1)
            .filter_map(|line| line.split_once(':'))
            .find(|(key, _)| key.trim().eq_ignore_ascii_case(name))
            .map(|(_, value)| value.trim().to_owned())
    }

    /// The request line, e.g. `GET /v1/chat HTTP/1.1`.
    #[must_use]
    pub fn request_line(&self) -> &str {
        self.head.lines().next().unwrap_or_default()
    }

    /// The request target from the request line.
    #[must_use]
    pub fn target(&self) -> String {
        self.request_line()
            .split_whitespace()
            .nth(1)
            .unwrap_or_default()
            .to_owned()
    }
}

/// How the mock upstream answers.
#[derive(Clone)]
pub enum MockBehavior {
    /// A fixed status, content type and body.
    Fixed {
        status: u16,
        content_type: &'static str,
        body: String,
    },
    /// A `text/event-stream` that emits `events`, pausing `gap` between them.
    Sse { events: Vec<String>, gap: Duration },
    /// Wait `delay` before answering — for timeout coverage.
    Slow { delay: Duration },
    /// Complete a WebSocket handshake and echo every subsequent byte.
    WebSocketEcho,
    /// Refuse an upgrade with an ordinary response.
    WebSocketRefused,
    /// Close the connection without answering.
    Hangup,
}

/// A minimal HTTP/1.1 upstream under the test's control.
pub struct MockUpstream {
    pub addr: SocketAddr,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl MockUpstream {
    /// Bind on an ephemeral loopback port and serve `behavior` on every
    /// connection until the test drops.
    pub async fn start(behavior: MockBehavior) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
        let addr = listener.local_addr().expect("mock addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&requests);

        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let behavior = behavior.clone();
                let recorder = Arc::clone(&recorder);
                tokio::spawn(async move {
                    let _ = serve_connection(stream, behavior, recorder).await;
                });
            }
        });

        Self { addr, requests }
    }

    /// Everything the upstream has seen so far.
    pub async fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().await.clone()
    }

    /// The most recent request, panicking when there is none.
    pub async fn last_request(&self) -> RecordedRequest {
        self.requests
            .lock()
            .await
            .last()
            .cloned()
            .expect("the upstream should have been called")
    }

    #[must_use]
    pub fn port(&self) -> u16 {
        self.addr.port()
    }
}

async fn serve_connection(
    mut stream: TcpStream,
    behavior: MockBehavior,
    recorder: Arc<Mutex<Vec<RecordedRequest>>>,
) -> std::io::Result<()> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];

    // Read the head.
    let head_end = loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(());
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = buffer
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
        {
            break position + 4;
        }
    };

    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let content_length = head
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);

    let mut body = buffer[head_end..].to_vec();
    while body.len() < content_length {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }

    recorder.lock().await.push(RecordedRequest {
        head: head.clone(),
        body: String::from_utf8_lossy(&body).into_owned(),
    });

    match behavior {
        MockBehavior::Fixed {
            status,
            content_type,
            body,
        } => {
            let response = format!(
                "HTTP/1.1 {status} OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await?;
        }
        MockBehavior::Sse { events, gap } => {
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                      Cache-Control: no-cache\r\nConnection: close\r\n\r\n",
                )
                .await?;
            stream.flush().await?;
            for event in events {
                stream
                    .write_all(format!("data: {event}\n\n").as_bytes())
                    .await?;
                stream.flush().await?;
                tokio::time::sleep(gap).await;
            }
        }
        MockBehavior::Slow { delay } => {
            tokio::time::sleep(delay).await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await?;
        }
        MockBehavior::WebSocketEcho => {
            let accept = websocket_accept(&head);
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\
                         Connection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await?;
            stream.flush().await?;
            // Opaque byte echo — the proxy must not interpret the frames.
            loop {
                let read = stream.read(&mut chunk).await?;
                if read == 0 {
                    break;
                }
                stream.write_all(&chunk[..read]).await?;
                stream.flush().await?;
            }
        }
        MockBehavior::WebSocketRefused => {
            let body = "{\"error\":\"upgrade refused\"}";
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 426 Upgrade Required\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await?;
        }
        MockBehavior::Hangup => {}
    }

    stream.flush().await?;
    Ok(())
}

/// RFC 6455 handshake accept value for the key in `head`.
fn websocket_accept(head: &str) -> String {
    const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
    let key = head
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("sec-websocket-key"))
        .map(|(_, value)| value.trim().to_owned())
        .unwrap_or_default();
    // The tests only assert the header is present and stable, so a digest that
    // is deterministic in the key is enough here.
    let mut digest: u64 = 1469598103934665603;
    for byte in format!("{key}{GUID}").bytes() {
        digest ^= u64::from(byte);
        digest = digest.wrapping_mul(1099511628211);
    }
    format!("{digest:016x}")
}

/// A harness plus the tenant its requests run as.
pub struct Fixture {
    pub harness: TestHarness,
    pub tenant: Uuid,
}

impl Fixture {
    #[must_use]
    pub fn new() -> Self {
        Self::with_builder(HarnessBuilder::new())
    }

    #[must_use]
    pub fn with_builder(builder: HarnessBuilder) -> Self {
        Self {
            harness: builder.build(),
            tenant: Uuid::new_v4(),
        }
    }

    #[must_use]
    pub fn router(&self) -> Router {
        self.harness.router(security_context(self.tenant))
    }

    /// Router acting as `tenant`, for cross-tenant assertions.
    #[must_use]
    pub fn router_as(&self, tenant: Uuid) -> Router {
        self.harness.router(security_context(tenant))
    }

    /// Send `request` through a freshly cloned router.
    pub async fn send(&self, request: Request<Body>) -> Response {
        self.router()
            .oneshot(request)
            .await
            .expect("the router is infallible")
    }

    /// Send `request` as `tenant`.
    pub async fn send_as(&self, tenant: Uuid, request: Request<Body>) -> Response {
        self.router_as(tenant)
            .oneshot(request)
            .await
            .expect("the router is infallible")
    }

    /// `POST` a JSON body and return `(status, parsed body)`.
    pub async fn post_json(&self, path: &str, body: Value) -> (StatusCode, Value) {
        let response = self.send(json_request("POST", path, &body)).await;
        split(response).await
    }

    /// `PUT` a JSON body and return `(status, parsed body)`.
    pub async fn put_json(&self, path: &str, body: Value) -> (StatusCode, Value) {
        let response = self.send(json_request("PUT", path, &body)).await;
        split(response).await
    }

    /// `GET` and return `(status, parsed body)`.
    pub async fn get(&self, path: &str) -> (StatusCode, Value) {
        let response = self.send(empty_request("GET", path)).await;
        split(response).await
    }

    /// `DELETE` and return the status.
    pub async fn delete(&self, path: &str) -> StatusCode {
        self.send(empty_request("DELETE", path)).await.status()
    }

    /// Create an upstream, asserting it was accepted.
    pub async fn create_upstream(&self, body: Value) -> Value {
        let (status, value) = self.post_json("/oagw/v1/upstreams", body).await;
        assert_eq!(status, StatusCode::CREATED, "create upstream: {value}");
        value
    }

    /// Create a route, asserting it was accepted.
    pub async fn create_route(&self, body: Value) -> Value {
        let (status, value) = self.post_json("/oagw/v1/routes", body).await;
        assert_eq!(status, StatusCode::CREATED, "create route: {value}");
        value
    }

    /// Wire an upstream + catch-all route at `alias` pointing at `port`.
    pub async fn wire_upstream(&self, alias: &str, port: u16, methods: Value) -> Uuid {
        let upstream = self
            .create_upstream(serde_json::json!({
                "alias": alias,
                "server": {"endpoints": [{"scheme": "http", "host": "127.0.0.1", "port": port}]},
                "protocol": HTTP_PROTOCOL,
            }))
            .await;
        let id = Uuid::parse_str(upstream["id"].as_str().expect("id")).expect("uuid");
        self.create_route(serde_json::json!({
            "upstream_id": id,
            "match": {"http": {"methods": methods, "path": "/"}},
        }))
        .await;
        id
    }
}

impl Default for Fixture {
    fn default() -> Self {
        Self::new()
    }
}

/// Build a JSON request.
#[must_use]
pub fn json_request(method: &str, path: &str, body: &Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("valid request")
}

/// Build a bodyless request.
#[must_use]
pub fn empty_request(method: &str, path: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .body(Body::empty())
        .expect("valid request")
}

/// Split a response into its status and parsed JSON body.
pub async fn split(response: Response) -> (StatusCode, Value) {
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .expect("body");
    if bytes.is_empty() {
        return (status, Value::Null);
    }
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    (status, value)
}

/// Read a response body as text.
pub async fn body_text(response: Response) -> String {
    let bytes = to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .expect("body");
    String::from_utf8_lossy(&bytes).into_owned()
}
