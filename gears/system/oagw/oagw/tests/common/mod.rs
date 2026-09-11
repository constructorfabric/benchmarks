//! Shared harness for the in-process integration tests.
//!
//! The gear's own axum router is driven through `tower::ServiceExt::oneshot`,
//! with a `httpmock` server standing in for the upstream so the proxy leg is
//! exercised over a real HTTP connection. No `Cargo` feature of the gear is
//! required beyond what the crate already declares as a dev-dependency.

#![allow(clippy::expect_used, clippy::unwrap_used)]
// Each integration binary uses the subset of the harness it needs.
#![allow(dead_code)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use httpmock::prelude::MockServer;
use tower::ServiceExt;
use uuid::Uuid;

use oagw::api::rest::handlers::proxy::{ProxyState, Shared};
use oagw::api::rest::routes::register_routes;

use credstore_sdk::test_util::MockCredStoreClient;
use oagw::domain::plugin::registry::{
    AuthPluginRegistry, GuardPluginRegistry, TransformPluginRegistry,
};
use oagw::domain::ratelimit::RateLimiter;
use oagw::domain::services::management::ControlPlaneService;
use oagw::domain::services::proxy::DataPlane;
use oagw::infra::credstore::CredStoreResolver;
use oagw::infra::memory_repo::{MemoryPluginRepo, MemoryRouteRepo, MemoryUpstreamRepo};
use oagw::infra::metrics::Metrics;
use oagw::infra::proxy::outbound::OutboundClient;
use oagw::infra::proxy::service::DataPlaneServiceImpl;
use oagw::infra::proxy::token_fetcher::HttpTokenFetcher;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationSpec;

/// A registry that accepts every schema without recording it.
pub struct NoopOpenApiRegistry;

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

/// The full gear wiring the integration tests drive.
pub struct Harness {
    /// The router carrying every OAGW route.
    pub router: Router,
    /// The management service, for seeding state directly when needed.
    pub control: Arc<ControlPlaneService>,
    /// The data plane, for assertions about its configuration.
    pub plane: Arc<DataPlaneServiceImpl>,
    /// The gear's counters, for assertions about streaming behaviour.
    pub metrics: Arc<Metrics>,
    /// The upstream repository, for direct seeding and cleanup.
    pub upstreams: Arc<MemoryUpstreamRepo>,
    /// The route repository, for direct seeding and cleanup.
    pub routes: Arc<MemoryRouteRepo>,
}

impl Harness {
    /// Builds the harness over a fresh set of repositories.
    pub fn new(server: &MockServer) -> Self {
        let harness = Self::assemble();
        let _ = server; // the caller keeps the mock server alive for the test
        harness
    }

    /// Builds a harness whose upstreams the test brings itself.
    #[must_use]
    pub fn without_mock() -> Self {
        Self::assemble()
    }

    /// Wires the whole gear over fresh repositories.
    fn assemble() -> Self {
        let upstreams = Arc::new(MemoryUpstreamRepo::new());
        let route_repo = Arc::new(MemoryRouteRepo::new());
        let plugins = Arc::new(MemoryPluginRepo::new());

        let control = Arc::new(ControlPlaneService::new(
            upstreams.clone(),
            route_repo.clone(),
            plugins,
        ));

        let credentials = Arc::new(CredStoreResolver::new(Some(Arc::new(
            MockCredStoreClient::with_secrets(vec![
                ("openai-key".to_owned(), "sk-secret-value".to_owned()),
                ("client-id".to_owned(), "cid-123".to_owned()),
                ("client-secret".to_owned(), "sekret".to_owned()),
            ]),
        ))));

        let outbound = OutboundClient::new(std::time::Duration::from_secs(10), true);
        let token_fetcher = Arc::new(HttpTokenFetcher::new(outbound.clone()));
        let auth_plugins = AuthPluginRegistry::with_builtins(
            &(token_fetcher as Arc<dyn oagw::domain::plugin::oauth2_client_cred::TokenFetcher>),
        );
        let guard_plugins = GuardPluginRegistry::with_builtins();
        let transform_plugins = TransformPluginRegistry::with_builtins();

        let metrics = Arc::new(Metrics::new());
        let limiter = Arc::new(RateLimiter::new());

        let plane = Arc::new(DataPlaneServiceImpl::new(
            upstreams.clone(),
            route_repo.clone(),
            credentials,
            auth_plugins,
            guard_plugins,
            transform_plugins,
            limiter,
            outbound,
            metrics.clone(),
        ));

        let plane_dyn: Arc<dyn DataPlane> = plane.clone();
        let proxy_state: Shared = Arc::new(ProxyState {
            plane: plane_dyn,
            metrics: metrics.clone(),
            anonymous_tenant: Uuid::nil(),
        });

        let openapi = NoopOpenApiRegistry;
        let router = register_routes(Router::new(), &openapi, control.clone(), proxy_state);

        Self {
            router,
            control,
            plane,
            metrics,
            upstreams,
            routes: route_repo,
        }
    }

    /// Sends a request to the router and returns the raw response.
    pub async fn send(&self, request: Request<Body>) -> axum::http::Response<Body> {
        self.router
            .clone()
            .oneshot(request)
            .await
            .expect("the router answers")
    }

    /// Sends a JSON request and returns the status plus the parsed body.
    pub async fn json(
        &self,
        method: &str,
        uri: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let response = self.send(request(method, uri, body)).await;
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let parsed = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, parsed)
    }

    /// Sends a JSON request and returns the status, parsed body and headers.
    pub async fn json_with_headers(
        &self,
        method: &str,
        uri: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value, axum::http::HeaderMap) {
        let response = self.send(request(method, uri, body)).await;
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let parsed = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, parsed, headers)
    }

    /// Registers an upstream pointing at the mock server.
    pub async fn seed_upstream(&self, alias: &str, server: &MockServer) -> serde_json::Value {
        let (status, body) = self
            .json(
                "POST",
                "/oagw/v1/upstreams",
                Some(serde_json::json!({
                    "alias": alias,
                    "protocol": oagw::gts_helpers::PROTOCOL_HTTP,
                    "server": {"endpoints": [
                        {"scheme": "http", "host": server.host(), "port": server.port()}
                    ]}
                })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "upstream seed: {body}");
        body
    }

    /// Registers a route on an upstream.
    pub async fn seed_route(
        &self,
        upstream_id: &str,
        path: &str,
        methods: &[&str],
    ) -> serde_json::Value {
        let (status, body) = self
            .json(
                "POST",
                &format!("/oagw/v1/upstreams/{upstream_id}/routes"),
                Some(serde_json::json!({
                    "match": {"http": {"path": path, "methods": methods}}
                })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "route seed: {body}");
        body
    }
}

impl Harness {
    /// Sends a raw request and returns status, parsed body and headers.
    pub async fn send_raw(
        &self,
        request: Request<Body>,
    ) -> (StatusCode, serde_json::Value, axum::http::HeaderMap) {
        let response = self.send(request).await;
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let parsed = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, parsed, headers)
    }
}

/// Builds a request with an optional JSON body.
#[must_use]
pub fn request(method: &str, uri: &str, body: Option<serde_json::Value>) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let content = match body {
        Some(value) => Body::from(serde_json::to_vec(&value).expect("serialisable body")),
        None => Body::empty(),
    };
    builder.body(content).expect("valid request")
}

/// Builds a request with explicit headers and an optional JSON body.
#[must_use]
pub fn raw_request(
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    body: Option<serde_json::Value>,
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let content = match body {
        Some(value) => Body::from(serde_json::to_vec(&value).expect("serialisable body")),
        None => Body::empty(),
    };
    builder.body(content).expect("valid request")
}

impl Harness {
    /// Sends a raw request and returns the parsed JSON body, whatever it is.
    pub async fn send_json(&self, request: Request<Body>) -> serde_json::Value {
        let response = self.send(request).await;
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    }
}
