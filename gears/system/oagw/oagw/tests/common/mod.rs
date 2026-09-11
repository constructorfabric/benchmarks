//! Shared harness for the OAGW integration tests.
//!
//! The harness wires the management API and the data plane onto one axum
//! `Router` — exactly as the gear's `register_rest` does — and injects a
//! `SecurityContext` directly, standing in for the gateway's auth middleware.

#![allow(dead_code)]

use async_trait::async_trait;
use axum::body::Body;
use http::Request;
use serde_json::Value;
use tenant_resolver_sdk::{
    GetAncestorsOptions, GetAncestorsResponse, GetDescendantsOptions, GetDescendantsResponse,
    GetTenantsOptions, IsAncestorOptions, TenantId, TenantInfo, TenantRef, TenantResolverClient,
    TenantResolverError, TenantStatus,
};
use toolkit_security::SecurityContext;
use tower::ServiceExt;

use oagw::config::OagwConfig;
use oagw::domain::store::Store;
use oagw::proxy::ProxyState;

pub mod mock_upstream;
#[allow(unused_imports)]
pub use mock_upstream::{MockBody, MockResponse, MockUpstream, echo_upstream, ws_echo_upstream};

/// Tenant with no parent in the test hierarchy.
pub const TENANT_ROOT: uuid::Uuid = uuid::uuid!("00000000-0000-0000-0000-000000000001");
/// Child of [`TENANT_ROOT`].
pub const TENANT_L1A: uuid::Uuid = uuid::uuid!("00000000-0000-0000-0000-000000000002");
/// Second child of [`TENANT_ROOT`].
pub const TENANT_L1B: uuid::Uuid = uuid::uuid!("00000000-0000-0000-0000-000000000003");
/// A tenant outside the hierarchy.
pub const TENANT_OTHER: uuid::Uuid = uuid::uuid!("00000000-0000-0000-0000-000000000009");

/// In-memory tenant hierarchy: `ROOT → L1A`, `ROOT → L1B`.
#[derive(Clone)]
pub struct MockResolver {
    parent_of: Vec<(uuid::Uuid, Option<uuid::Uuid>)>,
}

impl MockResolver {
    /// Builds the resolver over `(tenant, parent)` pairs.
    #[must_use]
    pub fn new(parent_of: Vec<(uuid::Uuid, Option<uuid::Uuid>)>) -> Self {
        Self { parent_of }
    }

    /// The default test hierarchy.
    #[must_use]
    pub fn default_hierarchy() -> Self {
        Self::new(vec![
            (TENANT_ROOT, None),
            (TENANT_L1A, Some(TENANT_ROOT)),
            (TENANT_L1B, Some(TENANT_ROOT)),
            (TENANT_OTHER, None),
        ])
    }

    fn ancestors(&self, id: uuid::Uuid) -> Vec<uuid::Uuid> {
        let mut chain = Vec::new();
        let mut current = Some(id);
        while let Some(t) = current {
            current = self
                .parent_of
                .iter()
                .find(|(id, _)| *id == t)
                .and_then(|(_, parent)| *parent);
            if let Some(t) = current {
                chain.push(t);
            }
        }
        chain
    }
}

#[async_trait]
impl TenantResolverClient for MockResolver {
    async fn get_tenant(
        &self,
        _ctx: &SecurityContext,
        id: TenantId,
    ) -> Result<TenantInfo, TenantResolverError> {
        self.parent_of
            .iter()
            .find(|(t, _)| *t == id.0)
            .map(|(t, parent)| TenantInfo {
                id: TenantId(*t),
                name: format!("tenant-{t}"),
                status: TenantStatus::Active,
                tenant_type: None,
                parent_id: parent.map(TenantId),
                self_managed: false,
            })
            .ok_or(TenantResolverError::TenantNotFound { tenant_id: id })
    }

    async fn get_root_tenant(
        &self,
        ctx: &SecurityContext,
    ) -> Result<TenantInfo, TenantResolverError> {
        let (root, _) = self
            .parent_of
            .iter()
            .find(|(_, parent)| parent.is_none())
            .copied()
            .expect("hierarchy has a root");
        self.get_tenant(ctx, TenantId(root)).await
    }

    async fn get_tenants(
        &self,
        _ctx: &SecurityContext,
        _ids: &[TenantId],
        _options: &GetTenantsOptions,
    ) -> Result<Vec<TenantInfo>, TenantResolverError> {
        Err(TenantResolverError::Internal("unused in tests".to_owned()))
    }

    async fn get_ancestors(
        &self,
        _ctx: &SecurityContext,
        id: TenantId,
        _options: &GetAncestorsOptions,
    ) -> Result<GetAncestorsResponse, TenantResolverError> {
        Ok(GetAncestorsResponse {
            tenant: Self::tenant_ref(id.0, self.parent_of(id.0)),
            ancestors: self
                .ancestors(id.0)
                .into_iter()
                .map(|t| Self::tenant_ref(t, self.parent_of(t)))
                .collect(),
        })
    }

    async fn get_descendants(
        &self,
        _ctx: &SecurityContext,
        id: TenantId,
        _options: &GetDescendantsOptions,
    ) -> Result<GetDescendantsResponse, TenantResolverError> {
        Ok(GetDescendantsResponse {
            tenant: Self::tenant_ref(id.0, self.parent_of(id.0)),
            descendants: Vec::new(),
        })
    }

    async fn is_ancestor(
        &self,
        _ctx: &SecurityContext,
        ancestor: TenantId,
        descendant: TenantId,
        _options: &IsAncestorOptions,
    ) -> Result<bool, TenantResolverError> {
        Ok(self.ancestors(descendant.0).contains(&ancestor.0))
    }
}

impl MockResolver {
    fn parent_of(&self, id: uuid::Uuid) -> Option<uuid::Uuid> {
        self.parent_of
            .iter()
            .find(|(t, _)| *t == id)
            .and_then(|(_, p)| *p)
    }

    fn tenant_ref(id: uuid::Uuid, parent: Option<uuid::Uuid>) -> TenantRef {
        TenantRef {
            id: TenantId(id),
            status: TenantStatus::Active,
            tenant_type: None,
            parent_id: parent.map(TenantId),
            self_managed: false,
        }
    }
}

// Re-exports so the test modules do not repeat the SDK imports.
/// An OAGW instance with the management API and data plane mounted.
pub struct App {
    router: axum::Router,
    pub state: std::sync::Arc<ProxyState>,
    pub store: std::sync::Arc<Store>,
}

impl App {
    /// Builds the app for `tenant`, rooted at `resolver`'s hierarchy.
    #[must_use]
    pub fn new(resolver: MockResolver, security: SecurityContext, config: OagwConfig) -> Self {
        Self::with_store(
            std::sync::Arc::new(Store::new()),
            resolver,
            security,
            config,
        )
    }

    /// Builds the app over an existing store, so several tenants can be seen
    /// against one configuration — which is what the hierarchy tests need.
    #[must_use]
    pub fn with_store(
        store: std::sync::Arc<Store>,
        resolver: MockResolver,
        security: SecurityContext,
        config: OagwConfig,
    ) -> Self {
        let credstore: std::sync::Arc<dyn credstore_sdk::CredStoreClientV1> =
            std::sync::Arc::new(credstore_sdk::test_util::MockCredStoreClient::empty());
        let resolver_client: std::sync::Arc<dyn TenantResolverClient> =
            std::sync::Arc::new(resolver);
        let state = std::sync::Arc::new(ProxyState::new(
            std::sync::Arc::clone(&store),
            config,
            credstore,
            Some(resolver_client),
        ));
        let openapi = toolkit::OpenApiRegistryImpl::new();
        let router = oagw::api::register_routes(
            axum::Router::new(),
            &openapi,
            std::sync::Arc::clone(&state),
        )
        .layer(axum::Extension(security));
        Self {
            router,
            state,
            store,
        }
    }

    /// Sends a request and returns the status plus the parsed body.
    ///
    /// # Panics
    ///
    /// Panics when the router does not answer.
    pub async fn send(
        &mut self,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (http::StatusCode, Value) {
        let builder = Request::builder().method(method).uri(uri);
        let builder = match body.as_ref() {
            Some(_) => builder.header("content-type", "application/json"),
            None => builder,
        };
        let request = builder
            .body(Body::from(body.map(|j| j.to_string()).unwrap_or_default()))
            .expect("request builds");
        let response = self
            .router
            .clone()
            .oneshot(request)
            .await
            .expect("response");
        let status = response.status();
        let bytes = http_body_util::BodyExt::collect(response.into_body())
            .await
            .expect("body")
            .to_bytes();
        let parsed = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into_owned()))
        };
        (status, parsed)
    }

    /// Sends a request and returns the response without parsing the body.
    ///
    /// # Panics
    ///
    /// Panics when the router does not answer.
    pub async fn send_raw(
        &mut self,
        method: &str,
        uri: &str,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> http::Response<axum::body::Body> {
        let builder = Request::builder().method(method).uri(uri);
        let builder = match body.as_ref() {
            Some(_) => builder.header("content-type", "application/json"),
            None => builder,
        };
        let builder = headers
            .iter()
            .fold(builder, |b, (name, value)| b.header(*name, *value));
        let request = builder
            .body(Body::from(body.map(|j| j.to_string()).unwrap_or_default()))
            .expect("request builds");
        self.router
            .clone()
            .oneshot(request)
            .await
            .expect("response")
    }

    /// Reads a response to completion.
    ///
    /// # Panics
    ///
    /// Panics when the body cannot be read.
    pub async fn read(response: http::Response<axum::body::Body>) -> Vec<u8> {
        http_body_util::BodyExt::collect(response.into_body())
            .await
            .expect("body")
            .to_bytes()
            .to_vec()
    }

    /// Serves the router on an ephemeral loopback port and returns its address.
    ///
    /// Streaming tests need a real server: an upgrade answered through
    /// `oneshot` has nothing driving the socket afterwards.
    ///
    /// # Panics
    ///
    /// Panics when the listener cannot be bound or the server fails to start.
    pub async fn serve(self) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            axum::serve(listener, self.router)
                .await
                .expect("server serves");
        });
        addr
    }

    /// Creates an upstream from a JSON template, returning its full id.
    ///
    /// # Panics
    ///
    /// Panics when the upstream is rejected.
    pub async fn create_upstream(&mut self, upstream: Value) -> String {
        let (status, body) = self
            .send("POST", "/oagw/v1/upstreams", Some(upstream))
            .await;
        assert_eq!(status, http::StatusCode::CREATED, "upstream create: {body}");
        body["id"].as_str().expect("id").to_owned()
    }

    /// Creates a route from a JSON template, returning its full id.
    ///
    /// # Panics
    ///
    /// Panics when the route is rejected.
    pub async fn create_route(&mut self, route: Value) -> String {
        let (status, body) = self.send("POST", "/oagw/v1/routes", Some(route)).await;
        assert_eq!(status, http::StatusCode::CREATED, "route create: {body}");
        body["id"].as_str().expect("id").to_owned()
    }
}

/// A `SecurityContext` for `tenant`.
#[must_use]
pub fn context_for(tenant: uuid::Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(uuid::Uuid::new_v4())
        .subject_type("user")
        .subject_tenant_id(tenant)
        .build()
        .expect("security context builds")
}

/// The default data-plane configuration used by the tests.
#[must_use]
pub fn test_config() -> OagwConfig {
    OagwConfig {
        proxy_timeout_secs: 5,
        allow_http_upstream: true,
        ..OagwConfig::default()
    }
}

/// An app whose caller is [`TENANT_A`] with no hierarchy.
#[must_use]
pub fn app() -> App {
    App::new(
        MockResolver::default_hierarchy(),
        context_for(TENANT_L1A),
        test_config(),
    )
}

/// A minimal valid upstream declaration pointing at `host:port`.
#[must_use]
pub fn upstream_body(host: &str, port: u16) -> Value {
    serde_json::json!({
        "protocol": oagw::gts::PROTOCOL_HTTP,
        "server": {"endpoints": [{"scheme": "https", "host": host, "port": port}]},
        "auth": {"type": oagw::gts::auth_plugin::NOOP}
    })
}

/// A minimal route binding `path` to `upstream_id`.
#[must_use]
pub fn route_body(upstream_id: &str, path: &str) -> Value {
    serde_json::json!({
        "upstream_id": upstream_id,
        "match": {"http": {"methods": ["GET", "POST", "PUT", "DELETE"], "path": path}}
    })
}

/// An upstream declaration pointing at a loopback mock, with an explicit alias
/// (IP endpoints do not derive one).
#[must_use]
pub fn loopback_upstream(alias: &str, port: u16) -> Value {
    serde_json::json!({
        "alias": alias,
        "protocol": oagw::gts::PROTOCOL_HTTP,
        "server": {"endpoints": [{"scheme": "http", "host": "127.0.0.1", "port": port}]},
        "auth": {"type": oagw::gts::auth_plugin::NOOP}
    })
}

/// Creates the loopback upstream for `mock` and returns its alias.
///
/// # Panics
///
/// Panics when the upstream is rejected.
pub async fn register_mock(app: &mut App, alias: &str, mock: &MockUpstream) -> String {
    let (status, body) = app
        .send(
            "POST",
            "/oagw/v1/upstreams",
            Some(loopback_upstream(alias, mock.port())),
        )
        .await;
    assert_eq!(status, http::StatusCode::CREATED, "upstream create: {body}");
    body["alias"].as_str().expect("alias").to_owned()
}

/// The proxy path for `alias` plus `suffix`.
#[must_use]
pub fn proxy_path(alias: &str, suffix: &str) -> String {
    if suffix.is_empty() {
        format!("/oagw/v1/proxy/{alias}")
    } else {
        format!("/oagw/v1/proxy/{alias}/{suffix}")
    }
}
