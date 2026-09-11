//! Shared harness for the OAGW integration tests.
//!
//! Builds a complete gateway over in-memory repositories and a mock
//! credential store, and serves it on a real local TCP port so that
//! streaming responses and WebSocket upgrades can be exercised end to end.

// The harness is included by every test binary, and each one exercises a
// different subset of it, so items that are live in one binary read as dead in
// another.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::doc_markdown)]
#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use http_body_util::BodyExt as _;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationSpec;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use oagw::api::rest::proxy;
use oagw::api::rest::routes;
use oagw::api::rest::state::OagwState;
use oagw::config::OagwConfig;
use oagw::domain::model::{
    Endpoint, EndpointScheme, Protocol, Route, ServerConfig, Upstream,
};
use oagw::infra::control_plane::OagwControlPlane;
use oagw::infra::memory::{MemoryPluginRepository, MemoryRouteRepository, MemoryUpstreamRepository};
use oagw::infra::plugin::builtin_registries;
use oagw::infra::proxy::ProxyEngine;
use oagw::infra::secret::SecretResolver;
use oagw::domain::service::ControlPlaneService as _;

/// Registry that records nothing; the OpenAPI document is not under test.
pub struct NoopOpenApiRegistry;

impl OpenApiRegistry for NoopOpenApiRegistry {
    fn register_operation(&self, _spec: &OperationSpec) {}

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

/// A parent/child tenant chain, used to exercise hierarchy shadowing.
pub struct ChainTenantResolver {
    /// child tenant -> parent tenant.
    pub parents: std::collections::HashMap<Uuid, Uuid>,
}

#[async_trait]
impl tenant_resolver_sdk::TenantResolverClient for ChainTenantResolver {
    async fn get_tenant(
        &self,
        _ctx: &SecurityContext,
        id: tenant_resolver_sdk::TenantId,
    ) -> Result<tenant_resolver_sdk::TenantInfo, tenant_resolver_sdk::TenantResolverError> {
        Ok(tenant_info(id))
    }

    async fn get_root_tenant(
        &self,
        _ctx: &SecurityContext,
    ) -> Result<tenant_resolver_sdk::TenantInfo, tenant_resolver_sdk::TenantResolverError> {
        Ok(tenant_info(tenant_resolver_sdk::TenantId(Uuid::nil())))
    }

    async fn get_tenants(
        &self,
        _ctx: &SecurityContext,
        ids: &[tenant_resolver_sdk::TenantId],
        _options: &tenant_resolver_sdk::GetTenantsOptions,
    ) -> Result<Vec<tenant_resolver_sdk::TenantInfo>, tenant_resolver_sdk::TenantResolverError> {
        Ok(ids.iter().map(|id| tenant_info(*id)).collect())
    }

    async fn get_ancestors(
        &self,
        _ctx: &SecurityContext,
        id: tenant_resolver_sdk::TenantId,
        _options: &tenant_resolver_sdk::GetAncestorsOptions,
    ) -> Result<tenant_resolver_sdk::GetAncestorsResponse, tenant_resolver_sdk::TenantResolverError>
    {
        let mut ancestors = Vec::new();
        let mut current = id;
        while let Some(parent) = self.parents.get(&current.0) {
            ancestors.push(tenant_ref(tenant_resolver_sdk::TenantId(*parent)));
            current = tenant_resolver_sdk::TenantId(*parent);
        }
        Ok(tenant_resolver_sdk::GetAncestorsResponse {
            tenant: tenant_ref(id),
            ancestors,
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
            tenant: tenant_ref(id),
            descendants: Vec::new(),
        })
    }

    async fn is_ancestor(
        &self,
        _ctx: &SecurityContext,
        ancestor: tenant_resolver_sdk::TenantId,
        descendant: tenant_resolver_sdk::TenantId,
        _options: &tenant_resolver_sdk::IsAncestorOptions,
    ) -> Result<bool, tenant_resolver_sdk::TenantResolverError> {
        let mut current = descendant;
        while let Some(parent) = self.parents.get(&current.0) {
            if *parent == ancestor.0 {
                return Ok(true);
            }
            current = tenant_resolver_sdk::TenantId(*parent);
        }
        Ok(false)
    }
}

fn tenant_info(id: tenant_resolver_sdk::TenantId) -> tenant_resolver_sdk::TenantInfo {
    tenant_resolver_sdk::TenantInfo {
        id,
        name: "tenant".to_owned(),
        status: tenant_resolver_sdk::TenantStatus::Active,
        tenant_type: None,
        parent_id: None,
        self_managed: false,
    }
}

fn tenant_ref(id: tenant_resolver_sdk::TenantId) -> tenant_resolver_sdk::TenantRef {
    tenant_resolver_sdk::TenantRef {
        id,
        status: tenant_resolver_sdk::TenantStatus::Active,
        tenant_type: None,
        parent_id: None,
        self_managed: false,
    }
}

/// A gateway wired to in-memory repositories and a mock credential store.
pub struct Harness {
    /// Shared REST/data-plane state.
    pub state: Arc<OagwState>,
    /// The calling tenant used by the test requests.
    pub tenant_id: Uuid,
    /// The control plane, for direct test setup.
    pub control_plane: Arc<OagwControlPlane>,
    addr: std::sync::OnceLock<SocketAddr>,
}

impl Harness {
    /// Builds a gateway with the given configuration and tenant hierarchy.
    #[must_use]
    pub fn new(
        config: OagwConfig,
        parents: std::collections::HashMap<Uuid, Uuid>,
    ) -> Arc<Self> {
        Self::with_credential_store(
            config,
            parents,
            &{
                let store: Arc<dyn credstore_sdk::CredStoreClientV1> =
                    Arc::new(credstore_sdk::test_util::MockCredStoreClient::empty());
                store
            },
        )
    }

    /// Builds a gateway whose credential store resolves a fixed set of secrets.
    #[must_use]
    pub fn with_credential_store(
        config: OagwConfig,
        parents: std::collections::HashMap<Uuid, Uuid>,
        credstore: &Arc<dyn credstore_sdk::CredStoreClientV1>,
    ) -> Arc<Self> {
        let registries = Arc::new(builtin_registries(
            SecretResolver::new(Arc::clone(credstore)),
            std::time::Duration::from_mins(1),
            16,
        ));
        let tenant_client: Option<Arc<dyn tenant_resolver_sdk::TenantResolverClient>> =
            Some(Arc::new(ChainTenantResolver { parents }));
        let control_plane = Arc::new(OagwControlPlane::new(
            Arc::new(MemoryUpstreamRepository::default()),
            Arc::new(MemoryRouteRepository::default()),
            Arc::new(MemoryPluginRepository::default()),
            Arc::clone(&registries),
            tenant_client,
            config.clone(),
        ));
        let engine = Arc::new(ProxyEngine::new(
            Arc::clone(&control_plane) as Arc<dyn oagw::domain::service::ControlPlaneService>,
            Arc::clone(&registries),
            config,
        ));
        Arc::new(Self {
            state: Arc::new(OagwState {
                control_plane: Arc::clone(&control_plane),
                engine,
                registries,
            }),
            tenant_id: Uuid::new_v4(),
            control_plane,
            addr: std::sync::OnceLock::new(),
        })
    }

    /// Builds a gateway with the default configuration and a flat hierarchy.
    #[must_use]
    pub fn plain() -> Arc<Self> {
        Self::new(OagwConfig::default(), std::collections::HashMap::new())
    }

    /// Builds a gateway whose credential store knows the secrets the auth
    /// plugin tests use: `cred://test-key`, `cred://client`, `cred://secret`.
    #[must_use]
    pub fn with_credentials() -> Arc<Self> {
        Self::with_credential_store(
            OagwConfig {
                allow_http_upstream: true,
                ..OagwConfig::default()
            },
            std::collections::HashMap::new(),
            &{
                let store: Arc<dyn credstore_sdk::CredStoreClientV1> = Arc::new(
                    credstore_sdk::test_util::MockCredStoreClient::with_secrets(vec![
                        ("cred://test-key".to_owned(), "super-secret".to_owned()),
                        ("cred://client".to_owned(), "client-id".to_owned()),
                        ("cred://secret".to_owned(), "client-secret".to_owned()),
                    ]),
                );
                store
            },
        )
    }

    /// Builds a gateway that admits plaintext upstreams.
    #[must_use]
    pub fn allowing_http() -> Arc<Self> {
        Self::new(
            OagwConfig {
                allow_http_upstream: true,
                ..OagwConfig::default()
            },
            std::collections::HashMap::new(),
        )
    }

    /// A second calling identity over the same gateway.
    ///
    /// The control plane and the proxy engine — and therefore the rate-limit
    /// counters — are shared, so requests from both tenants land in the same
    /// bucket store.
    #[must_use]
    pub fn with_tenant(self: &Arc<Self>, tenant_id: Uuid) -> Arc<Self> {
        Arc::new(Self {
            state: Arc::clone(&self.state),
            tenant_id,
            control_plane: Arc::clone(&self.control_plane),
            addr: std::sync::OnceLock::new(),
        })
    }

    /// The security context used by the test requests.
    #[must_use]
    pub fn context(&self) -> SecurityContext {
        SecurityContext::builder()
            .subject_id(self.tenant_id)
            .subject_tenant_id(self.tenant_id)
            .build()
            .expect("valid security context")
    }

    /// The assembled router, ready to be served.
    ///
    /// The host gateway normally authenticates and injects the security
    /// context; here a middleware stands in for it, using the harness tenant.
    pub fn router(self: &Arc<Self>) -> Router {
        let openapi = NoopOpenApiRegistry;
        let router = routes::register(Router::new(), &openapi);
        let router = proxy::register(router, &openapi);
        let tenant_id = self.tenant_id;
        router
            .layer(axum::Extension(Arc::clone(&self.state)))
            .layer(axum::middleware::from_fn(
                move |request: axum::extract::Request, next: axum::middleware::Next| async move {
                    let context = SecurityContext::builder()
                        .subject_id(tenant_id)
                        .subject_tenant_id(tenant_id)
                        .build()
                        .expect("valid security context");
                    let mut request = request;
                    request.extensions_mut().insert(context);
                    next.run(request).await
                },
            ))
    }

    /// Serves the router once and returns its address.
    ///
    /// Later calls return the same address, so a test may build several clients
    /// against one gateway.
    pub async fn serve_once(self: &Arc<Self>) -> SocketAddr {
        if let Some(addr) = self.addr.get() {
            return *addr;
        }
        let addr = self.serve().await;
        let _stored = self.addr.set(addr);
        addr
    }

    /// Serves the router on an ephemeral local port.
    pub async fn serve(self: &Arc<Self>) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the test listener");
        let addr = listener.local_addr().expect("local address");
        let router = self.router();
        tokio::spawn(async move {
            axum::serve(listener, router).await.expect("serve the router");
        });
        addr
    }

    /// Creates an upstream through the control plane.
    pub async fn create_upstream(&self, upstream: Upstream) -> Upstream {
        self.control_plane
            .create_upstream(self.tenant_id, upstream)
            .await
            .expect("create the upstream")
    }

    /// Creates a route through the control plane.
    pub async fn create_route(&self, route: Route) -> Route {
        self.control_plane
            .create_route(self.tenant_id, route)
            .await
            .expect("create the route")
    }

    /// Registers a single-endpoint `http` upstream pointing at `addr`.
    ///
    /// The alias is whatever the endpoints derive, returned for use in the
    /// proxy path.
    pub async fn upstream_at(&self, addr: SocketAddr) -> Upstream {
        self.create_upstream(Upstream {
            id: None,
            enabled: true,
            alias: None,
            tags: Vec::new(),
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: EndpointScheme::Http,
                    host: "127.0.0.1".to_owned(),
                    port: addr.port(),
                }],
            },
            protocol: Protocol::Http,
            auth: None,
            headers: None,
            plugins: oagw::domain::model::PluginsConfig::default(),
            rate_limit: None,
            cors: None,
        })
        .await
    }
}

/// A rendered gateway response.
#[derive(Debug, Clone)]
pub struct TestResponse {
    /// Status code.
    pub status: u16,
    /// Response headers, lowercase names.
    pub headers: Vec<(String, String)>,
    /// Buffered body.
    pub body: Vec<u8>,
}

impl TestResponse {
    /// The first value of a header, case-insensitively.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        let lowered = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(key, _)| *key == lowered)
            .map(|(_, value)| value.as_str())
    }

    /// The body as UTF-8.
    #[must_use]
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// The body parsed as JSON.
    #[must_use]
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or(serde_json::Value::Null)
    }
}

/// A streaming gateway response, for `SSE`.
pub struct TestStream {
    /// Status code.
    pub status: u16,
    /// Response headers, lowercase names.
    pub headers: Vec<(String, String)>,
    body: hyper::body::Incoming,
}

impl TestStream {
    /// Reads the next body chunk, `None` at end of stream.
    pub async fn next_chunk(&mut self) -> Option<Vec<u8>> {
        self.body.frame().await?.ok()?.data_ref().map(|data| data.to_vec())
    }

    /// The first value of a header, case-insensitively.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        let lowered = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(key, _)| *key == lowered)
            .map(|(_, value)| value.as_str())
    }
}

/// Sends a request to the gateway.
pub async fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> TestResponse {
    let response = send(addr, method, path, headers, body, false).await;
    TestResponse {
        status: response.status().as_u16(),
        headers: header_pairs(response.headers()),
        body: read_body(response).await,
    }
}

/// Sends a request and hands back the body before it is fully consumed.
pub async fn request_stream(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> TestStream {
    let response = send(addr, method, path, headers, body, false).await;
    let (parts, body) = response.into_parts();
    TestStream {
        status: parts.status.as_u16(),
        headers: header_pairs(&parts.headers),
        body,
    }
}

/// Sends a WebSocket upgrade request and returns the raw upgraded stream.
///
/// The low-level hyper client connection cannot fulfil upgrades, so the
/// handshake is written onto the socket by hand.
pub async fn upgrade(
    addr: SocketAddr,
    path: &str,
    headers: &[(&str, &str)],
) -> Result<(u16, Vec<(String, String)>, tokio::net::TcpStream), TestResponse> {
    use std::fmt::Write as _;
    use tokio::io::AsyncWriteExt as _;

    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");

    let mut head = format!(
        "GET {path} HTTP/1.1\r\nhost: {addr}\r\nconnection: Upgrade\r\nupgrade: websocket\r\n\
         sec-websocket-key: dGhlIHNhbXBsZSBub25jZQ==\r\nsec-websocket-version: 13\r\n"
    );
    for (name, value) in headers {
        write!(head, "{name}: {value}\r\n").expect("write a header line");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await.expect("write the upgrade");

    let raw = read_head(&mut stream).await;
    let (bytes, tail) = split_head(&raw).unwrap_or((&raw, &[][..]));
    let head = String::from_utf8_lossy(bytes).into_owned();
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or_default();
    let pairs: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| {
            (
                name.trim().to_ascii_lowercase(),
                value.trim().to_owned(),
            )
        })
        .collect();
    if status != 101 {
        return Err(TestResponse {
            status,
            headers: pairs,
            body: tail.to_vec(),
        });
    }
    Ok((status, pairs, stream))
}

/// Reads from `stream` until a full response head (`\r\n\r\n`) has arrived.
async fn read_head(stream: &mut tokio::net::TcpStream) -> Vec<u8> {
    use tokio::io::AsyncReadExt as _;

    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let read = stream.read(&mut chunk).await.expect("read the response head");
        if read == 0 {
            return buffer;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            return buffer;
        }
    }
}

/// Splits a raw response at its head terminator.
fn split_head(raw: &[u8]) -> Option<(&[u8], &[u8])> {
    let index = raw.windows(4).position(|window| window == b"\r\n\r\n")?;
    Some((&raw[..index], &raw[index + 4..]))
}

fn header_pairs(headers: &hyper::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_ascii_lowercase(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect()
}

type BoxBody = axum::body::Body;

fn send(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
    _streaming: bool,
) -> impl std::future::Future<Output = hyper::Response<hyper::body::Incoming>> {
    let uri = format!("http://{addr}{path}");
    async move {
        let stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
        let io = hyper_util::rt::TokioIo::new(stream);
        let (mut sender, conn) =
            hyper::client::conn::http1::handshake(io).await.expect("handshake");
        tokio::spawn(async move {
            drop(conn.await);
        });
        let method = hyper::Method::from_bytes(method.as_bytes()).expect("valid method");
        let mut builder = hyper::Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let request = builder
            .header("host", format!("{addr}"))
            .body(if body.is_empty() {
                http_body_util::Empty::<bytes::Bytes>::new().boxed_unsync()
            } else {
                http_body_util::Full::new(bytes::Bytes::from(body)).boxed_unsync()
            })
            .expect("build the request");
        sender.send_request(request).await.expect("send the request")
    }
}

async fn read_body(response: hyper::Response<hyper::body::Incoming>) -> Vec<u8> {
    let (_, body) = response.into_parts();
    body.collect()
        .await
        .expect("read the body")
        .to_bytes()
        .to_vec()
}

/// The `BoxBody` type used by axum responses.
type AxumBody = BoxBody;

// ---------------------------------------------------------------------------------------
// JSON helpers
// ---------------------------------------------------------------------------------------

/// Sends a request with a `JSON` body and returns the rendered response.
pub async fn json_request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    body: serde_json::Value,
) -> TestResponse {
    let bytes = serde_json::to_vec(&body).expect("serialise the body");
    let mut headers: Vec<(&str, &str)> = vec![("content-type", "application/json")];
    if method == "GET" || method == "DELETE" {
        headers.clear();
    }
    request(addr, method, path, &headers, bytes).await
}

/// `POST`s a `JSON` body.
pub async fn post(addr: SocketAddr, path: &str, body: serde_json::Value) -> TestResponse {
    json_request(addr, "POST", path, body).await
}

/// `PUT`s a `JSON` body.
pub async fn put(addr: SocketAddr, path: &str, body: serde_json::Value) -> TestResponse {
    json_request(addr, "PUT", path, body).await
}

/// Sends a `GET`.
pub async fn get(addr: SocketAddr, path: &str) -> TestResponse {
    request(addr, "GET", path, &[], Vec::new()).await
}

/// Sends a `DELETE`.
pub async fn delete(addr: SocketAddr, path: &str) -> TestResponse {
    request(addr, "DELETE", path, &[], Vec::new()).await
}

/// Writes a raw `HTTP`/1.1 request to the socket, for cases the client library
/// cannot express (a wrong `Content-Length`, an unsupported transfer encoding).
pub async fn raw_request(addr: SocketAddr, request: &[u8]) -> TestResponse {
    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    tokio::io::AsyncWriteExt::write_all(&mut stream, request)
        .await
        .expect("write the raw request");
    tokio::io::AsyncWriteExt::shutdown(&mut stream)
        .await
        .expect("half-close");
    let mut buffer = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut buffer)
        .await
        .expect("read the raw response");
    let split = buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("a response head");
    let head = String::from_utf8_lossy(&buffer[..split]).into_owned();
    let status: u16 = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .expect("a status code");
    let headers: Vec<(String, String)> = head
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    TestResponse {
        status,
        headers,
        body: buffer[split + 4..].to_vec(),
    }
}

// ---------------------------------------------------------------------------------------
// Request body builders
// ---------------------------------------------------------------------------------------

/// An upstream body pointing at `host:port` with the given scheme.
pub fn upstream_body(host: &str, port: u16, scheme: &str) -> serde_json::Value {
    serde_json::json!({
        "server": {"endpoints": [{"scheme": scheme, "host": host, "port": port}]},
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
    })
}

/// A route body matching `methods` on `path` for `upstream_id`.
pub fn route_body(upstream_id: &str, methods: &[&str], path: &str) -> serde_json::Value {
    serde_json::json!({
        "upstream_id": upstream_id,
        "match": {"http": {"methods": methods, "path": path}}
    })
}

/// An upstream body with the given explicit alias.
pub fn aliased_upstream_body(host: &str, port: u16, scheme: &str, alias: &str) -> serde_json::Value {
    let mut body = upstream_body(host, port, scheme);
    body["alias"] = serde_json::Value::String(alias.to_owned());
    body
}

/// The GTS identifier suffix after the `~`.
pub fn uuid_of(id: &str) -> &str {
    id.rsplit('~').next().unwrap_or(id)
}

// ---------------------------------------------------------------------------------------
// Mock upstream
// ---------------------------------------------------------------------------------------

/// One request the mock upstream received.
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    /// Method.
    pub method: String,
    /// Path.
    pub path: String,
    /// Raw query string, without the `?`.
    pub query: String,
    /// Headers, lowercase names.
    pub headers: Vec<(String, String)>,
    /// Buffered body.
    pub body: Vec<u8>,
}

impl RecordedRequest {
    /// The first value of a header, case-insensitively.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// True when the named header is absent.
    #[must_use]
    pub fn has_no_header(&self, name: &str) -> bool {
        self.header(name).is_none()
    }

    /// The body as UTF-8.
    #[must_use]
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Collects what the mock upstream receives.
#[derive(Clone, Default)]
pub struct Recorder {
    requests: Arc<std::sync::Mutex<Vec<RecordedRequest>>>,
}

impl Recorder {
    /// A fresh recorder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The requests recorded so far.
    #[must_use]
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().expect("lock the recorder").clone()
    }

    /// How many requests were recorded.
    #[must_use]
    pub fn count(&self) -> usize {
        self.requests.lock().expect("lock the recorder").len()
    }

    /// Waits until `count` requests have been recorded.
    pub async fn wait_for(&self, count: usize) {
        for _ in 0..400 {
            if self.count() >= count {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("only {} requests recorded, expected {count}", self.count());
    }

    /// The first request, or `None`.
    #[must_use]
    pub fn first(&self) -> Option<RecordedRequest> {
        self.requests().into_iter().next()
    }

    fn record(&self, request: RecordedRequest) {
        self.requests.lock().expect("lock the recorder").push(request);
    }

    /// A router that records every request and answers `200 {"ok":true}`.
    pub fn router(&self) -> axum::Router {
        self.responding_with(axum::http::StatusCode::OK, "application/json", "{\"ok\":true}")
    }

    /// A router that records every request and answers with a fixed response.
    pub fn responding_with(
        &self,
        status: axum::http::StatusCode,
        content_type: &'static str,
        body: &'static str,
    ) -> axum::Router {
        let recorder = self.clone();
        let fixed_body = body;
        axum::Router::new().fallback(
            move |method: axum::http::Method,
                  uri: axum::http::Uri,
                  headers: axum::http::HeaderMap,
                  body: bytes::Bytes| async move {
                recorder.record(RecordedRequest {
                    method: method.as_str().to_owned(),
                    path: uri.path().to_owned(),
                    query: uri.query().unwrap_or_default().to_owned(),
                    headers: header_pairs(&headers),
                    body: body.to_vec(),
                });
                let mut response =
                    axum::response::Response::new(axum::body::Body::from(fixed_body));
                *response.status_mut() = status;
                response.headers_mut().insert(
                    axum::http::header::CONTENT_TYPE,
                    axum::http::HeaderValue::from_static(content_type),
                );
                response
            },
        )
    }
}

/// A router that streams `text/event-stream` events, one per `interval`.
///
/// `active` is incremented when a connection opens and decremented when the
/// response body is dropped, so a test can observe a client disconnect.
pub fn sse_router(active: Arc<std::sync::atomic::AtomicUsize>) -> axum::Router {
    use std::sync::atomic::Ordering;

    axum::Router::new().route(
        "/v1/events",
        axum::routing::get(move || async move {
            active.fetch_add(1, Ordering::SeqCst);
            let guard = ActiveGuard(active);
            let body = axum::body::Body::from_stream(async_stream::stream! {
                let _guard = guard;
                for event in ["one", "two"] {
                    yield Ok::<bytes::Bytes, std::convert::Infallible>(
                        bytes::Bytes::from(format!("data: {event}\n\n")),
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
                }
            });
            axum::response::Response::builder()
                .status(axum::http::StatusCode::OK)
                .header(axum::http::header::CONTENT_TYPE, "text/event-stream")
                .body(body)
                .expect("build the event stream")
        }),
    )
}

/// Decrements the active-connection counter when dropped.
struct ActiveGuard(Arc<std::sync::atomic::AtomicUsize>);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// A router that upgrades `GET /v1/ws` and echoes text frames back.
pub fn echo_ws_router() -> axum::Router {
    use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};

    axum::Router::new().route(
        "/v1/ws",
        axum::routing::get(|ws: WebSocketUpgrade| async move {
            ws.on_upgrade(|mut socket: WebSocket| async move {
                while let Some(Ok(message)) = socket.recv().await {
                    if let Message::Text(text) = message
                        && socket.send(Message::text(text.clone())).await.is_err()
                    {
                        break;
                    }
                }
            })
        }),
    )
}

/// Serves a router on an ephemeral local port.
pub async fn spawn(router: axum::Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the mock upstream");
    let addr = listener.local_addr().expect("mock upstream address");
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve the mock");
    });
    addr
}

/// Serves a router on an ephemeral port of a specific loopback address.
///
/// Used to build multi-endpoint upstreams whose endpoints differ by host as
/// well as by port.
/// Serves `router` on `host` at an explicit `port`.
pub async fn spawn_on_port(
    host: std::net::IpAddr,
    port: u16,
    router: axum::Router,
) -> SocketAddr {
    let listener =
        tokio::net::TcpListener::bind(std::net::SocketAddr::new(host, port))
            .await
            .expect("bind the mock upstream");
    let addr = listener.local_addr().expect("mock upstream address");
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve the mock");
    });
    addr
}

pub async fn spawn_on(host: std::net::IpAddr, router: axum::Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind(std::net::SocketAddr::new(host, 0))
        .await
        .expect("bind the mock upstream");
    let addr = listener.local_addr().expect("mock upstream address");
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve the mock");
    });
    addr
}

// ---------------------------------------------------------------------------------------
// Minimal WebSocket client frames
// ---------------------------------------------------------------------------------------

/// Sends one unfragmented text frame.
pub async fn ws_send(stream: &mut tokio::net::TcpStream, payload: &[u8]) {
    use tokio::io::AsyncWriteExt as _;

    let mask = [0x11u8, 0x22, 0x33, 0x44];
    let mut frame = vec![0x81u8];
    debug_assert!(payload.len() < 126, "only short frames are supported");
    frame.push(u8::try_from(payload.len()).expect("a short frame") | 0x80);
    frame.extend_from_slice(&mask);
    for (index, byte) in payload.iter().enumerate() {
        frame.push(byte ^ mask[index % 4]);
    }
    stream.write_all(&frame).await.expect("write the frame");
    stream.flush().await.expect("flush the frame");
}

/// Reads one frame, returning its payload.
pub async fn ws_recv(stream: &mut tokio::net::TcpStream) -> Vec<u8> {
    use tokio::io::AsyncReadExt as _;

    let mut head = [0u8; 2];
    stream.read_exact(&mut head).await.expect("read the frame head");
    let length = usize::from(head[1] & 0x7f);
    let mut payload = vec![0u8; length];
    stream.read_exact(&mut payload).await.expect("read the frame body");
    payload
}
