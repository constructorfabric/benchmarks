//! In-process harness for the integration suite (feature `test-utils`).
//!
//! [`TestGateway`] stands the whole gear up behind a real TCP listener —
//! Control Plane, Data Plane, plugin registries and the Axum router — with
//! the `SecurityContext` the api-gateway's auth layer would normally inject
//! supplied directly. A real socket matters: SSE and WebSocket behaviour is
//! only observable over one.
//!
//! [`MockUpstream`] is the other end: an echo/SSE/WebSocket server that
//! records what OAGW actually forwarded.

use std::any::Any;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::{IntoResponse, Response, Sse, sse::Event};
use axum::routing::{any, get};
use credstore_sdk::CredStoreClientV1;
use futures_util::StreamExt;
use tokio::net::TcpListener;
use toolkit::api::{OpenApiRegistry, OperationSpec};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::OagwState;
use crate::config::{OagwConfig, SsrfPolicyConfig};
use crate::domain::services::management::{ControlPlaneService, ControlPlaneServiceImpl};
use crate::domain::services::proxy::DataPlaneService;
use crate::domain::services::tenancy::TenantHierarchy;
use crate::infra::metrics::OagwMetrics;
use crate::infra::plugin::{PluginRegistries, TokenCacheConfig};
use crate::infra::proxy::DataPlaneServiceImpl;
use crate::infra::storage::{InMemoryPluginRepo, InMemoryRouteRepo, InMemoryUpstreamRepo};

/// `OpenApiRegistry` that records nothing — route registration needs one, the
/// integration suite does not care what it says.
#[derive(Debug, Default)]
pub struct NoopOpenApi;

impl OpenApiRegistry for NoopOpenApi {
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

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// A running OAGW instance.
pub struct TestGateway {
    /// Base URL, e.g. `http://127.0.0.1:34567`.
    pub base_url: String,
    /// Bound address.
    pub addr: SocketAddr,
    /// The Control Plane, for seeding configuration directly.
    pub control_plane: Arc<dyn ControlPlaneService>,
    /// The security context every request runs under.
    pub security_context: SecurityContext,
}

/// Builder for [`TestGateway`].
pub struct TestGatewayBuilder {
    config: OagwConfig,
    credstore: Option<Arc<dyn CredStoreClientV1>>,
    hierarchy: Option<Arc<dyn TenantHierarchy>>,
    tenant_id: Uuid,
    subject_id: Uuid,
}

impl Default for TestGatewayBuilder {
    fn default() -> Self {
        Self {
            config: OagwConfig {
                // Loopback mock upstreams over plaintext are the whole point
                // of the harness.
                allow_http_upstream: true,
                ssrf_policy: SsrfPolicyConfig {
                    enabled: false,
                    ..SsrfPolicyConfig::default()
                },
                proxy_timeout_secs: 5,
                ..OagwConfig::default()
            },
            credstore: None,
            hierarchy: None,
            tenant_id: Uuid::new_v4(),
            subject_id: Uuid::new_v4(),
        }
    }
}

impl TestGatewayBuilder {
    /// Start from the defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the gear configuration.
    #[must_use]
    pub fn config(mut self, config: OagwConfig) -> Self {
        self.config = config;
        self
    }

    /// Seed the credential store with `(reference, value)` pairs.
    #[must_use]
    pub fn secrets(mut self, pairs: Vec<(&str, &str)>) -> Self {
        self.credstore = Some(Arc::new(
            credstore_sdk::test_util::MockCredStoreClient::with_secrets(
                pairs
                    .into_iter()
                    .map(|(k, v)| (k.to_owned(), v.to_owned()))
                    .collect(),
            ),
        ));
        self
    }

    /// Supply a tenant hierarchy.
    #[must_use]
    pub fn hierarchy(mut self, hierarchy: Arc<dyn TenantHierarchy>) -> Self {
        self.hierarchy = Some(hierarchy);
        self
    }

    /// Run requests as this tenant.
    #[must_use]
    pub fn tenant(mut self, tenant_id: Uuid) -> Self {
        self.tenant_id = tenant_id;
        self
    }

    /// Bind a listener and serve. The server task ends when the process does.
    ///
    /// # Panics
    ///
    /// Panics if a loopback listener cannot be bound — the harness has no
    /// meaningful fallback.
    pub async fn start(self) -> TestGateway {
        let credstore = self.credstore.unwrap_or_else(|| {
            Arc::new(credstore_sdk::test_util::MockCredStoreClient::empty())
        });
        let hierarchy = self
            .hierarchy
            .unwrap_or_else(|| Arc::new(crate::domain::services::tenancy::FlatHierarchy));

        let control_plane: Arc<dyn ControlPlaneService> = Arc::new(ControlPlaneServiceImpl::new(
            Arc::new(InMemoryUpstreamRepo::new()),
            Arc::new(InMemoryRouteRepo::new()),
            Arc::new(InMemoryPluginRepo::new()),
            hierarchy,
            self.config.plugin_gc_ttl_secs,
            self.config.allow_http_upstream,
        ));
        let registries = Arc::new(PluginRegistries::with_builtins(
            credstore,
            TokenCacheConfig {
                ttl: self.config.token_cache_ttl(),
                capacity: self.config.token_cache_capacity,
            },
        ));
        let data_plane: Arc<dyn DataPlaneService> = Arc::new(DataPlaneServiceImpl::new(
            Arc::clone(&control_plane),
            registries,
            Arc::new(OagwMetrics::from_global()),
            self.config.clone(),
        ));

        let state = Arc::new(OagwState {
            control_plane: Arc::clone(&control_plane),
            data_plane,
            // Authorization is the api-gateway's PDP in production; the
            // harness exercises OAGW's own behaviour.
            enforcer: None,
        });

        let security_context = SecurityContext::builder()
            .subject_id(self.subject_id)
            .subject_tenant_id(self.tenant_id)
            .build()
            .unwrap_or_else(|_| SecurityContext::anonymous());

        let injected = security_context.clone();
        let router = crate::api::rest::register_routes(Router::new(), &NoopOpenApi, state).layer(
            axum::middleware::from_fn(
                move |mut req: axum::extract::Request, next: axum::middleware::Next| {
                    let ctx = injected.clone();
                    async move {
                        req.extensions_mut().insert(ctx);
                        next.run(req).await
                    }
                },
            ),
        );

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback listener for the test gateway");
        let addr = listener.local_addr().expect("listener address");
        tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await;
        });

        TestGateway {
            base_url: format!("http://{addr}"),
            addr,
            control_plane,
            security_context,
        }
    }
}

/// A stand-in upstream service.
pub struct MockUpstream {
    /// Bound address.
    pub addr: SocketAddr,
}

impl MockUpstream {
    /// Bind and serve on a loopback port.
    ///
    /// Routes:
    ///
    /// | Path            | Behaviour                                        |
    /// |-----------------|--------------------------------------------------|
    /// | `/v1/echo/*`    | JSON echo of method, path, query and headers     |
    /// | `/v1/sse`       | Three server-sent events, then close             |
    /// | `/v1/ws`        | WebSocket echo, prefixing `echo:`                |
    /// | `/v1/status/{code}` | Respond with that status                     |
    /// | `/v1/slow`      | Sleep two seconds, then echo                     |
    /// | `/v1/nocontenttype` | `200` with no `Content-Type`                 |
    ///
    /// # Panics
    ///
    /// Panics if a loopback listener cannot be bound.
    pub async fn start() -> Self {
        let router = Router::new()
            .route("/v1/sse", get(sse_handler))
            .route("/v1/ws", get(ws_handler))
            .route("/v1/status/{code}", any(status_handler))
            .route("/v1/slow", any(slow_handler))
            .route("/v1/nocontenttype", any(no_content_type_handler))
            .fallback(any(echo_handler));

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback listener for the mock upstream");
        let addr = listener.local_addr().expect("listener address");
        tokio::spawn(async move {
            let _ = axum::serve(listener, router.into_make_service()).await;
        });
        Self { addr }
    }

    /// Host part of the bound address.
    #[must_use]
    pub fn host(&self) -> String {
        self.addr.ip().to_string()
    }

    /// Port part of the bound address.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.addr.port()
    }
}

async fn echo_handler(request: axum::extract::Request) -> Response {
    let method = request.method().as_str().to_owned();
    let path = request.uri().path().to_owned();
    let query = request.uri().query().unwrap_or_default().to_owned();
    let headers: serde_json::Map<String, serde_json::Value> = request
        .headers()
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_owned(),
                serde_json::Value::String(v.to_str().unwrap_or_default().to_owned()),
            )
        })
        .collect();
    let body = axum::body::to_bytes(request.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap_or_default();

    axum::Json(serde_json::json!({
        "method": method,
        "path": path,
        "query": query,
        "headers": headers,
        "body": String::from_utf8_lossy(&body),
    }))
    .into_response()
}

async fn sse_handler() -> Response {
    let stream = async_stream::stream! {
        for index in 0..3 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            yield Ok::<Event, std::convert::Infallible>(Event::default().data(format!("event-{index}")));
        }
    };
    Sse::new(stream).into_response()
}

async fn ws_handler(upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(ws_echo)
}

async fn ws_echo(mut socket: WebSocket) {
    while let Some(Ok(message)) = socket.next().await {
        match message {
            Message::Text(text) => {
                if socket
                    .send(Message::Text(format!("echo:{text}").into()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Message::Close(_) => return,
            _ => {}
        }
    }
}

async fn status_handler(axum::extract::Path(code): axum::extract::Path<u16>) -> Response {
    let status = http::StatusCode::from_u16(code).unwrap_or(http::StatusCode::OK);
    (status, axum::Json(serde_json::json!({ "upstream": "said", "code": code }))).into_response()
}

async fn slow_handler() -> Response {
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    axum::Json(serde_json::json!({ "slow": true })).into_response()
}

async fn no_content_type_handler() -> Response {
    let mut response = axum::body::Body::from("no content type here").into_response();
    response.headers_mut().remove(http::header::CONTENT_TYPE);
    response
}
