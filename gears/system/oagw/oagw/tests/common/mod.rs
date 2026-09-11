//! Shared harness for the OAGW integration tests.
//!
//! [`TestApp`] mounts the gear's own router exactly as the host would —
//! `Oagw::init` followed by `register_rest` — over the in-memory repositories,
//! the real plugin registries and a mock credential store. The subject each
//! request acts as is injected by hand, the way the platform's bearer-token
//! middleware would, so the tests exercise the handlers below that layer.
//!
//! [`LocalUpstream`] is a programmable axum server on `127.0.0.1:0` playing the
//! part of an upstream service: it records what it received and answers with
//! whatever the test scripted, streamed bodies and WebSocket echoes included.

// The harness is one module shared by every integration target, and no single
// target touches all of it: the parts a target leaves alone are not dead code.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use http_body_util::BodyExt;
use parking_lot::Mutex;
use serde_json::{Value, json};
use toolkit::Gear;
use toolkit::GearCtx;
use toolkit::RestApiCapability;
use toolkit::api::OpenApiRegistry;
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use utoipa::openapi::RefOr;
use utoipa::openapi::schema::Schema;

/// A no-op `OpenAPI` registry: the HTTP tests exercise the runtime router, and
/// the document itself is pinned by the route builder's unit tests.
pub struct NoopOpenApiRegistry;

impl OpenApiRegistry for NoopOpenApiRegistry {
    fn register_operation(&self, _spec: &toolkit::api::operation_builder::OperationSpec) {}

    fn ensure_schema_raw(&self, name: &str, _schemas: Vec<(String, RefOr<Schema>)>) -> String {
        name.to_owned()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

pub struct TestConfigProvider {
    pub config: Value,
}

impl toolkit::config::ConfigProvider for TestConfigProvider {
    fn get_gear_config(&self, gear_name: &str) -> Option<&Value> {
        self.config.get(gear_name)
    }
}

/// A tenant resolver fake whose whole world is one parent.
pub struct FakeTenantResolver {
    parent: uuid::Uuid,
}

impl FakeTenantResolver {
    /// A resolver whose every tenant hangs off `parent`.
    pub fn rooted(parent: uuid::Uuid) -> Self {
        Self { parent }
    }
}

#[async_trait::async_trait]
impl tenant_resolver_sdk::TenantResolverClient for FakeTenantResolver {
    async fn get_tenant(
        &self,
        _ctx: &SecurityContext,
        id: tenant_resolver_sdk::TenantId,
    ) -> Result<tenant_resolver_sdk::TenantInfo, tenant_resolver_sdk::TenantResolverError> {
        Ok(tenant_info(
            id,
            Some(tenant_resolver_sdk::TenantId(self.parent)),
        ))
    }

    async fn get_root_tenant(
        &self,
        _ctx: &SecurityContext,
    ) -> Result<tenant_resolver_sdk::TenantInfo, tenant_resolver_sdk::TenantResolverError> {
        Err(tenant_resolver_sdk::TenantResolverError::NoPluginAvailable)
    }

    async fn get_tenants(
        &self,
        _ctx: &SecurityContext,
        ids: &[tenant_resolver_sdk::TenantId],
        _options: &tenant_resolver_sdk::GetTenantsOptions,
    ) -> Result<Vec<tenant_resolver_sdk::TenantInfo>, tenant_resolver_sdk::TenantResolverError>
    {
        Ok(ids
            .iter()
            .map(|id| tenant_info(*id, Some(tenant_resolver_sdk::TenantId(self.parent))))
            .collect())
    }

    async fn get_ancestors(
        &self,
        _ctx: &SecurityContext,
        id: tenant_resolver_sdk::TenantId,
        _options: &tenant_resolver_sdk::GetAncestorsOptions,
    ) -> Result<tenant_resolver_sdk::GetAncestorsResponse, tenant_resolver_sdk::TenantResolverError>
    {
        Ok(tenant_resolver_sdk::GetAncestorsResponse {
            tenant: tenant_ref(id, Some(tenant_resolver_sdk::TenantId(self.parent))),
            ancestors: vec![tenant_ref(tenant_resolver_sdk::TenantId(self.parent), None)],
        })
    }

    async fn get_descendants(
        &self,
        _ctx: &SecurityContext,
        id: tenant_resolver_sdk::TenantId,
        _options: &tenant_resolver_sdk::GetDescendantsOptions,
    ) -> Result<tenant_resolver_sdk::GetDescendantsResponse, tenant_resolver_sdk::TenantResolverError>
    {
        Ok(tenant_resolver_sdk::GetDescendantsResponse {
            tenant: tenant_ref(id, Some(tenant_resolver_sdk::TenantId(self.parent))),
            descendants: Vec::new(),
        })
    }

    async fn is_ancestor(
        &self,
        _ctx: &SecurityContext,
        _ancestor_id: tenant_resolver_sdk::TenantId,
        _descendant_id: tenant_resolver_sdk::TenantId,
        _options: &tenant_resolver_sdk::IsAncestorOptions,
    ) -> Result<bool, tenant_resolver_sdk::TenantResolverError> {
        Ok(false)
    }
}

fn tenant_info(
    id: tenant_resolver_sdk::TenantId,
    parent: Option<tenant_resolver_sdk::TenantId>,
) -> tenant_resolver_sdk::TenantInfo {
    tenant_resolver_sdk::TenantInfo {
        id,
        name: "fake tenant".to_owned(),
        status: tenant_resolver_sdk::TenantStatus::Active,
        tenant_type: None,
        parent_id: parent,
        self_managed: false,
    }
}

fn tenant_ref(
    id: tenant_resolver_sdk::TenantId,
    parent: Option<tenant_resolver_sdk::TenantId>,
) -> tenant_resolver_sdk::TenantRef {
    tenant_resolver_sdk::TenantRef {
        id,
        status: tenant_resolver_sdk::TenantStatus::Active,
        tenant_type: None,
        parent_id: parent,
        self_managed: false,
    }
}

/// The header naming which side produced an error document.
#[must_use]
pub const fn error_source_header() -> &'static str {
    oagw::api::rest::error::ERROR_SOURCE_HEADER
}

/// The gateway's value for [`error_source_header`].
#[must_use]
pub const fn error_source_gateway() -> &'static str {
    oagw::api::rest::error::ERROR_SOURCE_GATEWAY
}

/// The upstream's value for [`error_source_header`].
#[must_use]
pub const fn error_source_upstream() -> &'static str {
    oagw::api::rest::error::ERROR_SOURCE_UPSTREAM
}

/// A subject acting in `tenant`.
#[must_use]
pub fn subject(tenant: uuid::Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(uuid::Uuid::new_v4())
        .subject_tenant_id(tenant)
        .build()
        .expect("valid security context")
}

/// A mounted OAGW gear plus the tenants the hierarchy knows about.
pub struct TestApp {
    router: Router,
    /// The tenant every request acts in by default.
    pub tenant: uuid::Uuid,
    /// The parent tenant, reachable through the hierarchy.
    pub parent: uuid::Uuid,
    /// A tenant that shares nothing with `tenant`.
    pub foreign: uuid::Uuid,
    /// The control plane the gear was built with.
    pub control_plane: Arc<oagw::domain::services::control_plane::ControlPlaneService>,
}

/// An OAGW gear over the in-memory repositories, with `allow_http_upstream`
/// enabled so the tests can dial a loopback upstream in the clear.
pub async fn app() -> TestApp {
    app_with(json!({"allow_http_upstream": true, "proxy_timeout_secs": 5})).await
}

/// An OAGW gear with an explicit configuration section.
pub async fn app_with(config: Value) -> TestApp {
    let tenant = uuid::Uuid::new_v4();
    let parent = uuid::Uuid::new_v4();
    let foreign = uuid::Uuid::new_v4();

    let hub = Arc::new(toolkit::client_hub::ClientHub::new());
    hub.register::<dyn tenant_resolver_sdk::TenantResolverClient>(Arc::new(FakeTenantResolver {
        parent,
    }));
    hub.register::<dyn credstore_sdk::CredStoreClientV1>(Arc::new(
        credstore_sdk::test_util::MockCredStoreClient::with_secrets(vec![
            ("stripe-key".to_owned(), "sk_test_123".to_owned()),
            ("internal-token".to_owned(), "tok_internal".to_owned()),
        ]),
    ));

    let ctx = GearCtx::new(
        "oagw",
        uuid::Uuid::new_v4(),
        Arc::new(TestConfigProvider {
            config: json!({"oagw": {"config": config}}),
        }),
        hub,
        tokio_util::sync::CancellationToken::new(),
    );

    let gear = oagw::gear::Oagw::default();
    gear.init(&ctx).await.expect("oagw initializes");
    let control_plane = gear.control_plane().expect("control plane");
    let router = gear
        .register_rest(&ctx, Router::new(), &NoopOpenApiRegistry)
        .expect("oagw registers its routes");

    TestApp {
        router,
        tenant,
        parent,
        foreign,
        control_plane,
    }
}

impl TestApp {
    /// A request to the gear as the default tenant.
    pub fn request(
        &self,
        method: http::Method,
        path: &str,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> Request {
        request_as(self.tenant, method, path, body, headers)
    }

    /// Send a request and await the response.
    pub async fn send(&self, request: Request) -> http::Response<Body> {
        self.router
            .clone()
            .oneshot(request)
            .await
            .expect("router answers")
    }

    /// Serve the gear on a loopback port and return the address it listens on.
    ///
    /// A WebSocket upgrade is owned by the HTTP server, so a test that dials
    /// one needs a real socket rather than the in-process `oneshot` path. The
    /// layer installed here is the platform's bearer-token middleware stand-in:
    /// it puts the same subject into the extensions that the middleware would.
    pub async fn serve_with_subject(&self, tenant: uuid::Uuid) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind gateway under test");
        let addr = listener.local_addr().expect("gateway local addr");
        let ctx = subject(tenant);
        let router = self.router.clone().layer(axum::middleware::from_fn(
            move |mut request: Request, next: axum::middleware::Next| {
                let ctx = ctx.clone();
                async move {
                    request.extensions_mut().insert(ctx);
                    next.run(request).await
                }
            },
        ));
        tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });
        addr
    }

    /// Send a request and decode a JSON body.
    pub async fn send_json(
        &self,
        method: http::Method,
        path: &str,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> (StatusCode, Value) {
        let response = self.send(self.request(method, path, body, headers)).await;
        status_and_document(response).await
    }

    /// Send a request acting in `tenant` and decode a JSON body.
    pub async fn send_json_as(
        &self,
        tenant: uuid::Uuid,
        method: http::Method,
        path: &str,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> (StatusCode, Value) {
        let response = self
            .send(request_as(tenant, method, path, body, headers))
            .await;
        status_and_document(response).await
    }

    /// Create an upstream through the management API and return its document.
    pub async fn create_upstream(&self, body: Value) -> Value {
        self.create_upstream_as(self.tenant, body).await
    }

    /// Create an upstream acting in `tenant`.
    pub async fn create_upstream_as(&self, tenant: uuid::Uuid, body: Value) -> Value {
        let (status, document) = self
            .send_json_as(
                tenant,
                http::Method::POST,
                "/oagw/v1/upstreams",
                Some(body),
                &[],
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "upstream created: {document}");
        document
    }

    /// Create a route through the management API and return its document.
    pub async fn create_route(&self, body: Value) -> Value {
        let (status, document) = self
            .send_json(http::Method::POST, "/oagw/v1/routes", Some(body), &[])
            .await;
        assert_eq!(status, StatusCode::CREATED, "route created: {document}");
        document
    }

    /// Attempt a route creation that is expected to fail, and report why.
    pub async fn create_route_bad(&self, body: Value) -> (StatusCode, Value) {
        self.send_json(http::Method::POST, "/oagw/v1/routes", Some(body), &[])
            .await
    }

    /// Wire a route on `prefix` to `upstream` and return both documents.
    pub async fn wire(&self, spec: Value, prefix: &str) -> (Value, Value) {
        let upstream = self.create_upstream(spec).await;
        let alias = upstream["alias"].as_str().expect("alias").to_owned();
        let route = self
            .create_route(json!({
                "path": prefix,
                "methods": ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"],
                "target_alias": alias
            }))
            .await;
        (upstream, route)
    }
}

/// Split a response into its status and decoded JSON document.
pub async fn status_and_document(response: http::Response<Body>) -> (StatusCode, Value) {
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let document = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, document)
}

/// What the local upstream received.
#[derive(Clone, Debug)]
pub struct Received {
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Received {
    /// The first value of `name`, case-insensitively.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// Every value of `name`, case-insensitively.
    #[must_use]
    pub fn headers_all(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
            .collect()
    }

    /// The body as UTF-8, lossily.
    #[must_use]
    pub fn body_string(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// One scripted answer the local upstream gives.
#[derive(Clone, Default)]
pub struct Answer {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    /// When set, the body is written chunk by chunk with a pause between them.
    pub stream: Option<Vec<Vec<u8>>>,
    /// When set, accepted WebSocket connections echo frames back.
    pub echo_frames: bool,
}

impl Answer {
    /// `200 OK` with a JSON body.
    #[must_use]
    pub fn json(value: &Value) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
            body: Some(value.to_string().into_bytes()),
            ..Self::default()
        }
    }

    /// A bare status with an empty body.
    #[must_use]
    pub fn status(code: u16) -> Self {
        Self {
            status: code,
            ..Self::default()
        }
    }

    /// A body streamed chunk by chunk.
    #[must_use]
    pub fn stream(content_type: &str, chunks: Vec<Vec<u8>>) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type".to_owned(), content_type.to_owned())],
            stream: Some(chunks),
            ..Self::default()
        }
    }
}

/// The state the local upstream serves from.
#[derive(Clone, Default)]
struct Script {
    answer: Arc<Mutex<Answer>>,
    received: Arc<Mutex<Vec<Received>>>,
}

/// A local HTTP server that answers with a scripted response.
///
/// The bound address is what a test puts into an upstream's endpoint pool;
/// `set_answer` swaps the answer for subsequent requests.
pub struct LocalUpstream {
    pub addr: std::net::SocketAddr,
    script: Script,
}

impl LocalUpstream {
    /// Bind a listener and start serving in the background.
    pub async fn start() -> Self {
        Self::start_with(Answer::json(&json!({"ok": true}))).await
    }

    /// Bind a listener pre-loaded with `answer`.
    pub async fn start_with(answer: Answer) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local upstream");
        let addr = listener.local_addr().expect("local addr");
        let script = Script {
            answer: Arc::new(Mutex::new(answer)),
            received: Arc::new(Mutex::new(Vec::new())),
        };

        let app = Router::new()
            .route(
                "/{*rest}",
                axum::routing::any(upstream_handler).merge(axum::routing::get(upstream_handler)),
            )
            .route("/", axum::routing::any(upstream_handler))
            .route("/ws", axum::routing::get(websocket_handler))
            .with_state(script.clone());

        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        Self { addr, script }
    }

    /// Replace the scripted answer for subsequent requests.
    pub fn set_answer(&self, answer: Answer) {
        *self.script.answer.lock() = answer;
    }

    /// Every request the upstream has received so far.
    pub fn received(&self) -> Vec<Received> {
        self.script.received.lock().clone()
    }

    /// The most recent request, if any.
    pub fn last(&self) -> Option<Received> {
        self.script.received.lock().last().cloned()
    }

    /// The number of requests the upstream has served.
    pub fn count(&self) -> usize {
        self.script.received.lock().len()
    }

    /// The `http` endpoint naming this server.
    #[must_use]
    pub fn endpoint(&self) -> Value {
        json!({
            "scheme": "http",
            "host": self.addr.ip().to_string(),
            "port": self.addr.port()
        })
    }

    /// A second endpoint on the same host, for multi-endpoint pools.
    #[must_use]
    pub fn endpoint_on(&self, port: u16) -> Value {
        json!({"scheme": "http", "host": self.addr.ip().to_string(), "port": port})
    }

    /// A full upstream specification pointing at this server.
    #[must_use]
    pub fn upstream_spec(&self, alias: &str) -> Value {
        json!({
            "alias": alias,
            "name": "Local test upstream",
            "description": "bound by the test harness",
            "endpoints": [self.endpoint()],
            "sharing": "inherit"
        })
    }

    /// A route specification forwarding `prefix` to `alias`.
    #[must_use]
    pub fn route_spec(alias: &str, prefix: &str) -> Value {
        json!({
            "path": prefix,
            "methods": ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"],
            "target_alias": alias
        })
    }
}

/// Record the request, then answer with the script.
async fn upstream_handler(State(script): State<Script>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = body.collect().await.expect("body").to_bytes();
    script.received.lock().push(Received {
        method: parts.method.as_str().to_owned(),
        path: parts.uri.path().to_owned(),
        query: parts.uri.query().unwrap_or_default().to_owned(),
        headers: parts
            .headers
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
            .collect(),
        body: bytes.to_vec(),
    });

    let scripted = script.answer.lock().clone();
    if scripted.echo_frames {
        return (
            StatusCode::from_u16(scripted.status).unwrap(),
            Body::empty(),
        )
            .into_response();
    }

    let status = StatusCode::from_u16(scripted.status).unwrap_or(StatusCode::OK);
    if let Some(chunks) = &scripted.stream {
        let pieces = chunks.clone();
        let stream = async_stream::stream! {
            for piece in pieces {
                yield Ok::<_, std::convert::Infallible>(bytes::Bytes::from(piece));
                tokio::time::sleep(Duration::from_millis(120)).await;
            }
        };
        let mut response = Response::builder()
            .status(status)
            .body(Body::from_stream(stream))
            .expect("streamed response");
        *response.headers_mut() = headers_from(&scripted);
        return response;
    }

    let body = scripted.body.clone().unwrap_or_default();
    let mut response = (status, body).into_response();
    *response.headers_mut() = headers_from(&scripted);
    response
}

/// The scripted headers, applied wholesale.
fn headers_from(scripted: &Answer) -> http::HeaderMap {
    let mut headers = http::HeaderMap::new();
    for (name, value) in &scripted.headers {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }
    headers
}

/// Accept the upgrade and echo every frame back.
async fn websocket_handler(
    State(script): State<Script>,
    upgrade: axum::extract::ws::WebSocketUpgrade,
) -> Response {
    script.received.lock().push(Received {
        method: "GET".to_owned(),
        path: "/ws".to_owned(),
        query: String::new(),
        headers: Vec::new(),
        body: Vec::new(),
    });
    upgrade
        .on_upgrade(|mut socket| async move {
            while let Some(message) = socket.recv().await {
                let Ok(message) = message else { break };
                if socket.send(message).await.is_err() {
                    break;
                }
            }
        })
        .into_response()
}

/// A request to the gear acting as an explicit subject: it needs no app, only
/// the subject to install as an extension.
pub fn request_with_subject(
    ctx: SecurityContext,
    method: http::Method,
    path: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> Request {
    let mut builder = http::Request::builder()
        .method(method)
        .uri(path)
        .extension(ctx);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    match body {
        Some(value) => builder
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(value.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    }
}

/// A request to the gear acting in `tenant`.
pub fn request_as(
    tenant: uuid::Uuid,
    method: http::Method,
    path: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> Request {
    let mut builder = http::Request::builder()
        .method(method)
        .uri(path)
        .extension(subject(tenant));
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    match body {
        Some(value) => builder
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(value.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    }
}

/// An upstream that records the request line verbatim.
///
/// [`LocalUpstream`] is axum, which normalises whatever target it is handed, so
/// it cannot tell a gateway that forwarded an absolute-form request target from
/// one that forwarded origin-form. This one reads the first line off the socket
/// itself and answers a fixed response, so a test can pin what went on the wire.
pub struct RawUpstream {
    pub addr: std::net::SocketAddr,
    request_line: Arc<Mutex<Option<String>>>,
}

impl RawUpstream {
    /// Bind a listener that captures its first request line per connection.
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind raw upstream");
        let addr = listener.local_addr().expect("local addr");
        let request_line: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

        tokio::spawn({
            let request_line = Arc::clone(&request_line);
            async move {
                loop {
                    let Ok((mut socket, _)) = listener.accept().await else {
                        break;
                    };
                    let request_line = Arc::clone(&request_line);
                    tokio::spawn(async move {
                        let mut buffer = Vec::new();
                        let mut chunk = [0u8; 1024];
                        loop {
                            let read = socket.read(&mut chunk).await.unwrap_or(0);
                            if read == 0 {
                                return;
                            }
                            buffer.extend_from_slice(&chunk[..read]);
                            if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                                break;
                            }
                        }
                        let head = String::from_utf8_lossy(&buffer).into_owned();
                        *request_line.lock() = head.lines().next().map(str::to_owned);
                        let answer = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                                       Content-Length: 2\r\nConnection: close\r\n\r\n{}";
                        socket.write_all(answer).await.ok();
                        socket.shutdown().await.ok();
                    });
                }
            }
        });

        Self { addr, request_line }
    }

    /// The request line the last connection carried.
    #[must_use]
    pub fn request_line(&self) -> Option<String> {
        self.request_line.lock().clone()
    }
}

/// A request with a body of `size` bytes, a chosen `Content-Length` and any
/// extra headers, for exercising the gateway's body rules.
///
/// The gateway sees `Body::empty()` as a body of length zero, so a test that
/// wants a body it did not declare — or a length that disagrees with the
/// payload — has to assemble the request itself.
pub fn raw_body_request(
    tenant: uuid::Uuid,
    method: http::Method,
    path: &str,
    payload: Vec<u8>,
    content_length: Option<usize>,
    extra: &[(&str, &str)],
) -> Request {
    let mut builder = http::Request::builder()
        .method(method)
        .uri(path)
        .extension(subject(tenant));
    if let Some(length) = content_length {
        builder = builder.header(http::header::CONTENT_LENGTH, length.to_string());
    }
    for (name, value) in extra {
        builder = builder.header(*name, *value);
    }
    builder.body(Body::from(payload)).expect("valid request")
}

/// An upstream the test scripts at the byte level.
///
/// [`LocalUpstream`] speaks HTTP properly, so it cannot be made to answer with
/// a response head that is not HTTP, nor to take a connection and stay silent.
/// Both are documented gateway failures, so this one holds the socket open and
/// does exactly what it was told: nothing, or an arbitrary byte stream.
pub struct ByteUpstream {
    pub addr: std::net::SocketAddr,
}

impl ByteUpstream {
    /// Bind a listener that answers every connection with `script`.
    ///
    /// `None` means the connection is accepted and never written to, which is
    /// how an upstream that has hung behaves from the gateway's side.
    pub async fn start(script: Option<&'static [u8]>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind byte upstream");
        let addr = listener.local_addr().expect("byte upstream local addr");
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let script = script;
                tokio::spawn(async move {
                    // Read the request head so the answer is not sent before
                    // the gateway has finished asking.
                    let mut buffer = Vec::new();
                    let mut chunk = [0u8; 1024];
                    loop {
                        let read = socket.read(&mut chunk).await.unwrap_or(0);
                        if read == 0 {
                            return;
                        }
                        buffer.extend_from_slice(&chunk[..read]);
                        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                    }
                    if let Some(script) = script {
                        socket.write_all(script).await.ok();
                    }
                    // A hung upstream holds the socket without answering.
                    if script.is_some() {
                        socket.shutdown().await.ok();
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                });
            }
        });
        Self { addr }
    }

    /// The `http` endpoint naming this server.
    #[must_use]
    pub fn endpoint(&self) -> Value {
        json!({
            "scheme": "http",
            "host": self.addr.ip().to_string(),
            "port": self.addr.port()
        })
    }
}
