//! Shared test harness for the OAGW gear.
//!
//! Builds a real `Router` (via `register_routes`) over the in-memory
//! store, with mock SDK clients for the gear's dependencies:
//!
//! * [`StubTypesRegistry`] — always-succeeding `register`, not-found
//!   reads (the SDK's `MockTypesRegistryClient` panics on `register`, so
//!   a hand-rolled stub is required).
//! * [`MockTenantResolver`] — configurable ancestor chain returned by
//!   `get_ancestors`, implementing the full `TenantResolverClient` trait
//!   (the SDK ships no test-util).
//! * `MockCredStoreClient` from `credstore-sdk` `test-util`.
//!
//! `SecurityContext`s are injected into request extensions (the gateway
//! auth middleware normally provides them).

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::doc_markdown,
    dead_code
)]

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, Response, StatusCode, header};
use http_body_util::BodyExt;
use tower::ServiceExt;
use uuid::Uuid;

use oagw::api::rest::routes::register_routes;
use oagw::config::{OagwConfig, SsrfPolicy};
use oagw::domain::services::ControlPlane;
use oagw::infra::plugins::PluginEngine;
use oagw::infra::proxy::DataPlane;
use oagw::infra::storage::MemoryStore;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationSpec;
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;

use credstore_sdk::CredStoreClientV1;
use tenant_resolver_sdk::{
    GetAncestorsOptions, GetAncestorsResponse, GetDescendantsOptions, GetDescendantsResponse,
    GetTenantsOptions, IsAncestorOptions, TenantId, TenantInfo, TenantRef, TenantResolverClient,
    TenantResolverError, TenantStatus,
};
use types_registry_sdk::{
    GtsInstance, GtsTypeSchema, InstanceQuery, RegisterResult, TypeSchemaQuery,
    TypesRegistryClient,
};

/// HTTP protocol identifier accepted by upstream validation.
pub const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

// =====================================================================
//                        Noop OpenAPI registry
// =====================================================================

/// Test OpenAPI registry — records nothing.
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

// =====================================================================
//                       Stub types-registry client
// =====================================================================

/// `TypesRegistryClient` test double: `register*` always reports
/// success, reads report `NotFound`. The SDK's `MockTypesRegistryClient`
/// panics on non-empty `register`, so this stub stands in.
#[derive(Default)]
pub struct StubTypesRegistry;

impl StubTypesRegistry {
    fn ok_results(entities: &[serde_json::Value]) -> Vec<RegisterResult> {
        entities
            .iter()
            .map(|entity| RegisterResult::Ok {
                gts_id: entity
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_owned(),
            })
            .collect()
    }
}

#[async_trait]
impl TypesRegistryClient for StubTypesRegistry {
    async fn register(
        &self,
        entities: Vec<serde_json::Value>,
    ) -> Result<Vec<RegisterResult>, CanonicalError> {
        Ok(Self::ok_results(&entities))
    }

    async fn register_type_schemas(
        &self,
        type_schemas: Vec<serde_json::Value>,
    ) -> Result<Vec<RegisterResult>, CanonicalError> {
        Ok(Self::ok_results(&type_schemas))
    }

    async fn get_type_schema(&self, type_id: &str) -> Result<GtsTypeSchema, CanonicalError> {
        Err(types_registry_sdk::testing::not_found(type_id))
    }

    async fn get_type_schema_by_uuid(&self, id: Uuid) -> Result<GtsTypeSchema, CanonicalError> {
        Err(types_registry_sdk::testing::not_found(id.to_string()))
    }

    async fn get_type_schemas(
        &self,
        type_ids: Vec<String>,
    ) -> HashMap<String, Result<GtsTypeSchema, CanonicalError>> {
        type_ids
            .into_iter()
            .map(|id| (id.clone(), Err(types_registry_sdk::testing::not_found(id))))
            .collect()
    }

    async fn get_type_schemas_by_uuid(
        &self,
        ids: Vec<Uuid>,
    ) -> HashMap<Uuid, Result<GtsTypeSchema, CanonicalError>> {
        ids.into_iter()
            .map(|id| (id, Err(types_registry_sdk::testing::not_found(id.to_string()))))
            .collect()
    }

    async fn list_type_schemas(
        &self,
        _query: TypeSchemaQuery,
    ) -> Result<Vec<GtsTypeSchema>, CanonicalError> {
        Ok(Vec::new())
    }

    async fn register_instances(
        &self,
        instances: Vec<serde_json::Value>,
    ) -> Result<Vec<RegisterResult>, CanonicalError> {
        Ok(Self::ok_results(&instances))
    }

    async fn get_instance(&self, id: &str) -> Result<GtsInstance, CanonicalError> {
        Err(types_registry_sdk::testing::not_found(id))
    }

    async fn get_instance_by_uuid(&self, id: Uuid) -> Result<GtsInstance, CanonicalError> {
        Err(types_registry_sdk::testing::not_found(id.to_string()))
    }

    async fn get_instances(
        &self,
        ids: Vec<String>,
    ) -> HashMap<String, Result<GtsInstance, CanonicalError>> {
        ids.into_iter()
            .map(|id| (id.clone(), Err(types_registry_sdk::testing::not_found(id))))
            .collect()
    }

    async fn get_instances_by_uuid(
        &self,
        ids: Vec<Uuid>,
    ) -> HashMap<Uuid, Result<GtsInstance, CanonicalError>> {
        ids.into_iter()
            .map(|id| (id, Err(types_registry_sdk::testing::not_found(id.to_string()))))
            .collect()
    }

    async fn list_instances(
        &self,
        _query: InstanceQuery,
    ) -> Result<Vec<GtsInstance>, CanonicalError> {
        Ok(Vec::new())
    }
}

// =====================================================================
//                       Mock tenant resolver
// =====================================================================

/// `TenantResolverClient` test double with a configurable ancestor chain.
///
/// `get_ancestors` returns the caller's tenant plus `ancestors` (direct
/// parent → root), letting proxy tests exercise descendant→root alias
/// resolution.
pub struct MockTenantResolver {
    ancestors: Vec<Uuid>,
}

impl MockTenantResolver {
    pub fn new(ancestors: Vec<Uuid>) -> Self {
        Self { ancestors }
    }

    fn ref_for(id: Uuid) -> TenantRef {
        TenantRef {
            id: TenantId(id),
            status: TenantStatus::Active,
            tenant_type: None,
            parent_id: None,
            self_managed: false,
        }
    }
}

#[async_trait]
impl TenantResolverClient for MockTenantResolver {
    async fn get_tenant(
        &self,
        _ctx: &SecurityContext,
        id: TenantId,
    ) -> Result<TenantInfo, TenantResolverError> {
        Ok(TenantInfo {
            id,
            name: "mock-tenant".to_owned(),
            status: TenantStatus::Active,
            tenant_type: None,
            parent_id: None,
            self_managed: false,
        })
    }

    async fn get_root_tenant(
        &self,
        _ctx: &SecurityContext,
    ) -> Result<TenantInfo, TenantResolverError> {
        Ok(TenantInfo {
            id: TenantId::nil(),
            name: "root".to_owned(),
            status: TenantStatus::Active,
            tenant_type: None,
            parent_id: None,
            self_managed: false,
        })
    }

    async fn get_tenants(
        &self,
        _ctx: &SecurityContext,
        ids: &[TenantId],
        _options: &GetTenantsOptions,
    ) -> Result<Vec<TenantInfo>, TenantResolverError> {
        Ok(ids
            .iter()
            .map(|id| TenantInfo {
                id: *id,
                name: "mock-tenant".to_owned(),
                status: TenantStatus::Active,
                tenant_type: None,
                parent_id: None,
                self_managed: false,
            })
            .collect())
    }

    async fn get_ancestors(
        &self,
        _ctx: &SecurityContext,
        id: TenantId,
        _options: &GetAncestorsOptions,
    ) -> Result<GetAncestorsResponse, TenantResolverError> {
        Ok(GetAncestorsResponse {
            tenant: Self::ref_for(id.0),
            ancestors: self.ancestors.iter().copied().map(Self::ref_for).collect(),
        })
    }

    async fn get_descendants(
        &self,
        _ctx: &SecurityContext,
        id: TenantId,
        _options: &GetDescendantsOptions,
    ) -> Result<GetDescendantsResponse, TenantResolverError> {
        Ok(GetDescendantsResponse {
            tenant: Self::ref_for(id.0),
            descendants: Vec::new(),
        })
    }

    async fn is_ancestor(
        &self,
        _ctx: &SecurityContext,
        _ancestor: TenantId,
        _descendant: TenantId,
        _options: &IsAncestorOptions,
    ) -> Result<bool, TenantResolverError> {
        Ok(false)
    }
}

// =====================================================================
//                            Test app
// =====================================================================

/// Fully wired OAGW router for `Router::oneshot` tests.
pub struct TestApp {
    pub router: Router,
    pub control_plane: Arc<ControlPlane>,
    pub store: Arc<MemoryStore>,
}

impl TestApp {
    /// A fresh app: no tenant ancestors, empty credential store.
    pub async fn new() -> Self {
        Self::with_credstore_and_chain(
            Arc::new(credstore_sdk::test_util::MockCredStoreClient::empty())
                as Arc<dyn CredStoreClientV1>,
            Arc::new(MockTenantResolver::new(vec![])),
        )
        .await
    }

    /// A fresh app with a configured credential store (for auth-plugin
    /// proxy tests).
    pub async fn with_credstore(credstore: Arc<dyn CredStoreClientV1>) -> Self {
        Self::with_credstore_and_chain(credstore, Arc::new(MockTenantResolver::new(vec![]))).await
    }

    /// A fresh app whose subject tenant sits under `ancestors` (direct
    /// parent → root) for hierarchy resolution tests.
    pub async fn with_tenant_chain(ancestors: Vec<Uuid>) -> Self {
        Self::with_credstore_and_chain(
            Arc::new(credstore_sdk::test_util::MockCredStoreClient::empty())
                as Arc<dyn CredStoreClientV1>,
            Arc::new(MockTenantResolver::new(ancestors)),
        )
        .await
    }

    async fn with_credstore_and_chain(
        credstore: Arc<dyn CredStoreClientV1>,
        resolver: Arc<MockTenantResolver>,
    ) -> Self {
        let store = Arc::new(MemoryStore::new());
        let config = Arc::new(test_config());
        let types_registry: Arc<dyn TypesRegistryClient> = Arc::new(StubTypesRegistry);
        let plugins = Arc::new(PluginEngine::new(credstore, &config));
        let control_plane =
            Arc::new(ControlPlane::new(store.clone(), config.clone(), types_registry));
        let tenants: Arc<dyn TenantResolverClient> = resolver;
        let data_plane = Arc::new(
            DataPlane::new(store.clone(), tenants, plugins, config)
                .expect("data plane builds with test config"),
        );
        let openapi = NoopOpenApiRegistry;
        let router = register_routes(
            Router::new(),
            &openapi,
            control_plane.clone(),
            data_plane.clone(),
        );
        Self {
            router,
            control_plane,
            store,
        }
    }
}

/// Test config: mirrors the graded e2e config (`ssrf_policy.enabled:
/// false`) and permits plain-HTTP upstreams so local mock servers are
/// routable.
pub fn test_config() -> OagwConfig {
    OagwConfig {
        allow_http_upstream: true,
        ssrf_policy: SsrfPolicy {
            enabled: false,
            ..SsrfPolicy::default()
        },
        ..OagwConfig::default()
    }
}

// =====================================================================
//                        Request / response helpers
// =====================================================================

/// Build a `SecurityContext` with the given tenant and token scopes.
pub fn make_ctx(tenant_id: Uuid, scopes: &[&str]) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(tenant_id)
        .token_scopes(scopes.iter().map(|s| (*s).to_owned()).collect())
        .build()
        .expect("valid SecurityContext")
}

/// Unrestricted security context (wildcard scope).
pub fn admin_ctx(tenant_id: Uuid) -> SecurityContext {
    make_ctx(tenant_id, &["*"])
}

/// Send a request through the router with a `SecurityContext` injected
/// into the extensions (as the gateway auth middleware would).
#[allow(clippy::too_many_arguments)]
pub async fn send(
    router: &Router,
    method: &str,
    uri: &str,
    ctx: &SecurityContext,
    body: Option<serde_json::Value>,
    headers: &[(&str, &str)],
) -> Response<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
    }
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let bytes = body
        .map(|b| serde_json::to_vec(&b).expect("request body serializes"))
        .unwrap_or_default();
    let mut req = builder
        .body(Body::from(bytes))
        .expect("valid request body");
    req.extensions_mut().insert(ctx.clone());
    router
        .clone()
        .oneshot(req)
        .await
        .expect("router handles request")
}

/// Read the response body as JSON (empty body → `Value::Null`).
pub async fn response_json(resp: Response<Body>) -> serde_json::Value {
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("response body reads")
        .to_bytes();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

/// Read the response body as UTF-8 text.
pub async fn response_text(resp: Response<Body>) -> String {
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("response body reads")
        .to_bytes();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Assert the response is a gateway problem document with the given
/// status, returning the parsed body.
pub async fn expect_problem(resp: Response<Body>, status: StatusCode) -> serde_json::Value {
    assert_eq!(resp.status(), status, "status for {:?}", resp);
    assert_eq!(
        resp.headers().get("content-type").and_then(|v| v.to_str().ok()),
        Some("application/problem+json; charset=utf-8")
    );
    assert_eq!(
        resp.headers()
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some("gateway")
    );
    response_json(resp).await
}

/// Create an upstream through the management API for `tenant`, returning
/// the created record. Panics on non-201.
pub async fn create_upstream(
    app: &TestApp,
    tenant: Uuid,
    body: serde_json::Value,
) -> serde_json::Value {
    let ctx = admin_ctx(tenant);
    let resp = send(&app.router, "POST", "/oagw/v1/upstreams", &ctx, Some(body), &[]).await;
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "create upstream: {}",
        response_text(resp).await
    );
    response_json(resp).await
}

/// Create a route through the management API for `tenant`, returning the
/// created record. Panics on non-201.
pub async fn create_route(app: &TestApp, tenant: Uuid, body: serde_json::Value) -> serde_json::Value {
    let ctx = admin_ctx(tenant);
    let resp = send(&app.router, "POST", "/oagw/v1/routes", &ctx, Some(body), &[]).await;
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "create route: {}",
        response_text(resp).await
    );
    response_json(resp).await
}

/// Seed an upstream + catch-all route for a proxy tests.
///
/// `endpoints` is the raw `server.endpoints` array; the upstream alias is
/// derived (or given explicitly for IP endpoints). Returns the upstream id.
pub async fn seed_upstream_and_route(
    app: &TestApp,
    tenant: Uuid,
    alias: Option<&str>,
    endpoints: serde_json::Value,
    extra: serde_json::Value,
) -> Uuid {
    let mut upstream = serde_json::json!({
        "enabled": true,
        "server": { "endpoints": endpoints },
        "protocol": PROTOCOL_HTTP,
        "tags": [],
    });
    if let Some(alias) = alias {
        upstream["alias"] = serde_json::Value::String(alias.to_owned());
    }
    if let Some(obj) = extra.as_object() {
        upstream
            .as_object_mut()
            .unwrap()
            .extend(obj.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    let upstream = create_upstream(app, tenant, upstream).await;
    let upstream_id = upstream["id"].as_str().expect("upstream id").to_owned();

    let route = serde_json::json!({
        "enabled": true,
        "upstream_id": upstream_id,
        "tags": [],
        "match": {
            "http": {
                "methods": ["GET", "POST", "PUT", "PATCH", "DELETE"],
                "path": "/",
                "path_suffix_mode": "append"
            }
        }
    });
    create_route(app, tenant, route).await;
    Uuid::parse_str(&upstream_id).expect("valid upstream uuid")
}
