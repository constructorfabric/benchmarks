//! Shared test scaffolding for the OAGW integration tests.
//!
//! Builds a full axum router via `routes::register_routes` (control plane +
//! data plane) with in-memory repositories, a permissive authz mock, and the
//! built-in plugin registries — the same wiring the gear performs at init.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use authz_resolver_sdk::{
    AuthZResolverClient, AuthZResolverError, EvaluationRequest, EvaluationResponse,
    EvaluationResponseContext, PolicyEnforcer,
};
use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt;
use oagw::api::rest::routes::register_routes;
use oagw::config::{OagwConfig, TokenCacheConfig};
use oagw::domain::error::DomainError;
use oagw::domain::plugin::SecretResolver;
use oagw::domain::services::management::ControlPlaneServiceImpl;
use oagw::infra::plugin::registry::{
    AuthPluginRegistry, GuardPluginRegistry, TransformPluginRegistry,
};
use oagw::infra::proxy::client::OagwHttpClient;
use oagw::infra::proxy::ratelimit::RateLimitManager;
use oagw::infra::proxy::service::DataPlaneServiceImpl;
use oagw::infra::storage::memory::{
    MemoryPluginRepository, MemoryRouteRepository, MemoryUpstreamRepository,
};
use oagw::infra::storage::tenant_hierarchy::MemoryHierarchy;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationSpec;
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

// ── Noop OpenAPI registry (route registration only needs the trait) ────────

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

// ── Permissive authz mock ──────────────────────────────────────────────────

pub struct AllowAllAuthZ;

#[async_trait]
impl AuthZResolverClient for AllowAllAuthZ {
    async fn evaluate(
        &self,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, AuthZResolverError> {
        // All OAGW calls use `require_constraints(false)`, so an empty context
        // compiles to `allow_all()`.
        Ok(EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext::default(),
        })
    }
}

// ── Security context ────────────────────────────────────────────────────────

pub fn make_ctx(tenant: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(tenant)
        .build()
        .expect("valid test SecurityContext")
}

// ── Static secret resolver ─────────────────────────────────────────────────

pub struct StaticSecretResolver(pub HashMap<String, String>);

#[async_trait]
impl SecretResolver for StaticSecretResolver {
    async fn resolve(
        &self,
        _tenant_id: Uuid,
        secret_ref: &str,
    ) -> Result<Vec<u8>, DomainError> {
        self.0
            .get(secret_ref)
            .map(|v| v.clone().into_bytes())
            .ok_or_else(|| DomainError::SecretNotFound(secret_ref.to_owned()))
    }
}

// ── Test environment ---------------------------------------------------------

pub struct TestEnv {
    pub router: Router,
    pub tenant: Uuid,
    pub upstreams: Arc<MemoryUpstreamRepository>,
    pub routes: Arc<MemoryRouteRepository>,
    pub hierarchy: Arc<MemoryHierarchy>,
    pub http: Arc<OagwHttpClient>,
}

/// Build a fully-wired router (control plane + data plane) sharing one set of
/// in-memory repositories and a single rate-limit manager.
pub fn build_env(config: OagwConfig) -> TestEnv {
    build_env_with_hierarchy(config, Uuid::new_v4(), Arc::new(MemoryHierarchy::default()))
}

/// Like [`build_env`] but with a caller-chosen tenant and hierarchy (for
/// tests that exercise cross-tenant shadowing and layering).
pub fn build_env_with_hierarchy(
    config: OagwConfig,
    tenant: Uuid,
    hierarchy: Arc<MemoryHierarchy>,
) -> TestEnv {
    let upstreams = Arc::new(MemoryUpstreamRepository::default());
    let routes = Arc::new(MemoryRouteRepository::default());
    let plugins = Arc::new(MemoryPluginRepository::default());
    let authz = PolicyEnforcer::new(Arc::new(AllowAllAuthZ));

    let control = Arc::new(ControlPlaneServiceImpl::new(
        upstreams.clone(),
        routes.clone(),
        plugins.clone(),
        hierarchy.clone(),
        authz.clone(),
    ));

    let http = Arc::new(OagwHttpClient::new());
    let secrets = Arc::new(StaticSecretResolver(HashMap::new()));
    let auth_registry = AuthPluginRegistry::with_builtins_resolver(
        secrets.clone(),
        http.clone(),
        TokenCacheConfig::default(),
    );
    let guard_registry = GuardPluginRegistry::with_builtins();
    let transform_registry = TransformPluginRegistry::with_builtins();
    let rate_limiter = Arc::new(RateLimitManager::new());

    let data = Arc::new(DataPlaneServiceImpl::new(
        upstreams.clone(),
        routes.clone(),
        plugins.clone(),
        hierarchy.clone(),
        authz,
        auth_registry,
        guard_registry,
        transform_registry,
        secrets,
        rate_limiter,
        http.clone(),
        config,
    ));

    let openapi = NoopOpenApiRegistry;
    let router = register_routes(Router::new(), &openapi, control, data);
    TestEnv {
        router,
        tenant,
        upstreams,
        routes,
        hierarchy,
        http,
    }
}

// ── Request / response helpers ─────────────────────────────────────────────

/// Build a JSON management request with the caller's `SecurityContext`
/// injected as a request extension (the pattern used by peer gears).
pub fn json_request(
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
    tenant: Uuid,
) -> Request<Body> {
    let ctx = make_ctx(tenant);
    let mut builder = Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let body = match body {
        Some(json) => Body::from(serde_json::to_vec(&json).expect("serialize body")),
        None => Body::empty(),
    };
    let mut req = builder.body(body).expect("request build");
    req.extensions_mut().insert(ctx);
    req
}

/// Build a proxy request (no JSON body, ctx injected).
pub fn proxy_request(method: &str, uri: &str, tenant: Uuid) -> Request<Body> {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .expect("proxy request build");
    req.extensions_mut().insert(make_ctx(tenant));
    req
}

pub async fn body_bytes(resp: axum::http::Response<Body>) -> Vec<u8> {
    resp.into_body().collect().await.expect("collect body").to_bytes().to_vec()
}

/// Fire a management-style request and parse the JSON body.
pub async fn api_json(
    env: &TestEnv,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
    tenant: Uuid,
) -> (StatusCode, serde_json::Value) {
    let resp = env
        .router
        .clone()
        .oneshot(json_request(method, uri, body, tenant))
        .await
        .expect("oneshot");
    let status = resp.status();
    let bytes = body_bytes(resp).await;
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

/// Fire an arbitrary request and return status + headers + raw body.
pub async fn raw(
    env: &TestEnv,
    req: Request<Body>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let resp = env.router.clone().oneshot(req).await.expect("oneshot");
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = body_bytes(resp).await;
    (status, headers, bytes)
}

// ── Upstream/route creation helpers (returns the created resource ids) ─────

pub async fn create_ip_upstream(
    env: &TestEnv,
    tenant: Uuid,
    alias: &str,
    port: u16,
) -> Uuid {
    let (status, json) = api_json(
        env,
        "POST",
        "/oagw/v1/upstreams",
        Some(serde_json::json!({
            "enabled": true,
            "alias": alias,
            "server": { "endpoints": [{ "scheme": "http", "host": "127.0.0.1", "port": port }] },
            "protocol": "http",
        })),
        tenant,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create upstream: {json}");
    assert_eq!(json["alias"], alias, "alias echoes back: {json}");
    Uuid::parse_str(json["id"].as_str().expect("upstream id")).expect("parse uuid")
}

pub async fn create_http_route(
    env: &TestEnv,
    tenant: Uuid,
    upstream_id: Uuid,
    path: &str,
    methods: &[&str],
) -> Uuid {
    let (status, json) = api_json(
        env,
        "POST",
        "/oagw/v1/routes",
        Some(serde_json::json!({
            "upstream_id": upstream_id.to_string(),
            "enabled": true,
            "priority": 0,
            "match": { "http": { "path": path, "path_suffix_mode": "append", "methods": methods } }
        })),
        tenant,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create route: {json}");
    Uuid::parse_str(json["id"].as_str().expect("route id")).expect("parse uuid")
}
