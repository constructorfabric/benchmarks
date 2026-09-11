//! Shared harness for the OAGW integration tests.
//!
//! Builds the real router through `api::routes::register_routes` and injects a
//! `SecurityContext` the way the gateway's auth middleware would.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    dead_code,
    reason = "the harness carries fixtures some suites in this directory do not use"
)]

use std::sync::Arc;

use axum::Router;
use axum::response::IntoResponse;
use http::{Method, StatusCode};
use tower::ServiceExt;
use utoipa::openapi::RefOr;
use utoipa::openapi::schema::Schema;

use oagw::api::error::ApiContext;
use oagw::api::routes::register_routes;
use oagw::config::OagwConfig;
use oagw::domain::service::ControlPlaneService;
use oagw::infra::http_client::build_client;
use oagw::infra::memory_repo::{
    MemoryPluginRepository, MemoryRouteRepository, MemoryUpstreamRepository,
};
use oagw::infra::plugin::oauth2_client_cred_auth::TokenCacheConfig;
use oagw::infra::plugin::{AuthPluginRegistry, GuardPluginRegistry, TransformPluginRegistry};
use oagw::infra::proxy::service::ProxyService;

/// `OpenApiRegistry` that discards every registration.
pub struct NoopOpenApiRegistry;

impl toolkit::api::OpenApiRegistry for NoopOpenApiRegistry {
    fn register_operation(&self, _spec: &toolkit::api::OperationSpec) {}

    fn ensure_schema_raw(&self, name: &str, _schemas: Vec<(String, RefOr<Schema>)>) -> String {
        name.to_owned()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// A credential store that always hands back one configured value.
pub struct FakeCredStore {
    secrets: std::collections::HashMap<String, String>,
}

impl FakeCredStore {
    /// A store holding a single secret.
    #[must_use]
    pub fn new(key: &str, value: &str) -> Self {
        Self {
            secrets: [(key.to_owned(), value.to_owned())].into_iter().collect(),
        }
    }
}

#[async_trait::async_trait]
impl credstore_sdk::CredStoreClientV1 for FakeCredStore {
    async fn get(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        key: &credstore_sdk::SecretRef,
    ) -> Result<Option<credstore_sdk::GetSecretResponse>, credstore_sdk::CredStoreError> {
        Ok(self
            .secrets
            .get(key.as_ref())
            .map(|value| credstore_sdk::GetSecretResponse {
                value: credstore_sdk::SecretValue::new(value.clone().into_bytes()),
                id: uuid::Uuid::nil(),
                owner_tenant_id: credstore_sdk::TenantId(uuid::Uuid::nil()),
                sharing: credstore_sdk::SharingMode::Private,
                is_inherited: false,
                version: 1,
                secret_type: "gts.cf.core.credstore.secret.v1~cf.core.string.v1".to_owned(),
                expires_at: None,
            }))
    }
}

/// Tenant used by every test request.
pub fn tenant() -> uuid::Uuid {
    uuid::Uuid::nil()
}

/// A `SecurityContext` for `tenant()`.
#[must_use]
pub fn security() -> toolkit_security::SecurityContext {
    toolkit_security::SecurityContext::builder()
        .subject_tenant_id(tenant())
        .subject_id(uuid::Uuid::nil())
        .build()
        .expect("security context builds")
}

/// The assembled test harness: the real router plus a JSON helper.
pub struct Harness {
    app: Router,
}

impl Harness {
    /// Build the router with an empty configuration and no credential store.
    #[must_use]
    pub fn new() -> Self {
        Self::with_config(OagwConfig::default(), None)
    }

    /// Build the router with the supplied configuration.
    #[must_use]
    pub fn with_config(
        config: OagwConfig,
        credstore: Option<Arc<dyn credstore_sdk::CredStoreClientV1>>,
    ) -> Self {
        let config = Arc::new(config);
        let control_plane = Arc::new(ControlPlaneService::new(
            Arc::new(MemoryUpstreamRepository::default()),
            Arc::new(MemoryRouteRepository::default()),
            Arc::new(MemoryPluginRepository::default()),
            config.allow_http_upstream,
        ));
        let credstore = credstore
            .unwrap_or_else(|| Arc::new(oagw::infra::plugin::absent_credstore::AbsentCredStore));
        let auth_plugins = AuthPluginRegistry::with_builtins(
            credstore,
            TokenCacheConfig {
                ttl: std::time::Duration::from_secs(30),
                capacity: 128,
            },
        );
        let proxy = Arc::new(ProxyService::new(
            Arc::clone(&control_plane),
            Arc::clone(&config),
            build_client(config.proxy_timeout()),
            auth_plugins,
            GuardPluginRegistry::with_builtins(),
            TransformPluginRegistry::with_builtins(),
        ));
        let context = Arc::new(ApiContext {
            control_plane,
            config,
            proxy,
            tenants: None,
        });
        let app = register_routes(Router::new(), &NoopOpenApiRegistry, context)
            .layer(axum::middleware::from_fn(inject_security));
        Self { app }
    }

    /// Send a request and return the response.
    ///
    /// # Panics
    /// Panics when the router fails to answer, which in these tests always
    /// means a programming error in the harness itself.
    pub async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<String>,
    ) -> axum::response::Response {
        let mut builder = http::Request::builder().method(method).uri(path);
        if let Some(body) = body {
            builder = builder.header(http::header::CONTENT_TYPE, "application/json");
            return self
                .app
                .clone()
                .oneshot(builder.body(axum::body::Body::from(body)).unwrap())
                .await
                .expect("router answers");
        }
        self.app
            .clone()
            .oneshot(builder.body(axum::body::Body::empty()).unwrap())
            .await
            .expect("router answers")
    }

    /// Send a JSON body.
    pub async fn post_json(&self, path: &str, body: serde_json::Value) -> axum::response::Response {
        self.send(Method::POST, path, Some(body.to_string())).await
    }

    /// Send a JSON body with `PUT`.
    pub async fn put_json(&self, path: &str, body: serde_json::Value) -> axum::response::Response {
        self.send(Method::PUT, path, Some(body.to_string())).await
    }

    /// Send a request with explicit headers and a raw body.
    pub async fn send_raw(
        &self,
        method: Method,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> axum::response::Response {
        let mut builder = http::Request::builder().method(method).uri(path);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let payload = body.map_or_else(axum::body::Body::default, |bytes| {
            axum::body::Body::from(bytes.to_vec())
        });
        self.app
            .clone()
            .oneshot(builder.body(payload).expect("request builds"))
            .await
            .expect("router answers")
    }

    /// Serve the router over a real TCP listener and return its address.
    ///
    /// Required for the upgrade tests: the relay needs a genuine client socket.
    #[must_use]
    pub async fn spawn(self) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener binds");
        let addr = listener.local_addr().expect("address resolves");
        tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, self.app.into_make_service()).await {
                eprintln!("gateway under test stopped: {err}");
            }
        });
        addr
    }

    /// Read the response body as bytes.
    ///
    /// # Panics
    /// Panics when the body cannot be buffered.
    pub async fn text(&self, response: axum::response::Response) -> Vec<u8> {
        let bytes = axum::body::to_bytes(
            response.into_body(),
            usize::try_from(oagw::config::OagwConfig::default().max_request_body_bytes)
                .unwrap_or(usize::MAX),
        )
        .await
        .expect("body buffers");
        bytes.to_vec()
    }

    /// Read the response body as a JSON value.
    pub async fn json(&self, response: axum::response::Response) -> serde_json::Value {
        serde_json::from_slice(&self.text(response).await).expect("body is JSON")
    }

    /// Assert a status and return the parsed problem document.
    pub async fn expect_problem(
        &self,
        response: axum::response::Response,
        status: StatusCode,
    ) -> serde_json::Value {
        assert_eq!(response.status(), status, "unexpected status: {response:?}");
        let value = self.json(response).await;
        assert!(value.get("type").is_some(), "problem carries a type");
        assert!(value.get("title").is_some(), "problem carries a title");
        value
    }
}

impl Default for Harness {
    fn default() -> Self {
        Self::new()
    }
}

/// Inject the security context the gateway middleware would.
async fn inject_security(
    mut request: http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    request.extensions_mut().insert(security());
    next.run(request).await
}

/// A well-formed upstream document carrying the alias its pool derives.
#[must_use]
pub fn upstream_body(host: &str, port: u16) -> serde_json::Value {
    let alias = if port == standard_https_port() {
        host.to_owned()
    } else {
        format!("{host}:{port}")
    };
    serde_json::json!({
        "alias": alias,
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "server": { "endpoints": [ { "scheme": "https", "host": host, "port": port } ] }
    })
}

/// Port a TLS endpoint needs no port suffix for.
#[must_use]
pub const fn standard_https_port() -> u16 {
    443
}

/// A well-formed route body.
#[must_use]
pub fn route_body(upstream_id: &str, path: &str) -> serde_json::Value {
    route_body_with(upstream_id, &["GET"], path)
}

/// A well-formed route body with an explicit method allowlist.
#[must_use]
pub fn route_body_with(upstream_id: &str, methods: &[&str], path: &str) -> serde_json::Value {
    serde_json::json!({
        "upstream_id": upstream_id,
        "match": { "http": { "methods": methods, "path": path } }
    })
}

/// Create an upstream and return its wire identifier.
pub async fn create_upstream(harness: &Harness, body: serde_json::Value) -> serde_json::Value {
    let response = harness.post_json("/oagw/v1/upstreams", body).await;
    let document = harness.json(response).await;
    assert_eq!(
        document["status"]
            .as_u64()
            .map_or(StatusCode::CREATED, |_| StatusCode::BAD_REQUEST),
        StatusCode::CREATED,
        "create upstream failed: {document}"
    );
    document
}

/// Create a route and return its wire identifier.
pub async fn create_route(harness: &Harness, body: serde_json::Value) -> serde_json::Value {
    let response = harness.post_json("/oagw/v1/routes", body).await;
    let document = harness.json(response).await;
    assert_eq!(
        document["status"]
            .as_u64()
            .map_or(StatusCode::CREATED, |_| StatusCode::BAD_REQUEST),
        StatusCode::CREATED,
        "create route failed: {document}"
    );
    document
}

/// Gateway configuration that admits plaintext upstreams.
#[must_use]
pub fn permissive_config() -> OagwConfig {
    OagwConfig {
        allow_http_upstream: true,
        ..OagwConfig::default()
    }
}

/// A real HTTP upstream the proxy can be pointed at.
///
/// Serves `/echo` (mirrors the request back as JSON), `/stream` (three SSE
/// events) and `/ws` (a WebSocket echo channel).
pub struct MockUpstream {
    /// Address the application listens on.
    pub addr: std::net::SocketAddr,
}

impl MockUpstream {
    /// Spawn the upstream on an ephemeral port.
    ///
    /// The upstream runs on its own single-threaded runtime so the test's
    /// runtime stays free to drive the gateway.
    #[must_use]
    pub fn start() -> Self {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("upstream runtime builds");
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("listener binds");
                let addr = listener.local_addr().expect("address resolves");
                sender.send(addr).expect("address is sent");
                if let Err(err) = axum::serve(listener, mock_app()).await {
                    eprintln!("mock upstream stopped: {err}");
                }
            });
        });
        let addr = receiver.recv().expect("address is reported");
        Self { addr }
    }

    /// The `http` endpoint document pointing at the application.
    #[must_use]
    pub fn endpoint(&self) -> serde_json::Value {
        serde_json::json!({
            "scheme": "http",
            "host": "127.0.0.1",
            "port": self.addr.port()
        })
    }

    /// An upstream document exposing the application under `alias`.
    #[must_use]
    pub fn upstream_body(&self, alias: &str) -> serde_json::Value {
        serde_json::json!({
            "alias": alias,
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "server": { "endpoints": [ self.endpoint() ] }
        })
    }
}

/// The routes served by [`MockUpstream`].
fn mock_app() -> Router {
    use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
    use axum::routing::{any, get};
    use futures_util::{SinkExt, StreamExt};

    async fn echo(
        method: http::Method,
        uri: axum::http::Uri,
        headers: http::HeaderMap,
        body: axum::body::Bytes,
    ) -> axum::response::Response {
        let mut seen = serde_json::Map::new();
        for (name, value) in &headers {
            if let Ok(value) = value.to_str() {
                seen.insert(
                    name.as_str().to_owned(),
                    serde_json::Value::String(value.to_owned()),
                );
            }
        }
        (
            StatusCode::OK,
            axum::Json(serde_json::json!({
                "method": method.as_str(),
                "path": uri.path(),
                "query": uri.query(),
                "headers": seen,
                "body": String::from_utf8_lossy(&body),
            })),
        )
            .into_response()
    }

    async fn stream() -> axum::response::Response {
        let events = vec![
            Ok::<_, std::io::Error>("data: one\n\n".to_owned()),
            Ok("data: two\n\n".to_owned()),
            Ok("data: three\n\n".to_owned()),
        ];
        axum::response::Response::builder()
            .header(http::header::CONTENT_TYPE, "text/event-stream")
            .body(axum::body::Body::from_stream(futures_util::stream::iter(
                events,
            )))
            .expect("stream response builds")
    }

    async fn upgrade(ws: WebSocketUpgrade) -> axum::response::Response {
        ws.on_upgrade(|socket: WebSocket| async move {
            let (mut sink, mut source) = StreamExt::split(socket);
            while let Some(Ok(message)) = source.next().await {
                if let Message::Text(text) = message {
                    let reply = Message::Text(format!("echo:{text}").into());
                    if sink.send(reply).await.is_err() {
                        break;
                    }
                }
            }
        })
    }

    async fn close_after_upgrade(ws: WebSocketUpgrade) -> axum::response::Response {
        ws.on_upgrade(|socket: WebSocket| async move {
            drop(socket);
        })
    }

    Router::new()
        .route("/echo", any(echo))
        .route("/stream", get(stream))
        .route("/ws", get(upgrade))
        .route("/ws-close", get(close_after_upgrade))
}

/// Proxy path for an alias and a suffix.
#[must_use]
pub fn proxy_path(alias: &str, suffix: &str) -> String {
    format!("/oagw/v1/proxy/{alias}{suffix}")
}
