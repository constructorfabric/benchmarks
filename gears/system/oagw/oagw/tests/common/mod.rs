#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

//! Shared helpers for the OAGW integration tests.
//!
//! The management REST surface is exercised through a plain `axum::Router`
//! built from the OAGW route registration (the toolkit `OperationBuilder`
//! records `OpenAPI` metadata but attaches no runtime bearer-check middleware,
//! so handlers run with a [`SecurityContext`] injected via request
//! extensions — the same double the sibling gears use).
//!
//! The proxy path is exercised end to end against a real plain-HTTP axum
//! upstream on `127.0.0.1` with `allow_http_upstream: true` and the SSRF
//! policy disabled, mirroring the frozen e2e configuration.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::Request;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationSpec;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use tenant_resolver_sdk::{
    GetAncestorsOptions, GetAncestorsResponse, GetDescendantsOptions, GetDescendantsResponse,
    GetTenantsOptions, IsAncestorOptions, TenantId, TenantInfo, TenantRef, TenantResolverClient,
    TenantResolverError, TenantStatus,
};

use oagw::config::OagwConfig;
use oagw::domain::control::ControlPlaneService;
use oagw::infra::data_plane::DataPlaneService;
use oagw::infra::memory_repo::MemoryRepository;

/// No-op `OpenAPI` registry: the tests assert route behaviour, not the
/// generated `OpenAPI` document.
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

/// Fixed tenant ids mirrored from the e2e payload (see the run contract).
pub const TENANT_ROOT: Uuid = Uuid::from_u128(0x0000_0000_df51_5b42_9538_d2b5_6b7e_e953);
pub const TENANT_A: Uuid = Uuid::from_u128(0x0000_0000_df51_5b42_9538_d2b5_6b7e_e952);
pub const TENANT_B: Uuid = Uuid::from_u128(0x0000_0000_df51_5b42_9538_d2b5_6b7e_e951);

/// Data-plane configuration matching the frozen e2e payload:
/// `proxy_timeout_secs: 2`, `allow_http_upstream: true`,
/// `ssrf_policy: { enabled: false }`.
pub fn e2e_config() -> OagwConfig {
    OagwConfig {
        proxy_timeout_secs: 2,
        allow_http_upstream: true,
        ssrf_policy: oagw::config::SsrfConfig {
            enabled: false,
            block_loopback: true,
            block_private: true,
        },
        ..OagwConfig::default()
    }
}

/// A `SecurityContext` for `tenant_id` (subject == a member of the tenant).
pub fn make_security(tenant_id: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(tenant_id)
        .build()
        .expect("valid security context")
}

/// In-memory repository + control plane with the default tenant topology
/// (root with two children A and B).
pub fn control_with_topology() -> Arc<ControlPlaneService> {
    let repo = Arc::new(MemoryRepository::new());
    let tenants: Arc<dyn TenantResolverClient> = Arc::new(MockTenantResolver::new(&[
        (TENANT_ROOT, None),
        (TENANT_A, Some(TENANT_ROOT)),
        (TENANT_B, Some(TENANT_ROOT)),
    ]));
    Arc::new(ControlPlaneService::new(repo, tenants))
}

/// Build the full OAGW REST router over the given services.
pub fn build_router(
    control: Arc<ControlPlaneService>,
    data_plane: Arc<DataPlaneService>,
) -> Router {
    oagw::api::rest::routes::register_routes(
        Router::new(),
        &NoopOpenApiRegistry,
        control,
        data_plane,
    )
}

/// A JSON management request with the caller's [`SecurityContext`] injected
/// into the request extensions (the axum `Extension` extractor reads it).
pub fn json_request(
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
    tenant_id: Uuid,
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let body = match body {
        Some(json) => Body::from(serde_json::to_vec(&json).expect("serialize request body")),
        None => Body::empty(),
    };
    let mut req = builder.body(body).expect("valid request");
    req.extensions_mut().insert(make_security(tenant_id));
    req
}

/// A raw (non-JSON) request, for the proxy route and preflight checks.
pub fn raw_request(method: &str, uri: &str, tenant_id: Uuid) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
    let mut req = builder.body(Body::empty()).expect("valid request");
    req.extensions_mut().insert(make_security(tenant_id));
    req
}

/// A raw request with caller-supplied extra headers (proxy tests).
pub fn request_with_headers(
    method: &str,
    uri: &str,
    tenant_id: Uuid,
    headers: &[(&str, &str)],
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let mut req = builder.body(Body::empty()).expect("valid request");
    req.extensions_mut().insert(make_security(tenant_id));
    req
}

/// An anonymous request (no `SecurityContext`) for preflight.
pub fn anonymous_request(method: &str, uri: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .expect("valid request")
}

/// Collect a response body and parse it as JSON.
pub async fn response_json(resp: axum::response::Response<Body>) -> serde_json::Value {
    let bytes = http_body_util::BodyExt::collect(resp.into_body())
        .await
        .expect("collect body")
        .to_bytes();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

/// A configurable tenant-resolver double: `get_ancestors` walks the parent
/// map (the only hierarchy call OAGW makes), the rest answer minimally.
pub struct MockTenantResolver {
    parents: HashMap<Uuid, Option<Uuid>>,
}

impl MockTenantResolver {
    /// Build from `(tenant_id, parent_id)` pairs; a root has `None`.
    pub fn new(pairs: &[(Uuid, Option<Uuid>)]) -> Self {
        Self {
            parents: pairs.iter().copied().collect(),
        }
    }

    fn ref_for(&self, id: Uuid) -> Option<TenantRef> {
        self.parents.get(&id).map(|parent| TenantRef {
            id: TenantId(id),
            status: TenantStatus::Active,
            tenant_type: None,
            parent_id: parent.map(TenantId),
            self_managed: false,
        })
    }

    fn info_for(&self, id: Uuid) -> Option<TenantInfo> {
        self.ref_for(id).map(|r| TenantInfo {
            id: r.id,
            name: format!("tenant-{id}"),
            status: r.status,
            tenant_type: r.tenant_type,
            parent_id: r.parent_id,
            self_managed: r.self_managed,
        })
    }
}

#[async_trait]
impl TenantResolverClient for MockTenantResolver {
    async fn get_tenant(
        &self,
        _ctx: &SecurityContext,
        id: TenantId,
    ) -> Result<TenantInfo, TenantResolverError> {
        self.info_for(id.0)
            .ok_or(TenantResolverError::TenantNotFound { tenant_id: id })
    }

    async fn get_root_tenant(
        &self,
        _ctx: &SecurityContext,
    ) -> Result<TenantInfo, TenantResolverError> {
        let (id, _) = self
            .parents
            .iter()
            .find(|(_, parent)| parent.is_none())
            .ok_or_else(|| TenantResolverError::Internal("no root tenant".to_owned()))?;
        self.info_for(*id)
            .ok_or(TenantResolverError::Internal("root missing".to_owned()))
    }

    async fn get_tenants(
        &self,
        _ctx: &SecurityContext,
        ids: &[TenantId],
        _options: &GetTenantsOptions,
    ) -> Result<Vec<TenantInfo>, TenantResolverError> {
        Ok(ids.iter().filter_map(|id| self.info_for(id.0)).collect())
    }

    async fn get_ancestors(
        &self,
        _ctx: &SecurityContext,
        id: TenantId,
        _options: &GetAncestorsOptions,
    ) -> Result<GetAncestorsResponse, TenantResolverError> {
        let start = self
            .ref_for(id.0)
            .ok_or(TenantResolverError::TenantNotFound { tenant_id: id })?;
        let mut ancestors = Vec::new();
        let mut current = start.parent_id;
        while let Some(parent) = current {
            let parent_ref = self
                .ref_for(parent.0)
                .ok_or(TenantResolverError::TenantNotFound { tenant_id: parent })?;
            current = parent_ref.parent_id;
            ancestors.push(parent_ref);
        }
        Ok(GetAncestorsResponse {
            tenant: start,
            ancestors,
        })
    }

    async fn get_descendants(
        &self,
        _ctx: &SecurityContext,
        id: TenantId,
        _options: &GetDescendantsOptions,
    ) -> Result<GetDescendantsResponse, TenantResolverError> {
        let start = self
            .ref_for(id.0)
            .ok_or(TenantResolverError::TenantNotFound { tenant_id: id })?;
        let mut descendants = Vec::new();
        for (candidate, parent) in &self.parents {
            if *parent == Some(id.0) {
                descendants.push(TenantRef {
                    id: TenantId(*candidate),
                    status: TenantStatus::Active,
                    tenant_type: None,
                    parent_id: Some(id),
                    self_managed: false,
                });
            }
        }
        Ok(GetDescendantsResponse {
            tenant: start,
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
        let mut current = self.parents.get(&descendant_id.0).copied().ok_or(
            TenantResolverError::TenantNotFound {
                tenant_id: descendant_id,
            },
        )?;
        while let Some(parent) = current {
            if parent == ancestor_id.0 {
                return Ok(true);
            }
            current = *self
                .parents
                .get(&parent)
                .ok_or(TenantResolverError::TenantNotFound {
                    tenant_id: TenantId(parent),
                })?;
        }
        Ok(false)
    }
}

/// Start a plain-HTTP axum server bound to `127.0.0.1:0` and return its port
/// and a shutdown handle. The handler echoes a JSON envelope with the
/// request method, path, query, headers and body — enough to assert the
/// whole proxy pipeline.
pub async fn start_upstream(handler: axum::Router) -> (u16, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let addr = listener.local_addr().expect("local addr");
    let handle = tokio::spawn(async move {
        axum::serve(listener, handler)
            .await
            .expect("upstream serves");
    });
    (addr.port(), handle)
}

/// Build the echo upstream router used across the data-plane tests.
pub fn echo_upstream() -> axum::Router {
    use axum::routing::any;
    axum::Router::new().route("/{*path}", any(echo_handler))
}

async fn echo_handler(
    method: axum::http::Method,
    uri: axum::http::Uri,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> axum::response::Response<Body> {
    let header_map: serde_json::Map<String, serde_json::Value> = headers
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                serde_json::Value::String(value.to_str().unwrap_or_default().to_owned()),
            )
        })
        .collect();
    let envelope = serde_json::json!({
        "method": method.as_str(),
        "path": uri.path(),
        "query": uri.query(),
        "headers": header_map,
        "body": String::from_utf8_lossy(&body),
    });
    axum::response::Response::builder()
        .status(axum::http::StatusCode::OK)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&envelope).expect("envelope")))
        .expect("build response")
}
