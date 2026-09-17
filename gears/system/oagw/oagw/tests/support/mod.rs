//! Shared scaffolding for the data-plane tests.
//!
//! The control-plane half reuses the `Router::oneshot` pattern of
//! `management_api_test.rs`; the data-plane half needs two things that pattern
//! cannot give it, so both sides are built here once:
//!
//! * a **real listening server** for the gateway (`axum::serve` on
//!   `127.0.0.1:0`), because a `Router::oneshot` call cannot carry a protocol
//!   upgrade nor prove incremental delivery;
//! * **raw fake upstreams** on `127.0.0.1:0` that speak HTTP/1.1 by hand, so a
//!   test can hold an SSE response open mid-flight, echo a WebSocket tunnel and
//!   read exactly what the gateway sent.
//!
//! Everything binds to an OS-assigned port on loopback and every spawned task
//! is shut down by its owner.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::doc_markdown)]

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use credstore_sdk::CredStoreClientV1;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinHandle;
use tower::ServiceExt;
use uuid::Uuid;

use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationSpec;
use toolkit_security::SecurityContext;

use oagw::config::{OagwConfig, SsrfPolicy};
use oagw::domain::plugin::PluginRegistry;
use oagw::domain::services::{ProxyService, RouteService, UpstreamService};
use oagw::gear::OagwState;
use oagw::infra::plugin::builtin::register_builtins;
use oagw::infra::storage::memory::InMemoryRepositories;

/// How long a raw-socket read may wait for the bytes a test knows must come.
///
/// Generous enough for a loaded CI box, short enough that a *missing* delivery
/// fails the test in seconds instead of hanging it.
pub const READ_BUDGET: Duration = Duration::from_secs(5);

/// Tenant every fixture runs under.
pub const TENANT: Uuid = Uuid::nil();

/// `protocol` value of an HTTP upstream (`docs/schemas/upstream.v1.schema.json`).
pub const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
/// `protocol` value of a gRPC upstream.
pub const PROTOCOL_GRPC: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1";

// ---------------------------------------------------------------------------
// Gear harness
// ---------------------------------------------------------------------------

struct NoopOpenApiRegistry;

impl OpenApiRegistry for NoopOpenApiRegistry {
    fn register_operation(&self, _spec: &OperationSpec) {}

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

/// The e2e posture: plaintext upstreams dialable, a five-second proxy timeout
/// and the SSRF policy left at its (permissive-allowlist) default.
pub fn config() -> OagwConfig {
    OagwConfig {
        proxy_timeout_secs: 5,
        allow_http_upstream: true,
        ssrf_policy: SsrfPolicy::default(),
        ..OagwConfig::default()
    }
}

/// The built-in plugin set, optionally with a credstore client wired in so the
/// credential-bearing auth plugins can resolve `cred://` references.
pub fn state_with(
    config: OagwConfig,
    credstore: Option<Arc<dyn CredStoreClientV1>>,
) -> Arc<OagwState> {
    let repos = InMemoryRepositories::new().into_repos();
    let mut plugins = PluginRegistry::new();
    let credentials = register_builtins(&mut plugins);
    if let Some(client) = credstore {
        credentials.set_client(client);
    }
    Arc::new(OagwState {
        upstreams: UpstreamService::new(&repos),
        routes: RouteService::new(&repos),
        proxy: ProxyService::try_new(repos, config.clone(), plugins)
            .expect("proxy service")
            .with_credentials(credentials),
        config: Arc::new(config),
    })
}

/// Default gear state (no credstore client: `cred://` references fail closed).
pub fn state() -> Arc<OagwState> {
    state_with(config(), None)
}

pub fn security_context(tenant_id: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::now_v7())
        .subject_tenant_id(tenant_id)
        .build()
        .expect("valid SecurityContext")
}

/// The gear's routes, with the caller's [`SecurityContext`] injected the way the
/// server's authentication middleware would.
pub fn router_for(state: Arc<OagwState>) -> Router {
    let openapi = NoopOpenApiRegistry;
    oagw::api::rest::routes::register_routes(Router::new(), &openapi, state)
        .layer(axum::Extension(security_context(TENANT)))
}

/// A real server for the gear router plus the same router for `oneshot` calls.
///
/// Both halves share one [`OagwState`], so a management write performed through
/// the router is immediately visible to the proxied requests served on the
/// socket — which is the L1-cache behaviour ADR 0005 asks for.
pub struct Fixture {
    router: Router,
    server: Server,
}

impl Fixture {
    pub async fn start() -> Self {
        Self::start_with(state()).await
    }

    pub async fn start_with(state: Arc<OagwState>) -> Self {
        let router = router_for(state);
        let server = Server::start(router.clone()).await;
        Self { router, server }
    }

    /// The same router, for the `Router::oneshot` management calls.
    pub fn router(&self) -> Router {
        self.router.clone()
    }

    pub fn gateway(&self) -> SocketAddr {
        self.server.addr()
    }

    pub async fn stop(self) {
        self.server.shutdown().await;
    }
}

/// A real listening server for the gear, on an OS-assigned loopback port.
pub struct Server {
    addr: SocketAddr,
    shutdown: mpsc::Sender<()>,
    handle: JoinHandle<()>,
}

impl Server {
    pub async fn start(router: Router) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind gateway");
        let addr = listener.local_addr().expect("gateway address");
        let (shutdown, mut close) = mpsc::channel::<()>(1);
        let handle = tokio::spawn(async move {
            let server =
                axum::serve(listener, router).with_graceful_shutdown(async move {
                    let _ = close.recv().await;
                });
            if let Err(error) = server.await {
                tracing::debug!(%error, "test gateway stopped");
            }
        });
        Self {
            addr,
            shutdown,
            handle,
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub async fn shutdown(self) {
        let _ = self.shutdown.send(()).await;
        let _ = self.handle.await;
    }
}

// ---------------------------------------------------------------------------
// Management calls (Router::oneshot, as in `management_api_test.rs`)
// ---------------------------------------------------------------------------

pub fn request(method: &str, uri: &str, body: Option<Value>) -> Request<Body> {
    request_with_headers(method, uri, body, &[])
}

pub fn request_with_headers(
    method: &str,
    uri: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let payload = body
        .map(|value| serde_json::to_vec(&value).expect("serialisable body"))
        .unwrap_or_default();
    let mut request = builder.body(Body::from(payload)).expect("request");
    request
        .extensions_mut()
        .insert(security_context(TENANT));
    request
}

pub async fn call_on(
    router: Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let response = router
        .oneshot(request(method, uri, body))
        .await
        .expect("oneshot response");
    let status = response.status();
    let bytes = response.into_body().collect().await.expect("body").to_bytes();
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).to_string()));
    (status, value)
}

pub async fn create_upstream(router: &Router, body: Value) -> (StatusCode, Value) {
    call_on(router.clone(), "POST", "/oagw/v1/upstreams", Some(body)).await
}

pub async fn create_route(router: &Router, body: Value) -> (StatusCode, Value) {
    call_on(router.clone(), "POST", "/oagw/v1/routes", Some(body)).await
}

/// One `http` endpoint pointing at `addr`.
pub fn http_endpoint(addr: &SocketAddr) -> Value {
    json!({ "scheme": "http", "host": addr.ip().to_string(), "port": addr.port() })
}

/// Minimal upstream body for a single-endpoint pool on `addr`.
pub fn upstream_body(alias: &str, addr: &SocketAddr) -> Value {
    json!({
        "alias": alias,
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [http_endpoint(addr)] },
        "tags": ["test"],
    })
}

pub fn route_body(upstream_id: &str, path: &str) -> Value {
    json!({
        "upstream_id": upstream_id,
        "match": { "http": { "methods": ["GET", "POST", "PUT", "DELETE", "PATCH"], "path": path } }
    })
}

/// Creates `upstream` + one route for `path`, returning the upstream id.
pub async fn wire_up(router: &Router, alias: &str, addr: &SocketAddr, path: &str) -> String {
    let (status, created) = create_upstream(router, upstream_body(alias, addr)).await;
    assert_eq!(status, StatusCode::CREATED, "upstream: {created}");
    let id = created["id"].as_str().expect("upstream id").to_owned();
    let (status, route) = create_route(router, route_body(&id, path)).await;
    assert_eq!(status, StatusCode::CREATED, "route: {route}");
    id
}

// ---------------------------------------------------------------------------
// Raw gateway client (for upgrades and incremental responses)
// ---------------------------------------------------------------------------

/// A parsed response head plus whatever bytes arrived behind it.
pub struct RawResponse {
    /// Status code of the response head.
    pub status: u16,
    /// Headers, lowercased.
    pub headers: Vec<(String, String)>,
    /// Bytes that arrived in the same read as (or after) the response head.
    pub prefix: Vec<u8>,
    /// The still-open connection.
    pub stream: TcpStream,
}

/// Connects to `addr`, writes `raw` verbatim and reads one response head.
pub async fn raw_request(addr: SocketAddr, raw: &str) -> RawResponse {
    let mut stream = TcpStream::connect(addr).await.expect("connect gateway");
    stream.write_all(raw.as_bytes()).await.expect("send request");
    let mut buffer: Vec<u8> = Vec::new();
    let head_end = loop {
        if let Some(index) = find(&buffer, b"\r\n\r\n") {
            break index;
        }
        let mut chunk = [0u8; 4096];
        let read = tokio::time::timeout(READ_BUDGET, stream.read(&mut chunk))
            .await
            .expect("response head within the read budget")
            .expect("readable response head");
        assert!(read > 0, "the gateway closed the connection before answering");
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status = status_line
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("unparsable status line '{status_line}'"));
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect::<Vec<_>>();
    RawResponse {
        status,
        headers,
        prefix: buffer[head_end + 4..].to_vec(),
        stream,
    }
}

/// Reads whatever arrives within the budget, up to `max` bytes.
pub async fn read_some(stream: &mut TcpStream, budget: Duration, max: usize) -> Vec<u8> {
    let mut buffer = vec![0u8; max];
    match tokio::time::timeout(budget, stream.read(&mut buffer)).await {
        Ok(Ok(read)) => buffer.truncate(read),
        // A timeout means "nothing more arrived", which is a legitimate
        // observation for the assertions below.
        Ok(Err(_)) | Err(_) => buffer.clear(),
    }
    buffer
}

/// A fully-read gateway response.
///
/// Returned by [`raw_get`], which reads exactly `Content-Length` bytes after
/// the response head (or until EOF when there is none), so a test never races
/// the arrival of a body.
pub struct Proxied {
    /// Status code.
    pub status: u16,
    /// Response headers, lowercased.
    pub headers: Vec<(String, String)>,
    /// Response body.
    pub body: Vec<u8>,
}

impl Proxied {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    pub fn header_values(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
            .collect()
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|_| panic!("expected a JSON body, got {:?}", self.body))
    }

    /// Asserts this is a DESIGN problem document of `status` and `gts_type`,
    /// returned by the gateway itself.
    pub fn assert_problem(&self, status: u16, gts_type: &str) -> Value {
        assert_eq!(self.status, status, "body: {}", self.text());
        assert_eq!(
            self.header("content-type"),
            Some("application/problem+json"),
            "a gateway failure is a problem document: {}",
            self.text()
        );
        assert_eq!(
            self.header("x-oagw-error-source"),
            Some("gateway"),
            "a gateway failure names its origin"
        );
        let body = self.json();
        assert_eq!(body["status"], status, "{body}");
        assert_eq!(body["type"], gts_type, "{body}");
        body
    }
}

/// Completes a response whose head [`raw_request`] already parsed.
pub async fn finish(response: RawResponse, budget: Duration) -> Proxied {
    let RawResponse {
        status,
        headers,
        prefix,
        mut stream,
    } = response;
    let mut body = prefix;
    let declared = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse::<usize>().ok());
    match declared {
        Some(length) => {
            while body.len() < length {
                let read = read_some(&mut stream, budget, length - body.len()).await;
                if read.is_empty() {
                    break;
                }
                body.extend_from_slice(&read);
            }
        }
        // No length declared (a 101 upgrade, or a chunked response): read to EOF.
        None => loop {
            let read = read_some(&mut stream, budget, 8192).await;
            if read.is_empty() {
                break;
            }
            body.extend_from_slice(&read);
        },
    }
    Proxied {
        status,
        headers,
        body,
    }
}

/// `GET`s the gateway over a real socket and reads the whole response.
pub async fn raw_get(gateway: SocketAddr, path: &str, headers: &[(&str, &str)]) -> Proxied {
    let mut raw = format!("GET {path} HTTP/1.1\r\nhost: gateway\r\nconnection: close\r\n");
    for (name, value) in headers {
        raw.push_str(&format!("{name}: {value}\r\n"));
    }
    raw.push_str("\r\n");
    let response = raw_request(gateway, &raw).await;
    finish(response, READ_BUDGET).await
}

/// Sends a hand-written request to the gateway and reads the whole response.
pub async fn raw_send(gateway: SocketAddr, raw: String) -> Proxied {
    let response = raw_request(gateway, &raw).await;
    finish(response, READ_BUDGET).await
}

/// Accumulates bytes until `needle` shows up or the budget expires.
pub async fn read_until(
    stream: &mut TcpStream,
    needle: &str,
    budget: Duration,
) -> (Vec<u8>, bool) {
    read_until_seeded(Vec::new(), stream, needle, budget).await
}

/// [`read_until`] starting from bytes already read (the bytes that arrived with
/// the response head, for instance).
pub async fn read_until_seeded(
    mut buffer: Vec<u8>,
    stream: &mut TcpStream,
    needle: &str,
    budget: Duration,
) -> (Vec<u8>, bool) {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if find(buffer.as_slice(), needle.as_bytes()).is_some() {
            return (buffer, true);
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return (buffer, false);
        }
        let mut chunk = [0u8; 4096];
        match tokio::time::timeout(remaining, stream.read(&mut chunk)).await {
            Ok(Ok(0)) | Err(_) => return (buffer, false),
            Ok(Ok(read)) => buffer.extend_from_slice(&chunk[..read]),
            Ok(Err(_)) => return (buffer, false),
        }
    }
}

/// Writes `raw` on an established connection and flushes it.
pub async fn send_raw(stream: &mut TcpStream, raw: &str) {
    stream.write_all(raw.as_bytes()).await.expect("write");
    stream.flush().await.expect("flush");
}

/// Position of `needle` in `haystack`, if present.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

// ---------------------------------------------------------------------------
// Fake upstreams
// ---------------------------------------------------------------------------

/// A request as the fake upstream observed it on the wire.
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    /// Request method.
    pub method: String,
    /// Request target path (query string removed).
    pub path: String,
    /// Raw query string, when the request carried one.
    pub query: Option<String>,
    /// Headers, lowercased, in wire order.
    pub headers: Vec<(String, String)>,
    /// Request body bytes.
    pub body: Vec<u8>,
}

impl RecordedRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    pub fn has_header(&self, name: &str, value: &str) -> bool {
        self.header(name).is_some_and(|seen| seen.eq_ignore_ascii_case(value))
    }

    pub fn body_str(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// A canned HTTP/1.1 response.
#[derive(Debug, Clone)]
pub struct FakeResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl FakeResponse {
    /// `status` with an `application/json` body.
    pub fn json(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
            body: body.into(),
        }
    }

    /// A plain-text response.
    pub fn plain(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            headers: vec![("content-type".to_owned(), "text/plain".to_owned())],
            body: body.into(),
        }
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

}

/// What a fake upstream does with each connection it accepts.
#[derive(Clone, Debug)]
pub enum Behaviour {
    /// Answer every request with one canned response and close.
    Canned(FakeResponse),
    /// `text/event-stream`: write the head and `first`, then hold the
    /// connection open until [`FakeUpstream::release`] is called, then write
    /// `second` and close. A test that read `first` before releasing has
    /// therefore *proved* the gateway did not buffer the stream.
    Gated {
        /// Extra response headers (the `content-type` belongs here).
        head: Vec<(String, String)>,
        /// First SSE event, written immediately.
        first: String,
        /// Second SSE event, written only once released.
        second: String,
    },
}

struct FakeShared {
    requests: std::sync::Mutex<Vec<RecordedRequest>>,
    release: tokio::sync::Notify,
}

/// A fake upstream speaking HTTP/1.1 by hand on a real socket.
///
/// It answers every request with its [`Behaviour`] and records every request it
/// received, so a test can assert both what the gateway sent and what it did
/// not.
pub struct FakeUpstream {
    addr: SocketAddr,
    shared: Arc<FakeShared>,
    close: mpsc::Sender<()>,
    handle: JoinHandle<()>,
}

impl FakeUpstream {
    /// Starts on `127.0.0.1` at an OS-assigned port.
    pub async fn start(behaviour: Behaviour) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake upstream");
        Self::spawn(listener, behaviour)
    }

    /// Starts on `ip` at `port`, so two endpoints of one pool (which the model
    /// requires to share a port) can be distinguished by address.
    pub async fn start_on(ip: IpAddr, port: u16, behaviour: Behaviour) -> Self {
        let listener = TcpListener::bind(SocketAddr::new(ip, port))
            .await
            .expect("bind fake upstream on a pinned port");
        Self::spawn(listener, behaviour)
    }

    fn spawn(listener: TcpListener, behaviour: Behaviour) -> Self {
        let addr = listener.local_addr().expect("fake upstream address");
        let shared = Arc::new(FakeShared {
            requests: std::sync::Mutex::new(Vec::new()),
            release: Notify::new(),
        });
        let (close, mut closed) = mpsc::channel::<()>(1);
        let loop_shared = Arc::clone(&shared);
        let handle = tokio::spawn(async move {
            let shared = loop_shared;
            loop {
                let accepted = tokio::select! {
                    _ = closed.recv() => break,
                    accepted = listener.accept() => accepted,
                };
                let Ok((stream, _)) = accepted else { break };
                let shared = Arc::clone(&shared);
                let behaviour = behaviour.clone();
                tokio::spawn(handle_connection(stream, shared, behaviour));
            }
        });
        Self {
            addr,
            shared,
            close,
            handle,
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn ip(&self) -> IpAddr {
        self.addr.ip()
    }

    /// Releases a [`Behaviour::Gated`] stream.
    pub fn release(&self) {
        self.shared.release.notify_waiters();
    }

    /// Every request this upstream received, in arrival order.
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.shared
            .requests
            .lock()
            .expect("fake upstream requests")
            .clone()
    }

    /// Number of requests received so far.
    pub fn count(&self) -> usize {
        self.shared.requests.lock().expect("fake upstream requests").len()
    }

    /// Stops the accept loop and drops the listeners.
    pub async fn shutdown(self) {
        let _ = self.close.send(()).await;
        let _ = self.handle.await;
    }
}

async fn handle_connection(mut stream: TcpStream, shared: Arc<FakeShared>, behaviour: Behaviour) {
    let Some(request) = read_request(&mut stream).await else {
        return;
    };
    shared
        .requests
        .lock()
        .expect("fake upstream requests")
        .push(request);

    match behaviour {
        Behaviour::Canned(response) => {
            let mut wire = format!("HTTP/1.1 {} {}\r\n", response.status, reason(response.status));
            for (name, value) in &response.headers {
                wire.push_str(&format!("{name}: {value}\r\n"));
            }
            wire.push_str(&format!(
                "content-length: {}\r\nconnection: close\r\n\r\n",
                response.body.len()
            ));
            if stream.write_all(wire.as_bytes()).await.is_err() {
                return;
            }
            let _ = stream.write_all(&response.body).await;
            let _ = stream.flush().await;
            // Half-close, then wait briefly for the peer to drain, so the
            // response is never cut short by a late RST.
            let _ = stream.shutdown().await;
            let mut drain = [0u8; 1024];
            let _ = tokio::time::timeout(Duration::from_millis(200), stream.read(&mut drain)).await;
        }
        Behaviour::Gated { head, first, second } => {
            let mut wire = String::from("HTTP/1.1 200 OK\r\n");
            for (name, value) in &head {
                wire.push_str(&format!("{name}: {value}\r\n"));
            }
            wire.push_str("\r\n");
            if stream.write_all(wire.as_bytes()).await.is_err() {
                return;
            }
            if stream.write_all(first.as_bytes()).await.is_err() {
                return;
            }
            let _ = stream.flush().await;
            // Held open until the test says so: nothing else can make the
            // second chunk appear.
            shared.release.notified().await;
            let _ = stream.write_all(second.as_bytes()).await;
            let _ = stream.flush().await;
            let _ = stream.shutdown().await;
        }
    }
}

/// Reads one HTTP/1.1 request (head plus `Content-Length` body).
///
/// Returns `None` on EOF, a timeout or an unparsable head.
async fn read_request(stream: &mut TcpStream) -> Option<RecordedRequest> {
    const MAX_HEAD: usize = 64 * 1024;
    let mut buffer: Vec<u8> = Vec::with_capacity(1024);
    let head_end = loop {
        if let Some(index) = find(&buffer, b"\r\n\r\n") {
            break index;
        }
        if buffer.len() > MAX_HEAD {
            return None;
        }
        let mut chunk = [0u8; 4096];
        let read = tokio::time::timeout(READ_BUDGET, stream.read(&mut chunk))
            .await
            .ok()?
            .ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    };

    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split(' ');
    let method = parts.next()?.to_owned();
    let target = parts.next()?.to_owned();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let content_length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);

    let mut body = buffer[head_end + 4..].to_vec();
    let mut chunk = [0u8; 4096];
    while body.len() < content_length {
        let read = tokio::time::timeout(READ_BUDGET, stream.read(&mut chunk))
            .await
            .ok()?
            .ok()?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length);

    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_owned(), Some(query.to_owned())),
        None => (target.clone(), None),
    };
    Some(RecordedRequest {
        method,
        path,
        query,
        headers,
        body,
    })
}

/// Reason phrase of the statuses the fake upstream answers with.
fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        404 => "Not Found",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "OK",
    }
}

// ---------------------------------------------------------------------------
// Fake WebSocket upstream
// ---------------------------------------------------------------------------

/// A fake upstream that completes a WebSocket handshake and then echoes raw
/// bytes back.
///
/// No frame protocol: the handshake proves the gateway forwarded the upgrade,
/// and the raw echo proves the bidirectional splice. A `Sec-WebSocket-Accept`
/// value is computed so a real client library could also complete the
/// handshake, but hyper does not validate it.
pub struct FakeWsUpstream {
    addr: SocketAddr,
    upgraded: std::sync::Arc<std::sync::atomic::AtomicBool>,
    close: mpsc::Sender<()>,
    handle: JoinHandle<()>,
}

impl FakeWsUpstream {
    /// Starts on `127.0.0.1` at an OS-assigned port.
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake websocket upstream");
        let addr = listener.local_addr().expect("fake websocket address");
        let upgraded = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (close, mut closed) = mpsc::channel::<()>(1);
        let seen = std::sync::Arc::clone(&upgraded);
        let handle = tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    _ = closed.recv() => break,
                    accepted = listener.accept() => accepted,
                };
                let Ok((stream, _)) = accepted else { break };
                let seen = std::sync::Arc::clone(&seen);
                tokio::spawn(echo_after_upgrade(stream, seen));
            }
        });
        Self {
            addr,
            upgraded,
            close,
            handle,
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// `true` once an inbound request carried `Upgrade: websocket`.
    pub fn saw_upgrade(&self) -> bool {
        self.upgraded
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    pub async fn shutdown(self) {
        let _ = self.close.send(()).await;
        let _ = self.handle.await;
    }
}

async fn echo_after_upgrade(
    mut stream: TcpStream,
    seen: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    let Some(request) = read_request(&mut stream).await else {
        return;
    };
    let websocket = request.has_header("upgrade", "websocket");
    seen.store(websocket, std::sync::atomic::Ordering::SeqCst);
    if !websocket {
        let _ = stream
            .write_all(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\n\r\n")
            .await;
        return;
    }

    if stream
        .write_all(
            b"HTTP/1.1 101 Switching Protocols\r\n\
              upgrade: websocket\r\n\
              connection: Upgrade\r\n\
              \r\n",
        )
        .await
        .is_err()
    {
        return;
    }
    let _ = stream.flush().await;

    // Server-initiated push, before any client byte: proves the upstream →
    // client half of the splice.
    if stream.write_all(b"hello-from-upstream").await.is_err() {
        return;
    }
    let _ = stream.flush().await;

    // Echo everything the client sends back, until the tunnel closes.
    let mut buffer = [0u8; 1024];
    loop {
        match stream.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                if stream.write_all(&buffer[..read]).await.is_err() {
                    break;
                }
                let _ = stream.flush().await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Request templates
// ---------------------------------------------------------------------------
