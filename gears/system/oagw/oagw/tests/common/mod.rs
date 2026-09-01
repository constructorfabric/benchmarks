// Shared harness for the OAGW integration tests: an in-process REST router,
// a stub `tenant-resolver` client and a scriptable upstream over real TCP.
#![allow(clippy::expect_used, clippy::unwrap_used)]
#![allow(dead_code)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::AsyncWriteExt;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use hyper_util::rt::TokioIo;
use tenant_resolver_sdk::{
    GetAncestorsOptions, GetAncestorsResponse, GetDescendantsOptions, GetDescendantsResponse,
    GetTenantsOptions, IsAncestorOptions, TenantId, TenantInfo, TenantRef, TenantResolverClient,
    TenantResolverError, TenantStatus,
};
use toolkit::api::operation_builder::OperationSpec;
use toolkit_security::SecurityContext;
use tower::ServiceExt;

use oagw::api::rest::routes::build_router;
use oagw::domain::model::{
    Endpoint, EndpointScheme, HttpMatch, MatchConfig, PathSuffixMode, Route, ServerConfig, Upstream,
};
use oagw::domain::services::management::ControlPlaneService;
use oagw::infra::controlplane::ControlPlaneServiceImpl;
use oagw::infra::metrics::OagwMetrics;
use oagw::infra::plugin::BuiltinPlugins;
use oagw::infra::proxy::service::DataPlaneServiceImpl;
pub use oagw::infra::proxy::service::ProxyOptions;
use oagw::infra::storage::InMemoryStore;

/// Noop OpenAPI registry: the integration tests exercise routing, not the
/// OpenAPI document.
struct NoopOpenApiRegistry;

impl toolkit::api::OpenApiRegistry for NoopOpenApiRegistry {
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

/// A two-tenant hierarchy: `root -> child`.
#[derive(Clone, Default)]
pub struct StaticTenants {
    tenants: Arc<HashMap<TenantId, Option<TenantId>>>,
}

impl StaticTenants {
    /// Builds the `root -> child` hierarchy.
    #[must_use]
    pub fn root_child(root: TenantId, child: TenantId) -> Self {
        let mut map = HashMap::new();
        map.insert(root, None);
        map.insert(child, Some(root));
        Self {
            tenants: Arc::new(map),
        }
    }

    fn info(&self, id: TenantId) -> TenantInfo {
        TenantInfo {
            id,
            name: format!("tenant-{}", id.0),
            status: tenant_resolver_sdk::TenantStatus::Active,
            tenant_type: None,
            parent_id: self.tenants.get(&id).copied().flatten(),
            self_managed: false,
        }
    }
}

#[async_trait]
impl TenantResolverClient for StaticTenants {
    async fn get_tenant(
        &self,
        _ctx: &SecurityContext,
        id: TenantId,
    ) -> Result<TenantInfo, TenantResolverError> {
        self.tenants.get(&id).map_or_else(
            || Err(TenantResolverError::TenantNotFound { tenant_id: id }),
            |_| Ok(self.info(id)),
        )
    }

    async fn get_root_tenant(
        &self,
        _ctx: &SecurityContext,
    ) -> Result<TenantInfo, TenantResolverError> {
        let root = self
            .tenants
            .iter()
            .find(|(_, parent)| parent.is_none())
            .map(|(id, _)| *id)
            .ok_or(TenantResolverError::NoPluginAvailable)?;
        Ok(self.info(root))
    }

    async fn get_tenants(
        &self,
        _ctx: &SecurityContext,
        ids: &[TenantId],
        _options: &GetTenantsOptions,
    ) -> Result<Vec<TenantInfo>, TenantResolverError> {
        Ok(ids
            .iter()
            .filter(|id| self.tenants.contains_key(id))
            .map(|id| self.info(*id))
            .collect())
    }

    async fn get_ancestors(
        &self,
        _ctx: &SecurityContext,
        id: TenantId,
        _options: &GetAncestorsOptions,
    ) -> Result<GetAncestorsResponse, TenantResolverError> {
        let mut ancestors = Vec::new();
        let mut current = id;
        while let Some(parent) = self.tenants.get(&current).copied().flatten() {
            ancestors.push(TenantRef {
                id: parent,
                status: TenantStatus::Active,
                tenant_type: None,
                parent_id: self.tenants.get(&parent).copied().flatten(),
                self_managed: false,
            });
            current = parent;
        }
        Ok(GetAncestorsResponse {
            tenant: TenantRef {
                id,
                status: TenantStatus::Active,
                tenant_type: None,
                parent_id: self.tenants.get(&id).copied().flatten(),
                self_managed: false,
            },
            ancestors,
        })
    }

    async fn get_descendants(
        &self,
        _ctx: &SecurityContext,
        id: TenantId,
        _options: &GetDescendantsOptions,
    ) -> Result<GetDescendantsResponse, TenantResolverError> {
        let own = self.tenants.get(&id).copied().flatten();
        let descendants = self
            .tenants
            .iter()
            .filter(|(_, parent)| **parent == Some(id))
            .map(|(child, _)| TenantRef {
                id: *child,
                status: TenantStatus::Active,
                tenant_type: None,
                parent_id: Some(id),
                self_managed: false,
            })
            .collect();
        Ok(GetDescendantsResponse {
            tenant: TenantRef {
                id,
                status: TenantStatus::Active,
                tenant_type: None,
                parent_id: own,
                self_managed: false,
            },
            descendants,
        })
    }

    async fn is_ancestor(
        &self,
        _ctx: &SecurityContext,
        ancestor_id: TenantId,
        descendant_id: TenantId,
        _options: &IsAncestorOptions,
    ) -> Result<bool, TenantResolverError> {
        let mut current = descendant_id;
        while let Some(parent) = self.tenants.get(&current).copied().flatten() {
            if parent == ancestor_id {
                return Ok(true);
            }
            current = parent;
        }
        Ok(false)
    }
}

/// Everything a test needs to drive the OAGW REST surface.
pub struct Harness {
    /// The OAGW sub-router (before dual-prefix mounting).
    pub sub: axum::Router,
    /// The sub-router mounted at both `/oagw/v1` and `/api/oagw/v1`.
    pub router: axum::Router,
    /// The backing store, for direct assertions.
    pub store: InMemoryStore,
    /// The Control Plane service, for direct configuration of a test.
    control_plane: Arc<dyn ControlPlaneService>,
}

impl Harness {
    /// Builds the harness around the given data-plane options.
    #[must_use]
    pub fn new(options: ProxyOptions) -> Self {
        Self::new_with_tenants(options, None)
    }

    /// Builds the harness with a `tenant-resolver` client for hierarchy tests.
    #[must_use]
    pub fn new_with_tenants(
        options: ProxyOptions,
        tenants: Option<Arc<dyn TenantResolverClient>>,
    ) -> Self {
        let store = InMemoryStore::new();
        let control_plane = Arc::new(
            ControlPlaneServiceImpl::new(store.clone(), tenants)
                .allowing_http_upstream(options.allow_http_upstream),
        );
        let data_plane = Arc::new(DataPlaneServiceImpl::new(
            control_plane.clone(),
            BuiltinPlugins::with_builtins_optional(None),
            OagwMetrics::new(),
            options,
        ));
        let registry = NoopOpenApiRegistry;
        let sub = build_router(control_plane.clone(), data_plane, &registry);
        let router = axum::Router::new()
            .merge(sub.clone())
            .nest("/api", sub.clone());
        Self {
            sub,
            router,
            store,
            control_plane,
        }
    }

    /// The Control Plane service handle.
    #[must_use]
    pub fn control_plane(&self) -> &Arc<dyn ControlPlaneService> {
        &self.control_plane
    }

    /// Creates a local upstream on `port` plus a route for it, returning the
    /// created upstream.
    ///
    /// # Errors
    ///
    /// Propagates any Control Plane validation error.
    pub async fn local_upstream(
        &self,
        ctx: &SecurityContext,
        port: u16,
        path: &str,
        methods: &[&str],
    ) -> Result<Upstream, oagw::domain::error::DomainError> {
        let created = self
            .control_plane
            .create_upstream(ctx, upstream_shell(port))
            .await?;
        self.control_plane
            .create_route(ctx, http_route(created.id, path, methods))
            .await?;
        Ok(created)
    }

    /// Sends a request through the OAGW sub-router as `ctx`.
    pub async fn send(
        &self,
        ctx: &SecurityContext,
        request: Request<Body>,
    ) -> axum::response::Response {
        let app = self.sub.clone().layer(axum::Extension(ctx.clone()));
        app.oneshot(request).await.expect("infallible service")
    }

    /// Sends a request as `ctx` through the dual-prefix router.
    pub async fn send_routed(
        &self,
        ctx: &SecurityContext,
        request: Request<Body>,
    ) -> axum::response::Response {
        let app = self.router.clone().layer(axum::Extension(ctx.clone()));
        app.oneshot(request).await.expect("infallible service")
    }

    /// Serves the dual-prefix router on an ephemeral port.
    ///
    /// The platform authentication middleware is not part of this harness, so
    /// `ctx` is layered onto every request instead, exactly as [`Self::send`]
    /// and [`Self::send_routed`] do.
    ///
    /// # Panics
    ///
    /// Panics when no ephemeral port can be bound.
    pub async fn serve_with(&self, ctx: &SecurityContext) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("local addr");
        let app = self.router.clone().layer(axum::Extension(ctx.clone()));
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("server");
        });
        address
    }
}

/// The calling tenant of a test context.
#[must_use]
pub fn tenant_of(ctx: &SecurityContext) -> TenantId {
    TenantId(ctx.subject_tenant_id())
}

/// An authenticated context for `tenant_id`.
#[must_use]
pub fn context_for(tenant: TenantId) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(uuid::Uuid::new_v4())
        .subject_tenant_id(tenant.0)
        .build()
        .expect("context")
}

/// An anonymous context (no tenant identity).
#[must_use]
pub fn anonymous() -> SecurityContext {
    SecurityContext::anonymous()
}

/// A cleartext endpoint on `127.0.0.1:port`.
#[must_use]
pub fn local_endpoint(port: u16) -> Endpoint {
    Endpoint {
        scheme: EndpointScheme::Http,
        host: "127.0.0.1".to_owned(),
        port,
    }
}

/// A server block for a single local endpoint.
#[must_use]
pub fn local_server(port: u16) -> ServerConfig {
    ServerConfig {
        endpoints: vec![local_endpoint(port)],
    }
}

/// A route matching `methods` on `path`, appending any path suffix.
#[must_use]
pub fn http_route(upstream_id: uuid::Uuid, path: &str, methods: &[&str]) -> Route {
    Route {
        id: uuid::Uuid::new_v4(),
        tenant_id: uuid::Uuid::nil(),
        upstream_id,
        match_config: MatchConfig {
            http: Some(HttpMatch {
                methods: methods.iter().map(|m| (*m).to_owned()).collect(),
                path: path.to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        },
        priority: 0,
        enabled: true,
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: Vec::new(),
        created_at: 0,
    }
}

/// A disabled upstream shell, completed by the Control Plane on create.
#[must_use]
pub fn upstream_shell(port: u16) -> Upstream {
    Upstream {
        id: uuid::Uuid::new_v4(),
        tenant_id: uuid::Uuid::nil(),
        alias: "local".to_owned(),
        protocol: "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1".to_owned(),
        enabled: true,
        server: local_server(port),
        auth: None,
        headers: None,
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: Vec::new(),
        created_at: 0,
    }
}

/// An HTTP request against the gateway.
#[must_use]
pub fn gateway_request(method: &str, uri: &str) -> Request<Body> {
    Request::builder()
        .method(axum::http::Method::from_bytes(method.as_bytes()).expect("method"))
        .uri(uri)
        .body(Body::empty())
        .expect("request")
}

/// An HTTP request with an arbitrary body.
#[must_use]
pub fn gateway_request_with(method: &str, uri: &str, body: &'static str) -> Request<Body> {
    Request::builder()
        .method(axum::http::Method::from_bytes(method.as_bytes()).expect("method"))
        .uri(uri)
        .header("content-type", "text/plain")
        .body(Body::from(body))
        .expect("request")
}

/// The response body as a lossy UTF-8 string.
pub async fn text(response: &mut axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(std::mem::take(response.body_mut()), MAX_BODY)
        .await
        .expect("body");
    String::from_utf8_lossy(&bytes).to_string()
}

/// Upper bound on the response bytes a test reads back.
const MAX_BODY: usize = 8 * 1024 * 1024;

/// A scriptable upstream behind a real TCP listener.
pub struct TestUpstream {
    /// The address the upstream listens on.
    pub address: SocketAddr,
}

impl TestUpstream {
    /// Starts an HTTP/1.1 upstream serving `handler` for every request.
    ///
    /// # Panics
    ///
    /// Panics when the listener cannot be bound.
    pub async fn start<F, Fut>(handler: F) -> Self
    where
        F: Fn(http::Request<hyper::body::Incoming>) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = UpstreamResponse> + Send + 'static,
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("addr");
        let shared = Arc::new(handler);
        tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    break;
                };
                let handler = shared.clone();
                tokio::spawn(async move {
                    let service = hyper::service::service_fn(move |request| {
                        let handler = handler.clone();
                        async move { Ok::<_, std::convert::Infallible>(handler(request).await) }
                    });
                    let io = TokioIo::new(socket);
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(io, service)
                        .with_upgrades()
                        .await;
                });
            }
        });
        Self { address }
    }

    /// The TCP port of the upstream.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.address.port()
    }
}

/// Shorthand for the response type the upstream handler returns.
pub type UpstreamResponse = http::Response<Body>;

/// An upstream that answers every request with a canned response and records
/// the *raw* bytes it received.
///
/// Unlike [`TestUpstream`] this is not backed by `hyper`'s server, which
/// normalises the request target and would mask a malformed request line. It
/// reads the first request head off the socket verbatim, so a test can pin the
/// exact request-target form the gateway puts on the wire.
pub struct RawUpstream {
    /// The address the upstream listens on.
    pub address: SocketAddr,
    /// The first request line + headers seen, captured verbatim.
    pub head: Arc<tokio::sync::Mutex<String>>,
}

impl RawUpstream {
    /// Starts a raw upstream that replies with `body` (as JSON) and keeps the
    /// first request head it receives.
    ///
    /// # Panics
    ///
    /// Panics when the listener cannot be bound.
    pub async fn start(body: &'static str) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("addr");
        let head = Arc::new(tokio::sync::Mutex::new(String::new()));
        let shared = Arc::clone(&head);
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            use tokio::io::AsyncReadExt;
            let mut buffer = vec![0u8; 8192];
            let mut captured = String::new();
            // Read until the head terminator (or the client stops talking).
            for _ in 0..32 {
                let read = match socket.read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                captured.push_str(&String::from_utf8_lossy(&buffer[..read]));
                if captured.contains("\r\n\r\n") {
                    break;
                }
            }
            let head_end = captured.find("\r\n\r\n").unwrap_or(captured.len());
            *shared.lock().await = captured[..head_end].to_owned();
            let payload = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(payload.as_bytes()).await;
            let _ = socket.shutdown().await;
        });
        Self { address, head }
    }

    /// The first request line the upstream received.
    #[must_use]
    pub async fn request_line(&self) -> String {
        self.head
            .lock()
            .await
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned()
    }
}

/// Builds a response with `status` and `body`.
#[must_use]
pub fn respond(status: http::StatusCode, body: impl Into<Body>) -> UpstreamResponse {
    http::Response::builder()
        .status(status)
        .body(body.into())
        .expect("response")
}

/// Builds a server-sent-events response streaming `events` one per chunk.
///
/// # Panics
///
/// Panics when the response cannot be built.
#[must_use]
pub fn sse_response(events: Vec<&'static str>) -> UpstreamResponse {
    let stream = futures_util::stream::iter(events.into_iter().map(|event| {
        Ok::<_, std::convert::Infallible>(bytes::Bytes::from(format!("{event}\n\n")))
    }));
    http::Response::builder()
        .status(http::StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(stream))
        .expect("response")
}

/// Builds a streaming response with `status`, `content_type` and `stream`.
#[must_use]
pub fn sse_stream<S>(status: StatusCode, content_type: &str, stream: S) -> UpstreamResponse
where
    S: futures_util::Stream<Item = Result<bytes::Bytes, std::convert::Infallible>> + Send + 'static,
{
    http::Response::builder()
        .status(status)
        .header("content-type", content_type)
        .body(Body::from_stream(stream))
        .expect("response")
}

/// Echoes the request method, path, query and body back as plain text.
pub async fn echo_upstream() -> TestUpstream {
    TestUpstream::start(|request| {
        let method = request.method().to_string();
        let path = request.uri().path().to_string();
        let query = request.uri().query().unwrap_or_default().to_string();
        async move {
            let body = http_body_util::BodyExt::collect(request.into_body())
                .await
                .map(|collected| String::from_utf8_lossy(&collected.to_bytes()).to_string())
                .unwrap_or_default();
            respond(
                http::StatusCode::OK,
                format!("{method} {path}?{query} {body}"),
            )
        }
    })
    .await
}

/// Starts an upstream that streams `count` numbered chunks.
///
/// # Panics
///
/// Panics when the listener cannot be bound.
pub async fn chunked_upstream(count: usize) -> TestUpstream {
    TestUpstream::start(move |_| {
        let stream = futures_util::stream::iter((0..count).map(|i| {
            Ok::<_, std::convert::Infallible>(bytes::Bytes::from(format!("chunk-{i:02}\n")))
        }));
        async move { respond(StatusCode::OK, Body::from_stream(stream)) }
    })
    .await
}

/// Starts a raw upstream that answers `101 Switching Protocols` and then
/// echoes every byte verbatim, as a WebSocket server would.
///
/// # Panics
///
/// Panics when the listener cannot be bound.
pub async fn echo_upgrade_upstream() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut head = Vec::new();
                let mut byte = [0_u8; 1];
                loop {
                    if socket.read_exact(&mut byte).await.is_err() {
                        return;
                    }
                    head.push(byte[0]);
                    if head.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                let head = String::from_utf8_lossy(&head);
                let accept = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("sec-websocket-key")
                            .then(|| value.trim().to_owned())
                    })
                    .map(|key| format!("sec-websocket-accept: {key}\r\n"))
                    .unwrap_or_default();
                let reply = format!(
                    "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n{accept}\r\n"
                );
                if socket.write_all(reply.as_bytes()).await.is_err() {
                    return;
                }
                let mut buffer = [0_u8; 4096];
                loop {
                    let read = socket.read(&mut buffer).await.unwrap_or(0);
                    if read == 0 || socket.write_all(&buffer[..read]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    address
}
