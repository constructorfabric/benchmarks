//! Full-stack integration of the `oagw` gear (PRD.md §5.4, §8, §9).
//!
//! The management API, the plain proxy path, the SSE passthrough, the
//! WebSocket tunnel and the built-in CORS handler (ADR-0004) are driven
//! through the composed router. The upstream is a real local `axum` server so
//! streaming and upgrades behave like they do in production.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::too_many_lines,
    clippy::items_after_statements,
    clippy::doc_markdown,
    clippy::significant_drop_tightening,
    clippy::cognitive_complexity
)]

use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::ws::{WebSocket, WebSocketUpgrade};
use axum::http::{Request, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use bytes::Bytes;
use futures_util::SinkExt as _;
use futures_util::StreamExt;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use toolkit::api::OpenApiRegistryImpl;
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

use oagw::api::rest::routes::register_routes;
use oagw::config::OagwConfig;
use oagw::domain::hierarchy::StaticTenantHierarchy;
use oagw::domain::services::OagwService;

/// How long a test waits for an answer before giving up.
const BUDGET: Duration = Duration::from_secs(15);

/// A router bound to a fresh service, with helpers to configure it.
struct Harness {
    router: Router,
    tenant: Uuid,
}

/// A proxied response, reduced to what the assertions need.
struct Sent {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Vec<u8>,
}

impl Sent {
    fn problem(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    fn header(&self, name: &str) -> Option<String> {
        self.headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    }
}

impl Harness {
    fn with(config: OagwConfig) -> Self {
        let service = std::sync::Arc::new(OagwService::new(
            config,
            std::sync::Arc::new(StaticTenantHierarchy::default()),
        ));
        let router = register_routes(Router::new(), &OpenApiRegistryImpl::new(), service)
            .expect("data plane composes");
        Self {
            router,
            tenant: Uuid::new_v4(),
        }
    }

    fn plain() -> Self {
        Self::with(OagwConfig {
            proxy_timeout_secs: 5,
            allow_http_upstream: true,
            ..OagwConfig::default()
        })
    }

    fn ctx(&self) -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::new_v4())
            .subject_tenant_id(self.tenant)
            .build()
            .expect("context")
    }

    /// Serves the router on a real socket, with the security context the
    /// platform's authentication middleware would provide.
    async fn served(self) -> ServedGateway {
        let tenant = self.tenant;
        let router = self.router.layer(axum::middleware::from_fn(
            move |request: Request<Body>, next: Next| {
                let tenant = tenant;
                async move {
                    let mut request = request;
                    request.extensions_mut().insert(
                        SecurityContext::builder()
                            .subject_id(Uuid::new_v4())
                            .subject_tenant_id(tenant)
                            .build()
                            .expect("context"),
                    );
                    next.run(request).await
                }
            },
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let task =
            tokio::spawn(async move { axum::serve(listener, router).await.expect("served") });
        ServedGateway { addr, _task: task }
    }

    async fn send(
        &self,
        method: &str,
        uri: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Sent {
        let mut builder = Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let request = match body {
            Some(bytes) => builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(bytes.to_vec()))
                .expect("request"),
            None => builder.body(Body::empty()).expect("request"),
        };
        let mut request = request;
        request.extensions_mut().insert(self.ctx());
        let response = tokio::time::timeout(BUDGET, self.router.clone().oneshot(request))
            .await
            .expect("served within the budget")
            .expect("served");
        let status = response.status();
        let headers = response.headers().clone();
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes()
            .to_vec();
        Sent {
            status,
            headers,
            body,
        }
    }

    async fn send_json(&self, method: &str, uri: &str, body: Value) -> Sent {
        self.send(
            method,
            uri,
            &[("content-type", "application/json")],
            Some(serde_json::to_vec(&body).unwrap().as_slice()),
        )
        .await
    }

    /// Creates an upstream whose only endpoint is `host:port` and returns its
    /// GTS id.
    async fn upstream(&self, alias: &str, addr: SocketAddr, extra: Value) -> String {
        let mut body = json!({
            "alias": alias,
            "server": { "endpoints": [
                { "scheme": "http", "host": addr.ip().to_string(), "port": addr.port() }
            ] }
        });
        merge(&mut body, extra);
        let sent = self.send_json("POST", "/oagw/v1/upstreams", body).await;
        assert_eq!(
            sent.status,
            StatusCode::CREATED,
            "create upstream: {}",
            sent.problem()
        );
        sent.problem()["id"]
            .as_str()
            .expect("upstream id")
            .to_owned()
    }

    /// Creates a route on `upstream_id` and returns its GTS id.
    async fn route(&self, upstream_id: &str, path: &str, methods: &[&str]) -> String {
        let body = json!({
            "upstream_id": upstream_id,
            "match": { "http": {
                "methods": methods,
                "path": path,
            } }
        });
        let sent = self.send_json("POST", "/oagw/v1/routes", body).await;
        assert_eq!(
            sent.status,
            StatusCode::CREATED,
            "create route: {}",
            sent.problem()
        );
        sent.problem()["id"].as_str().expect("route id").to_owned()
    }
}

/// A live gateway on a local port, for the tests that need a real socket.
struct ServedGateway {
    addr: SocketAddr,
    _task: tokio::task::JoinHandle<()>,
}

impl ServedGateway {
    fn stop(self) {
        self._task.abort();
    }
}

/// Recursively merges `extra` into `base`.
fn merge(base: &mut Value, extra: Value) {
    match (base, extra) {
        (Value::Object(base), Value::Object(extra)) => {
            for (key, value) in extra {
                merge(base.entry(key).or_insert(Value::Null), value);
            }
        }
        (base, extra) => *base = extra,
    }
}

/// A local upstream that streams SSE events and echoes WebSocket frames.
struct MockUpstream {
    addr: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl MockUpstream {
    /// Serves `/events` (three delayed SSE events) and `/ws` (an echo).
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let app = Router::new()
            .route("/events", get(sse_events))
            .route("/ws", get(ws_echo))
            .fallback(anything);
        let task = tokio::spawn(async move { axum::serve(listener, app).await.expect("served") });
        Self { addr, task }
    }

    fn stop(self) {
        self.task.abort();
    }
}

/// Three SSE events, one every 60 ms: a gateway that buffers would deliver a
/// single frame.
async fn sse_events() -> impl IntoResponse {
    let stream = futures_util::stream::unfold(0u32, |index| async move {
        if index >= 3 {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(60)).await;
        let chunk = Bytes::from(format!("event: tick\ndata: {index}\n\n"));
        Some((Ok::<Bytes, std::convert::Infallible>(chunk), index + 1))
    });
    (
        [(header::CONTENT_TYPE, "text/event-stream")],
        Body::from_stream(stream),
    )
}

/// The catch-all answer of the mock upstream.
async fn anything() -> impl IntoResponse {
    ([("x-upstream", "mock")], "mocked")
}

/// An echo socket: whatever the caller sends comes back until it leaves.
async fn ws_echo(upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(|socket: WebSocket| async move {
        let (mut sender, mut receiver) = socket.split();
        while let Some(Ok(message)) = receiver.next().await {
            if sender.send(message).await.is_err() {
                break;
            }
        }
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn management_crud_configures_a_working_proxy() {
    let harness = Harness::plain();
    let upstream = MockUpstream::start().await;
    let id = harness.upstream("echo", upstream.addr, json!({})).await;
    harness.route(&id, "/v1", &["GET"]).await;

    let listed = harness.send("GET", "/oagw/v1/upstreams", &[], None).await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.problem());
    assert_eq!(listed.problem()["items"].as_array().map(Vec::len), Some(1));

    let proxied = harness
        .send("GET", "/oagw/v1/proxy/echo/v1/things?q=1", &[], None)
        .await;
    assert_eq!(proxied.status, StatusCode::OK, "{}", proxied.problem());
    assert_eq!(
        proxied.header("x-oagw-error-source").as_deref(),
        Some("upstream")
    );
    upstream.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_sse_stream_is_forwarded_event_by_event() {
    let harness = Harness::plain();
    let upstream = MockUpstream::start().await;
    let id = harness.upstream("events", upstream.addr, json!({})).await;
    harness.route(&id, "/events", &["GET"]).await;

    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/events/events")
        .header("accept", "text/event-stream")
        .body(Body::empty())
        .expect("request");
    let mut request = request;
    request.extensions_mut().insert(harness.ctx());
    let response = tokio::time::timeout(BUDGET, harness.router.clone().oneshot(request))
        .await
        .expect("served")
        .expect("served");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream"),
        "the media type passes through"
    );

    let mut frames = 0;
    let mut payload = String::new();
    let mut body = response.into_body();
    let started = std::time::Instant::now();
    loop {
        match tokio::time::timeout(Duration::from_secs(5), body.frame()).await {
            Ok(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    payload.push_str(&String::from_utf8_lossy(data));
                    frames += 1;
                }
            }
            Ok(Some(Err(error))) => panic!("stream error: {error}"),
            Ok(None) | Err(_) => break,
        }
    }
    assert!(started.elapsed() < BUDGET, "the stream ended in time");
    assert_eq!(
        payload.matches("event: tick").count(),
        3,
        "all events arrived: {payload}"
    );
    assert!(
        frames >= 2,
        "the gateway must not buffer an SSE response: {frames} frames for {payload}"
    );
    upstream.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_websocket_upgrade_is_tunnelled_both_ways() {
    let harness = Harness::plain();
    let upstream = MockUpstream::start().await;
    let id = harness.upstream("realtime", upstream.addr, json!({})).await;
    harness.route(&id, "/ws", &["GET"]).await;
    let gateway = harness.served().await;

    let mut socket = WsClient::connect(gateway.addr).await;
    let (status, headers, body) = socket.handshake("realtime").await;
    assert_eq!(status, 101, "the gateway answers the handshake: {body}");
    assert_eq!(
        headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("upgrade"))
            .map(|(_, v)| v.clone()),
        Some("websocket".to_owned()),
        "the upgrade token passes through: {headers:?}"
    );

    socket.send_text("ping through the gateway").await;
    assert_eq!(
        socket.read_text().await,
        "ping through the gateway",
        "frames flow both ways"
    );
    socket.send_text("second frame").await;
    assert_eq!(
        socket.read_text().await,
        "second frame",
        "the tunnel stays open"
    );
    socket.close().await;
    let _ = id;
    gateway.stop();
    upstream.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cors_preflight_is_answered_locally_without_an_upstream() {
    let harness = Harness::plain();
    let upstream = MockUpstream::start().await;
    let id = harness
        .upstream(
            "browser",
            upstream.addr,
            json!({ "cors": { "enabled": true, "allowed_origins": ["https://app.example.com"] } }),
        )
        .await;
    harness.route(&id, "/v1", &["GET", "POST"]).await;

    let sent = harness
        .send(
            "OPTIONS",
            "/oagw/v1/proxy/browser/v1/things",
            &[
                ("origin", "https://app.example.com"),
                ("access-control-request-method", "POST"),
                (
                    "access-control-request-headers",
                    "Content-Type, Authorization",
                ),
            ],
            None,
        )
        .await;
    assert_eq!(sent.status, StatusCode::NO_CONTENT, "{}", sent.problem());
    assert_eq!(
        sent.header("access-control-allow-origin").as_deref(),
        Some("https://app.example.com")
    );
    assert_eq!(
        sent.header("access-control-allow-methods").as_deref(),
        Some("POST")
    );
    assert_eq!(
        sent.header("access-control-allow-headers").as_deref(),
        Some("Content-Type, Authorization")
    );
    assert_eq!(
        sent.header("access-control-max-age").as_deref(),
        Some("86400")
    );
    assert!(
        sent.header("vary").unwrap_or_default().contains("Origin"),
        "the preflight varies on the origin"
    );
    assert_eq!(
        sent.header("x-oagw-error-source").as_deref(),
        Some("gateway")
    );
    upstream.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cors_actual_request_is_validated_before_forwarding() {
    let harness = Harness::plain();
    let upstream = MockUpstream::start().await;
    let id = harness
        .upstream(
            "browser",
            upstream.addr,
            json!({
                "cors": {
                    "enabled": true,
                    "allowed_origins": ["https://app.example.com"],
                    "allowed_methods": ["GET", "DELETE"],
                    "expose_headers": ["X-Trace-Id"]
                }
            }),
        )
        .await;
    harness.route(&id, "/v1", &["GET", "DELETE"]).await;

    let allowed = harness
        .send(
            "GET",
            "/oagw/v1/proxy/browser/v1/things",
            &[("origin", "https://app.example.com")],
            None,
        )
        .await;
    assert_eq!(allowed.status, StatusCode::OK, "{}", allowed.problem());
    assert_eq!(
        allowed.header("access-control-allow-origin").as_deref(),
        Some("https://app.example.com")
    );
    assert_eq!(
        allowed.header("access-control-expose-headers").as_deref(),
        Some("X-Trace-Id")
    );

    let refused = harness
        .send(
            "GET",
            "/oagw/v1/proxy/browser/v1/things",
            &[("origin", "https://evil.com")],
            None,
        )
        .await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "{}",
        refused.problem()
    );
    assert_eq!(
        refused.header("x-oagw-error-source").as_deref(),
        Some("gateway")
    );
    assert_eq!(
        refused.problem()["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1"
    );
    upstream.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_disallowed_cors_method_is_refused() {
    let harness = Harness::plain();
    let upstream = MockUpstream::start().await;
    let id = harness
        .upstream(
            "browser",
            upstream.addr,
            json!({ "cors": { "enabled": true, "allowed_origins": ["https://app.example.com"] } }),
        )
        .await;
    harness.route(&id, "/v1", &["GET", "DELETE"]).await;

    // The upstream policy allows GET/POST only, so a DELETE is refused before
    // the upstream sees it.
    let sent = harness
        .send(
            "DELETE",
            "/oagw/v1/proxy/browser/v1/things",
            &[("origin", "https://app.example.com")],
            None,
        )
        .await;
    assert_eq!(sent.status, StatusCode::FORBIDDEN, "{}", sent.problem());
    assert_eq!(
        sent.problem()["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1"
    );
    assert_eq!(sent.problem()["method"], "DELETE");
    upstream.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_without_an_origin_bypasses_the_cors_check() {
    let harness = Harness::plain();
    let upstream = MockUpstream::start().await;
    let id = harness
        .upstream(
            "browser",
            upstream.addr,
            json!({ "cors": { "enabled": true, "allowed_origins": ["https://app.example.com"] } }),
        )
        .await;
    harness.route(&id, "/v1", &["GET", "DELETE"]).await;

    let sent = harness
        .send("DELETE", "/oagw/v1/proxy/browser/v1/things", &[], None)
        .await;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.problem());
    upstream.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn credentials_cannot_be_combined_with_the_wildcard_origin() {
    let harness = Harness::plain();
    let sent = harness
        .send_json(
            "POST",
            "/oagw/v1/upstreams",
            json!({
                "alias": "browser.example.com",
                "server": { "endpoints": [
                    { "scheme": "https", "host": "api.example.com" }
                ] },
                "cors": { "enabled": true, "allowed_origins": ["*"], "allow_credentials": true }
            }),
        )
        .await;
    assert_eq!(sent.status, StatusCode::BAD_REQUEST, "{}", sent.problem());
}

/// A minimal WebSocket client speaking the raw protocol over TCP.
///
/// The gateway is driven by a real socket so hyper produces the upgrade the
/// data plane needs; `oneshot` cannot.
struct WsClient {
    stream: tokio::net::TcpStream,
}

impl WsClient {
    async fn connect(gateway: SocketAddr) -> Self {
        let stream = tokio::net::TcpStream::connect(gateway)
            .await
            .expect("connect");
        Self { stream }
    }

    /// Sends the handshake through the gateway and reads the answer.
    async fn handshake(&mut self, alias: &str) -> (u16, Vec<(String, String)>, String) {
        use tokio::io::AsyncWriteExt;
        let request = format!(
            "GET /oagw/v1/proxy/{alias}/ws HTTP/1.1\r\n\
             Host: gateway\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-WebSocket-Version: 13\r\n\
             \r\n"
        );
        self.stream
            .write_all(request.as_bytes())
            .await
            .expect("written");
        let raw = self.read_raw().await;
        if raw.starts_with("HTTP/1.1 101") || raw.starts_with("HTTP/1.0 101") {
            // A successful handshake has no body; the frames follow.
            let (status, headers) = Self::parse(&raw);
            return (status, headers, raw);
        }
        // A failure carries a problem body worth reporting; the connection
        // stays alive, so the body is read by its declared length.
        let (status, headers) = Self::parse(&raw);
        let length = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.parse::<usize>().ok())
            .unwrap_or_default();
        let body = self.read_exact_bytes(length).await;
        (status, headers, format!("{raw}{body}"))
    }

    fn parse(raw: &str) -> (u16, Vec<(String, String)>) {
        let mut lines = raw.lines();
        let status = lines
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse::<u16>().ok())
            .unwrap_or_default();
        let headers: Vec<(String, String)> = lines
            .filter_map(|line| {
                let (name, value) = line.split_once(':')?;
                Some((name.trim().to_owned(), value.trim().to_owned()))
            })
            .collect();
        (status, headers)
    }

    async fn read_exact_bytes(&mut self, length: usize) -> String {
        use tokio::io::AsyncReadExt;
        let mut rest = vec![0_u8; length];
        self.stream.read_exact(&mut rest).await.expect("body read");
        String::from_utf8_lossy(&rest).to_string()
    }

    async fn send_text(&mut self, text: &str) {
        use tokio::io::AsyncWriteExt;
        let mask = [0x2a_u8, 0x4b, 0x11, 0x9f];
        let mut frame = vec![0x81, 0x80 | u8::try_from(text.len()).expect("short frame")];
        frame.extend_from_slice(&mask);
        frame.extend(
            text.bytes()
                .enumerate()
                .map(|(index, byte)| byte ^ mask[index % mask.len()]),
        );
        self.stream.write_all(&frame).await.expect("frame written");
        self.stream.flush().await.expect("flushed");
    }

    async fn read_text(&mut self) -> String {
        use tokio::io::AsyncReadExt;
        let mut header = [0_u8; 2];
        self.stream
            .read_exact(&mut header)
            .await
            .expect("frame header");
        let length = usize::from(header[1] & 0x7f);
        let mut payload = vec![0_u8; length];
        self.stream.read_exact(&mut payload).await.expect("payload");
        String::from_utf8(payload).expect("utf-8 frame")
    }

    async fn close(&mut self) {
        use tokio::io::AsyncWriteExt;
        let mask = [0x11_u8, 0x22, 0x33, 0x44];
        let frame = [0x88, 0x80, mask[0], mask[1], mask[2], mask[3]];
        let _ = self.stream.write_all(&frame).await;
        let _ = self.stream.flush().await;
    }

    /// Reads the handshake answer byte by byte, so no frame byte is consumed.
    async fn read_raw(&mut self) -> String {
        use tokio::io::AsyncReadExt;
        let mut raw = Vec::new();
        let mut byte = [0_u8; 1];
        loop {
            if self.stream.read_exact(&mut byte).await.is_err() {
                break;
            }
            raw.push(byte[0]);
            if raw.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        String::from_utf8(raw).unwrap_or_default()
    }
}
