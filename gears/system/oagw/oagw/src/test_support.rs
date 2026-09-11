//! Test scaffolding: an in-process gateway harness and a configurable mock
//! upstream.
//!
//! Available to this crate's unit tests and, under the `test-utils` feature,
//! to its integration tests. Not compiled into a release build.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::response::{IntoResponse, Response};
use credstore_sdk::CredStoreClientV1;
use credstore_sdk::test_util::MockCredStoreClient;
use http::{Request, StatusCode};
use serde_json::Value;
use toolkit::api::{OpenApiRegistry, OperationSpec};
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

use crate::api::rest::routes::register_routes;
use crate::config::{OagwConfig, SsrfPolicyConfig};
use crate::domain::ports::TenantDirectory;
use crate::domain::services::management::ControlPlaneService;
use crate::infra::metrics::OagwMetrics;
use crate::infra::plugin::TokenCacheConfig;
use crate::infra::plugin::registry::PluginRegistries;
use crate::infra::proxy::circuit::CircuitBreakerRegistry;
use crate::infra::proxy::connector::UpstreamConnector;
use crate::infra::proxy::service::DataPlaneService;
use crate::infra::ratelimit::RateLimiterRegistry;
use crate::infra::storage::{MemoryPluginRepo, MemoryRouteRepo, MemoryStore, MemoryUpstreamRepo};

/// Credential seeded into every harness, mirroring `config/e2e-local.yaml`.
pub const TEST_SECRET_REF: &str = "openai-key";
/// Value of [`TEST_SECRET_REF`]. Test-only fake, not a real credential.
pub const TEST_SECRET_VALUE: &str = "sk-test-e2e-fake-key";

/// An `OpenApiRegistry` that records nothing.
#[derive(Debug, Default)]
pub struct NoopRegistry;

impl OpenApiRegistry for NoopRegistry {
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

/// A tenant directory driven by an explicit `child -> parent` edge list.
#[derive(Debug, Default)]
pub struct StaticTenantDirectory {
    parents: HashMap<Uuid, Uuid>,
}

impl StaticTenantDirectory {
    /// Build a directory from `child -> parent` edges.
    #[must_use]
    pub fn new(edges: Vec<(Uuid, Uuid)>) -> Self {
        Self {
            parents: edges.into_iter().collect(),
        }
    }
}

#[async_trait]
impl TenantDirectory for StaticTenantDirectory {
    async fn chain(&self, _ctx: &SecurityContext, tenant_id: Uuid) -> Vec<Uuid> {
        let mut chain = vec![tenant_id];
        let mut current = tenant_id;
        // The edge list is finite and acyclic in tests; the visited guard is
        // belt-and-braces so a mistake in a fixture cannot hang a test.
        while let Some(parent) = self.parents.get(&current) {
            if chain.contains(parent) {
                break;
            }
            chain.push(*parent);
            current = *parent;
        }
        chain
    }
}

/// Gear configuration matching `config/e2e-local.yaml`: plaintext upstreams
/// permitted, SSRF guard off (the mock upstream is on loopback).
#[must_use]
pub fn test_config() -> OagwConfig {
    OagwConfig {
        proxy_timeout_secs: 2,
        allow_http_upstream: true,
        ssrf_policy: SsrfPolicyConfig {
            enabled: false,
            ..SsrfPolicyConfig::default()
        },
        ..OagwConfig::default()
    }
}

/// A Control Plane over an empty in-memory store with a flat hierarchy.
#[must_use]
pub fn control_plane() -> Arc<ControlPlaneService> {
    Harness::new().control_plane
}

/// A Data Plane bound to `control_plane`.
#[must_use]
pub fn data_plane(control_plane: &Arc<ControlPlaneService>) -> Arc<DataPlaneService> {
    let config = test_config();
    let registries = Arc::new(PluginRegistries::with_builtins(
        Arc::new(MockCredStoreClient::empty()),
        TokenCacheConfig::default(),
    ));
    Arc::new(DataPlaneService::new(
        Arc::clone(control_plane),
        registries,
        Arc::new(UpstreamConnector::new(&config)),
        Arc::new(RateLimiterRegistry::new()),
        Arc::new(CircuitBreakerRegistry::new()),
        Arc::new(OagwMetrics::from_global()),
        config,
    ))
}

/// An in-process OAGW: the real router, the real services, an in-memory
/// store, and a caller identity injected the way the API gateway's auth
/// middleware would.
pub struct Harness {
    /// Control Plane.
    pub control_plane: Arc<ControlPlaneService>,
    /// Data Plane.
    pub data_plane: Arc<DataPlaneService>,
    /// Backing store, for assertions that bypass the API.
    pub store: Arc<MemoryStore>,
    router: Router,
}

/// How a [`Harness`] is put together.
pub struct HarnessBuilder {
    config: OagwConfig,
    edges: Vec<(Uuid, Uuid)>,
    secrets: Vec<(String, String)>,
}

impl Default for HarnessBuilder {
    fn default() -> Self {
        Self {
            config: test_config(),
            edges: Vec::new(),
            secrets: vec![(TEST_SECRET_REF.to_owned(), TEST_SECRET_VALUE.to_owned())],
        }
    }
}

impl HarnessBuilder {
    /// Override the gear configuration.
    #[must_use]
    pub fn config(mut self, config: OagwConfig) -> Self {
        self.config = config;
        self
    }

    /// Declare `child -> parent` tenant edges.
    #[must_use]
    pub fn hierarchy(mut self, edges: Vec<(Uuid, Uuid)>) -> Self {
        self.edges = edges;
        self
    }

    /// Seed an extra credential.
    #[must_use]
    pub fn secret(mut self, reference: &str, value: &str) -> Self {
        self.secrets.push((reference.to_owned(), value.to_owned()));
        self
    }

    /// Build the harness.
    #[must_use]
    pub fn build(self) -> Harness {
        let store = MemoryStore::shared();
        let credstore: Arc<dyn CredStoreClientV1> =
            Arc::new(MockCredStoreClient::with_secrets(self.secrets));
        let registries = Arc::new(PluginRegistries::with_builtins(
            Arc::clone(&credstore),
            TokenCacheConfig::default(),
        ));
        let control_plane = Arc::new(ControlPlaneService::new(
            Arc::new(MemoryUpstreamRepo::new(Arc::clone(&store))),
            Arc::new(MemoryRouteRepo::new(Arc::clone(&store))),
            Arc::new(MemoryPluginRepo::new(Arc::clone(&store))),
            Arc::clone(&registries) as Arc<dyn crate::domain::ports::PluginCatalog>,
            Arc::new(StaticTenantDirectory::new(self.edges)),
        ));
        let data_plane = Arc::new(DataPlaneService::new(
            Arc::clone(&control_plane),
            registries,
            Arc::new(UpstreamConnector::new(&self.config)),
            Arc::new(RateLimiterRegistry::new()),
            Arc::new(CircuitBreakerRegistry::new()),
            Arc::new(OagwMetrics::from_global()),
            self.config,
        ));
        let router = register_routes(
            Router::new(),
            &NoopRegistry,
            Arc::clone(&control_plane),
            Arc::clone(&data_plane),
        );
        Harness {
            control_plane,
            data_plane,
            store,
            router,
        }
    }
}

impl Harness {
    /// A harness with the default configuration and a flat hierarchy.
    #[must_use]
    pub fn new() -> Self {
        HarnessBuilder::default().build()
    }

    /// Start configuring a harness.
    #[must_use]
    pub fn builder() -> HarnessBuilder {
        HarnessBuilder::default()
    }

    /// Send a request as `ctx`.
    pub async fn send(&self, ctx: &SecurityContext, request: Request<Body>) -> Response {
        self.router
            .clone()
            .layer(axum::Extension(ctx.clone()))
            .oneshot(request)
            .await
            .unwrap_or_else(|err| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("router error: {err}"),
                )
                    .into_response()
            })
    }

    /// `POST` a JSON body.
    pub async fn post_json(&self, ctx: &SecurityContext, path: &str, body: &Value) -> Response {
        self.send(ctx, json_request(http::Method::POST, path, body))
            .await
    }

    /// `PUT` a JSON body.
    pub async fn put_json(&self, ctx: &SecurityContext, path: &str, body: &Value) -> Response {
        self.send(ctx, json_request(http::Method::PUT, path, body))
            .await
    }

    /// `GET` a path.
    pub async fn get(&self, ctx: &SecurityContext, path: &str) -> Response {
        self.send(ctx, empty_request(http::Method::GET, path)).await
    }

    /// `DELETE` a path.
    pub async fn delete(&self, ctx: &SecurityContext, path: &str) -> Response {
        self.send(ctx, empty_request(http::Method::DELETE, path))
            .await
    }

    /// Serve the gateway on an ephemeral loopback port, with `ctx` as the
    /// caller identity.
    ///
    /// `oneshot` cannot carry a protocol upgrade — there is no socket to hand
    /// back — so upgrade tests need a real listener.
    pub async fn serve(&self, ctx: &SecurityContext) -> SocketAddr {
        let router = self.router.clone().layer(axum::Extension(ctx.clone()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the gateway");
        let addr = listener.local_addr().expect("local address");
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        addr
    }
}

impl Default for Harness {
    fn default() -> Self {
        Self::new()
    }
}

/// Build a `SecurityContext` for `tenant`.
#[must_use]
pub fn context_for(tenant: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(tenant)
        .build()
        .unwrap_or_else(|_| SecurityContext::anonymous())
}

/// A request with a JSON body.
#[must_use]
pub fn json_request(method: http::Method, path: &str, body: &Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap_or_else(|_| Request::new(Body::empty()))
}

/// A request with no body.
#[must_use]
pub fn empty_request(method: http::Method, path: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .body(Body::empty())
        .unwrap_or_else(|_| Request::new(Body::empty()))
}

/// Read a response body as JSON.
pub async fn read_json(response: Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap_or_default();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

/// Read a response body as text.
pub async fn read_text(response: Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap_or_default();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// One request the mock upstream saw.
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    /// Method.
    pub method: String,
    /// Path and query, as received.
    pub uri: String,
    /// Headers, lowercase names.
    pub headers: HashMap<String, String>,
    /// Body, as UTF-8.
    pub body: String,
}

impl RecordedRequest {
    /// Header value, by lowercase name.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }
}

/// A local HTTP server standing in for an external service.
///
/// Endpoints: `/v1/models` (JSON), `/v1/echo` (mirrors the request),
/// `/v1/sse` (three server-sent events), `/v1/ws` (WebSocket echo),
/// `/v1/slow` (sleeps past the proxy timeout), `/v1/boom` (500 with a JSON
/// body) and `/v1/bare` (200 with no `Content-Type`).
pub struct MockUpstream {
    addr: SocketAddr,
    recorded: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl MockUpstream {
    /// Bind on an ephemeral loopback port and start serving.
    pub async fn start() -> Self {
        let recorded: Arc<Mutex<Vec<RecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the mock upstream");
        let addr = listener.local_addr().expect("local address");
        let router = mock_router(Arc::clone(&recorded));
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Self { addr, recorded }
    }

    /// Bound address.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Bound host, as an endpoint `host` value.
    #[must_use]
    pub fn host(&self) -> String {
        self.addr.ip().to_string()
    }

    /// Bound port.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// Every request seen so far.
    #[must_use]
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.recorded
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// The most recent request, if any.
    #[must_use]
    pub fn last_request(&self) -> Option<RecordedRequest> {
        self.requests().pop()
    }
}

fn mock_router(recorded: Arc<Mutex<Vec<RecordedRequest>>>) -> Router {
    use axum::routing::{any, get};

    Router::new()
        .route("/v1/sse", get(mock_sse))
        .route("/v1/ws", any(mock_ws))
        .route("/v1/slow", any(mock_slow))
        .route("/v1/boom", any(mock_boom))
        .route("/v1/bare", any(mock_bare))
        .fallback(any(mock_echo))
        .layer(axum::Extension(recorded))
}

async fn record(recorded: &Arc<Mutex<Vec<RecordedRequest>>>, request: axum::extract::Request) {
    let method = request.method().to_string();
    let uri = request
        .uri()
        .path_and_query()
        .map(ToString::to_string)
        .unwrap_or_else(|| request.uri().path().to_owned());
    let headers = request
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_ascii_lowercase(),
                value.to_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    let body = axum::body::to_bytes(request.into_body(), 4 * 1024 * 1024)
        .await
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    if let Ok(mut guard) = recorded.lock() {
        guard.push(RecordedRequest {
            method,
            uri,
            headers,
            body,
        });
    }
}

async fn mock_echo(
    axum::Extension(recorded): axum::Extension<Arc<Mutex<Vec<RecordedRequest>>>>,
    request: axum::extract::Request,
) -> Response {
    record(&recorded, request).await;
    let last = recorded.lock().ok().and_then(|guard| guard.last().cloned());
    let payload = last.map_or_else(
        || serde_json::json!({ "upstream": "mock" }),
        |entry| {
            serde_json::json!({
                "upstream": "mock",
                "method": entry.method,
                "uri": entry.uri,
                "headers": entry.headers,
                "body": entry.body,
            })
        },
    );
    (StatusCode::OK, axum::Json(payload)).into_response()
}

async fn mock_sse(
    axum::Extension(recorded): axum::Extension<Arc<Mutex<Vec<RecordedRequest>>>>,
    request: axum::extract::Request,
) -> Response {
    record(&recorded, request).await;
    let stream = async_stream::stream! {
        for index in 0..3u8 {
            yield Ok::<_, std::io::Error>(axum::body::Bytes::from(format!(
                "event: tick\ndata: {index}\n\n"
            )));
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        yield Ok(axum::body::Bytes::from_static(b"event: done\ndata: bye\n\n"));
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(http::header::CONTENT_TYPE, "text/event-stream")
        .header(http::header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

async fn mock_ws(upgrade: axum::extract::ws::WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(|mut socket| async move {
        use axum::extract::ws::Message;
        while let Some(Ok(message)) = socket.recv().await {
            match message {
                Message::Text(text) => {
                    if socket
                        .send(Message::Text(format!("echo:{text}").into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    })
}

async fn mock_slow(
    axum::Extension(recorded): axum::Extension<Arc<Mutex<Vec<RecordedRequest>>>>,
    request: axum::extract::Request,
) -> Response {
    record(&recorded, request).await;
    tokio::time::sleep(Duration::from_secs(30)).await;
    StatusCode::OK.into_response()
}

async fn mock_boom(
    axum::Extension(recorded): axum::Extension<Arc<Mutex<Vec<RecordedRequest>>>>,
    request: axum::extract::Request,
) -> Response {
    record(&recorded, request).await;
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        axum::Json(serde_json::json!({ "error": "upstream exploded" })),
    )
        .into_response()
}

async fn mock_bare(
    axum::Extension(recorded): axum::Extension<Arc<Mutex<Vec<RecordedRequest>>>>,
    request: axum::extract::Request,
) -> Response {
    record(&recorded, request).await;
    let mut response = StatusCode::OK.into_response();
    response.headers_mut().remove(http::header::CONTENT_TYPE);
    response
}

// -- fixture builders -------------------------------------------------------

/// A minimal upstream body pointing at `upstream`.
#[must_use]
pub fn plaintext_upstream_body(upstream: &MockUpstream) -> Value {
    serde_json::json!({
        "server": {
            "endpoints": [
                { "scheme": "http", "host": upstream.host(), "port": upstream.port() }
            ]
        },
        "protocol": crate::domain::gts_helpers::PROTOCOL_HTTP,
        "alias": "mock-upstream"
    })
}

/// A route body for `upstream_id` matching `methods` on `path`.
#[must_use]
pub fn route_body(upstream_id: &str, path: &str, methods: &[&str]) -> Value {
    serde_json::json!({
        "upstream_id": upstream_id,
        "match": {
            "http": {
                "methods": methods,
                "path": path
            }
        }
    })
}
