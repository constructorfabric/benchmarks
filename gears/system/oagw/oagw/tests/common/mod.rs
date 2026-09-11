//! Shared harness for the `oagw` integration suite.
//!
//! Builds the gear's router the way `RestApiCapability::register_rest` does —
//! the management routes through the OpenAPI registry, the proxy routes
//! beside them — and inserts the caller's `SecurityContext` into every
//! request, which is what the api-gateway layer does in production.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use tower::ServiceExt;
use uuid::Uuid;

use oagw::OagwConfig;
use tenant_resolver_sdk::{
    GetAncestorsOptions, GetAncestorsResponse, GetDescendantsOptions, GetDescendantsResponse,
    GetTenantsOptions, IsAncestorOptions, TenantId, TenantInfo, TenantRef, TenantResolverClient,
    TenantResolverError, TenantStatus,
};
use oagw::api::rest::routes;
use oagw::api::rest::state::OagwState;
use oagw::domain::services::control_plane::ControlPlaneService;
use oagw::infra::plugin::PluginRegistry;
use oagw::infra::proxy::service::OagwDataPlane;
use oagw::infra::storage::Storage;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationSpec;
use toolkit_security::SecurityContext;

/// An OpenAPI registry that collects nothing.
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

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// The gear under test: the router plus the state the handlers resolve.
pub struct Harness {
    pub router: Router,
    pub state: Arc<OagwState>,
    pub tenant: Uuid,
}

impl Harness {
    /// A router serving the whole `oagw` API with no tenant resolver and no
    /// authz resolver: the single-tenant posture.
    pub fn new(config: OagwConfig) -> Self {
        Self::with_plugins(config, None, None)
    }

    /// A gear whose plugin registry is the caller's, so a test can install
    /// plugins that record how they were run.
    pub fn with_plugins(
        config: OagwConfig,
        resolver: Option<Arc<dyn TenantResolverClient>>,
        plugins: Option<Arc<PluginRegistry>>,
    ) -> Self {
        let plugins = plugins.unwrap_or_else(|| Arc::new(PluginRegistry::builtin()));
        Self::assemble(config, resolver, plugins)
    }

    /// A router with an explicit tenant resolver, so hierarchy semantics are
    /// exercised; `None` gives the single-tenant posture.
    pub fn with_resolver(
        config: OagwConfig,
        resolver: Option<Arc<dyn TenantResolverClient>>,
    ) -> Self {
        Self::with_plugins(config, resolver, None)
    }

    /// Builds the gear around an already-assembled plugin registry.
    fn assemble(
        config: OagwConfig,
        resolver: Option<Arc<dyn TenantResolverClient>>,
        plugins: Arc<PluginRegistry>,
    ) -> Self {
        let tenant = Uuid::from_u128(1000);
        let storage = Storage::new();
        let control_plane = Arc::new(ControlPlaneService::new(
            storage.upstreams.clone(),
            storage.routes.clone(),
            storage.plugins.clone(),
            None,
            Some(plugins.clone()),
        ));
        let config = Arc::new(config);
        let data_plane = Arc::new(OagwDataPlane::new(
            control_plane.clone(),
            storage.rate_limits.clone(),
            plugins,
            config.clone(),
        ));
        let state = Arc::new(OagwState {
            control_plane,
            data_plane,
            config,
            types_registry: None,
            tenant_resolver: resolver,
        });
        let router = routes::router(Router::new(), &NoopOpenApi).layer(axum::Extension(state.clone()));
        Self { router, state, tenant }
    }

    /// The default configuration: HTTPS-only, SSRF checks on.
    pub fn default_gear() -> Self {
        Self::new(OagwConfig::default())
    }

    /// A gear with a tenant resolver, so hierarchy semantics are exercised.
    pub fn hierarchical(config: OagwConfig, resolver: Arc<dyn TenantResolverClient>) -> Self {
        Self::with_resolver(config, Some(resolver))
    }

    /// A gear whose configuration allows plaintext upstream dials.
    pub fn plaintext_gear() -> Self {
        Self::new(Self::plaintext_config())
    }

    /// The configuration that allows plaintext upstream dials.
    pub fn plaintext_config() -> OagwConfig {
        let mut config = OagwConfig::default();
        config.allow_http_upstream = true;
        config.proxy_timeout_secs = 5;
        config
    }

    /// A `SecurityContext` for the harness tenant.
    pub fn context(&self) -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::from_u128(9000))
            .subject_type("user")
            .subject_tenant_id(self.tenant)
            .build()
            .expect("valid security context")
    }

    /// Sends a request carrying the harness tenant's security context.
    pub async fn send(&self, request: Request<Body>) -> axum::http::Response<Body> {
        self.router.clone().oneshot(request).await.expect("in-memory request")
    }

    /// Builds a request with the caller's security context attached.
    pub fn request(&self, method: &str, uri: &str, body: Option<serde_json::Value>) -> Request<Body> {
        self.request_for(method, uri, body, self.tenant)
    }

    /// Builds a request for another tenant, for cross-tenant assertions.
    pub fn request_for(
        &self,
        method: &str,
        uri: &str,
        body: Option<serde_json::Value>,
        tenant: Uuid,
    ) -> Request<Body> {
        let mut builder = Request::builder().method(method).uri(uri);
        if body.is_some() {
            builder = builder.header("content-type", "application/json");
        }
        let body = match body {
            Some(value) => Body::from(serde_json::to_vec(&value).expect("serialisable body")),
            None => Body::empty(),
        };
        let context = SecurityContext::builder()
            .subject_id(Uuid::from_u128(9000))
            .subject_type("user")
            .subject_tenant_id(tenant)
            .build()
            .expect("valid security context");
        let mut request = builder.body(body).expect("buildable request");
        request.extensions_mut().insert(context);
        request
    }

    /// A proxied request with arbitrary headers.
    pub fn proxy_request(
        &self,
        method: &str,
        uri: &str,
        headers: &[(&str, &str)],
        body: Option<Vec<u8>>,
    ) -> Request<Body> {
        let mut builder = Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let body = body.map(Body::from).unwrap_or_else(Body::empty);
        let mut request = builder.body(body).expect("buildable request");
        request.extensions_mut().insert(self.context());
        request
    }

    /// A proxied request carrying **no** security context, the way a browser
    /// preflight arrives: WHATWG Fetch sends no credentials on a preflight, so
    /// no tenant is resolvable for it (ADR 0004 §"Preflight Request Handling").
    pub fn anonymous_proxy_request(
        &self,
        method: &str,
        uri: &str,
        headers: &[(&str, &str)],
    ) -> Request<Body> {
        let mut builder = Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(Body::empty()).expect("buildable request")
    }

    /// Serves the router on a real socket, the way the host does.
    ///
    /// Streaming responses and protocol upgrades only behave behind a real
    /// hyper connection, so tests that care about the wire bind one and talk
    /// raw HTTP to it. The caller's tenant context is injected the way the
    /// api-gateway layer injects it in production.
    pub async fn served_on_socket(&self) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a bindable loopback port");
        let addr = listener.local_addr().expect("an address");
        let context = self.context();
        let app = self.router.clone().layer(axum::middleware::from_fn(
            move |mut request: Request<Body>, next: axum::middleware::Next| {
                let context = context.clone();
                async move {
                    request.extensions_mut().insert(context.clone());
                    next.run(request).await
                }
            },
        ));
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("the test server serves until it is dropped");
        });
        addr
    }
}

/// A tenant resolver with a fixed parent map, so a test can stand up a
/// hierarchy without a live tenant-resolver gear.
pub struct StubResolver {
    /// Child tenant → its direct parent.
    pub parents: HashMap<Uuid, Uuid>,
}

impl StubResolver {
    /// A resolver whose hierarchy is `child → parent → root`.
    pub fn lineage(child: Uuid, parent: Uuid, root: Uuid) -> Self {
        Self {
            parents: HashMap::from([(child, parent), (parent, root)]),
        }
    }

    /// The ordered chain of `tenant`, direct parent first, as the SDK answers.
    fn chain(&self, tenant: Uuid) -> Vec<TenantRef> {
        let mut chain = Vec::new();
        let mut cursor = Some(tenant);
        while let Some(id) = cursor.and_then(|t| self.parents.get(&t).copied()) {
            chain.push(Self::reference(id, self.parents.get(&id).copied()));
            cursor = Some(id);
        }
        chain
    }

    fn reference(id: Uuid, parent: Option<Uuid>) -> TenantRef {
        TenantRef {
            id: TenantId(id),
            status: TenantStatus::Active,
            tenant_type: None,
            parent_id: parent.map(TenantId),
            self_managed: false,
        }
    }
}

#[async_trait]
impl TenantResolverClient for StubResolver {
    async fn get_tenant(
        &self,
        _ctx: &SecurityContext,
        id: TenantId,
    ) -> Result<TenantInfo, TenantResolverError> {
        Ok(TenantInfo {
            id,
            name: "stub".to_string(),
            status: TenantStatus::Active,
            tenant_type: None,
            parent_id: self.parents.get(&id.0).map(|p| TenantId(*p)),
            self_managed: false,
        })
    }

    async fn get_root_tenant(
        &self,
        _ctx: &SecurityContext,
    ) -> Result<TenantInfo, TenantResolverError> {
        let root = self
            .parents
            .values()
            .copied()
            .find(|p| !self.parents.contains_key(p))
            .unwrap_or_else(Uuid::nil);
        Ok(TenantInfo {
            id: TenantId(root),
            name: "root".to_string(),
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
                name: "stub".to_string(),
                status: TenantStatus::Active,
                tenant_type: None,
                parent_id: self.parents.get(&id.0).map(|p| TenantId(*p)),
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
        let ancestors = self.chain(id.0);
        Ok(GetAncestorsResponse {
            tenant: Self::reference(id.0, self.parents.get(&id.0).copied()),
            ancestors,
        })
    }

    async fn get_descendants(
        &self,
        _ctx: &SecurityContext,
        id: TenantId,
        _options: &GetDescendantsOptions,
    ) -> Result<GetDescendantsResponse, TenantResolverError> {
        let descendants: Vec<TenantRef> = self
            .parents
            .iter()
            .filter(|(_, parent)| **parent == id.0)
            .map(|(child, parent)| Self::reference(*child, Some(*parent)))
            .collect();
        Ok(GetDescendantsResponse {
            tenant: Self::reference(id.0, self.parents.get(&id.0).copied()),
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
        let mut cursor = Some(descendant_id.0);
        while let Some(id) = cursor.and_then(|t| self.parents.get(&t).copied()) {
            if id == ancestor_id.0 {
                return Ok(true);
            }
            cursor = Some(id);
        }
        Ok(false)
    }
}

/// Creates an upstream that dials a stub at `host:port`.
pub fn upstream_body(alias: &str, host: &str, port: u16, scheme: &str) -> serde_json::Value {
    // IP endpoints take an explicit alias; a hostname derives its own, so the
    // field is only sent when the caller names one.
    let mut body = serde_json::json!({
        "enabled": true,
        "server": {
            "endpoints": [ { "scheme": scheme, "host": host, "port": port } ]
        },
        "protocol": "http"
    });
    if !alias.is_empty() {
        body["alias"] = serde_json::Value::String(alias.to_string());
    }
    body
}

/// Creates a route bound to an upstream.
pub fn route_body(upstream_id: &str, path: &str, methods: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "upstream_id": upstream_id,
        "match": {
            "http": {
                "methods": methods,
                "path": path,
                "path_suffix_mode": "append"
            }
        }
    })
}

/// Creates an upstream through the management API, returning its id.
pub async fn create_upstream(harness: &Harness, alias: &str, host: &str, port: u16, scheme: &str) -> String {
    let response = harness
        .send(harness.request(
            "POST",
            "/oagw/v1/upstreams",
            Some(upstream_body(alias, host, port, scheme)),
        ))
        .await;
    assert_eq!(response.status(), 201, "upstream `{alias}` must be created");
    if let Some(location) = response.headers().get("location").and_then(|v| v.to_str().ok()) {
        return location.rsplit('/').next().unwrap_or_default().to_string();
    }
    let body = read_json(response).await;
    body["id"].as_str().unwrap_or_default().to_string()
}

/// Creates a route through the management API, returning its id.
pub async fn create_route(harness: &Harness, upstream_id: &str, path: &str, methods: &[&str]) -> String {
    let response = harness
        .send(harness.request(
            "POST",
            "/oagw/v1/routes",
            Some(route_body(upstream_id, path, methods)),
        ))
        .await;
    assert_eq!(response.status(), 201, "route must be created");
    let body = read_json(response).await;
    body["id"].as_str().unwrap_or_default().to_string()
}

/// Reads a response body as JSON.
pub async fn read_json(response: axum::http::Response<Body>) -> serde_json::Value {
    let bytes = read_body(response).await;
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

/// Reads a response body as bytes.
pub async fn read_body(response: axum::http::Response<Body>) -> Vec<u8> {
    use http_body_util::BodyExt;
    response.into_body().collect().await.expect("body").to_bytes().to_vec()
}

/// A tenant id distinct from the harness one.
pub fn other_tenant() -> Uuid {
    Uuid::from_u128(2000)
}
