//! Shared harness for the gear's integration tests.
//!
//! The tests drive the gear's own router the way the platform does: the
//! services are built over an in-memory control plane, the router is the one
//! [`register_routes`] produces, and the platform's `SecurityContext`
//! extension is injected the way the authentication middleware would.

#![allow(dead_code)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderValue, Request, StatusCode};
use toolkit::api::OpenApiRegistryImpl;
use toolkit_security::SecurityContext;
use tower::ServiceExt;

use oagw::api::rest::routes::register_routes;
use oagw::config::OagwConfig;
use oagw::domain::repo::ControlPlane;
use oagw::domain::services::management::{ManagementService, SelfChain, TenantChain};
use oagw::infra::metrics::ProxyMetrics;
use oagw::infra::plugins::registry::PluginRegistry;
use oagw::infra::proxy::ProxyService;
use oagw::infra::storage::ControlPlaneStore;

/// The HTTP protocol identifier the wire format uses.
pub mod net;

#[allow(unused_imports)]
pub use net::{bind_upstream, free_port};

pub const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

/// A caller with a fixed tenant, mirroring the graded configuration's
/// static-authenticated subject.
pub struct Caller {
    pub tenant_id: uuid::Uuid,
    pub subject_id: uuid::Uuid,
}

impl Caller {
    #[must_use]
    pub fn new() -> Self {
        Self {
            tenant_id: uuid::Uuid::new_v4(),
            subject_id: uuid::Uuid::new_v4(),
        }
    }

    #[must_use]
    pub fn context(&self) -> SecurityContext {
        SecurityContext::builder()
            .subject_id(self.subject_id)
            .subject_tenant_id(self.tenant_id)
            .build()
            .expect("security context")
    }
}

impl Default for Caller {
    fn default() -> Self {
        Self::new()
    }
}

/// The gear's router over a fresh control plane, plus the services it runs on.
pub struct Harness {
    pub app: Router,
    pub management: Arc<ManagementService>,
    pub proxy: Arc<ProxyService>,
    pub config: OagwConfig,
    /// The caller whose context the router's extension carries.
    pub caller: Caller,
}

/// Builds the gear's router with plaintext upstreams allowed.
pub fn app() -> Router {
    harness(OagwConfig::default()).app
}

/// Builds the router and keeps the services it was built from.
#[must_use]
pub fn harness(config: OagwConfig) -> Harness {
    harness_with_chain(config, Arc::new(SelfChain))
}

/// A credential store the tests fill by hand: `name -> secret value`.
#[derive(Debug, Default)]
pub struct FakeCredStore {
    secrets: parking_lot::Mutex<std::collections::BTreeMap<String, Vec<u8>>>,
}

impl FakeCredStore {
    /// Stores one secret under `name`.
    pub fn put(&self, name: &str, value: &str) -> &Self {
        self.secrets
            .lock()
            .insert(name.to_owned(), value.as_bytes().to_vec());
        self
    }
}

#[async_trait::async_trait]
impl credstore_sdk::CredStoreClientV1 for FakeCredStore {
    async fn get(
        &self,
        _ctx: &SecurityContext,
        key: &credstore_sdk::SecretRef,
    ) -> Result<Option<credstore_sdk::GetSecretResponse>, credstore_sdk::CredStoreError> {
        let store = self.secrets.lock();
        let Some(value) = store.get(key.as_ref()) else {
            return Ok(None);
        };
        Ok(Some(credstore_sdk::GetSecretResponse {
            value: credstore_sdk::SecretValue::new(value.clone()),
            id: uuid::Uuid::nil(),
            owner_tenant_id: credstore_sdk::TenantId(uuid::Uuid::nil()),
            sharing: credstore_sdk::SharingMode::Shared,
            is_inherited: false,
            version: 1,
            secret_type: credstore_sdk::SecretType::generic().gts_id().to_owned(),
            expires_at: None,
        }))
    }
}

/// Builds the router over an explicit tenant chain, for the hierarchy tests.
#[must_use]
pub fn harness_with_chain(config: OagwConfig, chain: Arc<dyn TenantChain>) -> Harness {
    harness_with(config, chain, None)
}

/// Builds the router over a fake credential store, for the auth plugins.
#[must_use]
pub fn harness_with_credstore(config: OagwConfig, credstore: FakeCredStore) -> Harness {
    harness_with(
        config,
        Arc::new(SelfChain),
        Some(Arc::new(credstore) as Arc<dyn credstore_sdk::CredStoreClientV1>),
    )
}

/// Assembles the whole router over an explicit tenant chain and credstore.
fn harness_with(
    mut config: OagwConfig,
    chain: Arc<dyn TenantChain>,
    credstore: Option<Arc<dyn credstore_sdk::CredStoreClientV1>>,
) -> Harness {
    config.allow_http_upstream = true;
    let store: Arc<dyn ControlPlane> = Arc::new(ControlPlaneStore::new());
    let caller = Caller::default();
    let management = Arc::new(ManagementService::new(
        Arc::clone(&store),
        Arc::clone(&chain),
        config.allow_http_upstream,
    ));
    let registry = Arc::new(PluginRegistry::with_builtins(
        config.token_cache_ttl(),
        config.token_cache_capacity,
    ));
    let proxy = ProxyService::new(
        Arc::clone(&store),
        Arc::clone(&chain),
        registry,
        config.clone(),
        ProxyMetrics::new(),
    )
    .expect("proxy service");
    let proxy = Arc::new(proxy.with_credstore(credstore));
    let openapi = OpenApiRegistryImpl::new();
    let app = register_routes(
        Router::new(),
        &openapi,
        Arc::clone(&management),
        proxy.clone(),
    );
    Harness {
        app,
        management,
        proxy,
        config,
        caller,
    }
}

/// Attaches the caller's security context to every request.
///
/// Over a real socket there is no extension to inject, so this stands in for
/// the platform's authentication middleware.
pub fn with_context(app: Router, caller: &Caller) -> Router {
    let context = caller.context();
    app.layer(axum::middleware::from_fn(
        move |mut request: axum::extract::Request, next: axum::middleware::Next| {
            let context = context.clone();
            async move {
                request.extensions_mut().insert(context);
                next.run(request).await
            }
        },
    ))
}

/// Issues a request against the router and returns status, headers and body.
pub async fn send(
    app: &Router,
    caller: &Caller,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> (StatusCode, axum::http::HeaderMap, serde_json::Value) {
    let request = request_with(app, caller, method, path, body).await;
    let (parts, bytes) = request;
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (parts.status, parts.headers, json)
}

/// Issues a request and returns the raw response parts plus body bytes.
///
/// The caller's `SecurityContext` is attached per request, exactly as the
/// platform's authentication middleware would present it to the gear.
pub async fn request_with(
    app: &Router,
    caller: &Caller,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> (axum::http::response::Parts, Vec<u8>) {
    let method: axum::http::Method = method.parse().expect("method");
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", "Bearer test")
        .header("content-type", "application/json")
        .body(Body::from(body.unwrap_or_default().to_owned()))
        .expect("request");
    request.extensions_mut().insert(caller.context());
    let response = app.clone().oneshot(request).await.expect("response");
    let (parts, body) = response.into_parts();
    let bytes = axum::body::to_bytes(body, 32 * 1024 * 1024)
        .await
        .expect("body");
    (parts, bytes.to_vec())
}

/// Issues a request with extra headers, as JSON.
pub async fn send_with_headers(
    app: &Router,
    caller: &Caller,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, axum::http::HeaderMap, serde_json::Value) {
    let method: axum::http::Method = method.parse().expect("method");
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", "Bearer test")
        .header("content-type", "application/json")
        .body(Body::empty())
        .expect("request");
    for (name, value) in headers {
        let name = axum::http::HeaderName::from_bytes(name.as_bytes()).expect("header name");
        let value = HeaderValue::from_str(value).expect("header value");
        request.headers_mut().insert(name, value);
    }
    request.extensions_mut().insert(caller.context());
    let response = app.clone().oneshot(request).await.expect("response");
    let (parts, body) = response.into_parts();
    let bytes = axum::body::to_bytes(body, 32 * 1024 * 1024)
        .await
        .expect("body");
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (parts.status, parts.headers, json)
}

/// Creates an upstream and returns the stored document.
pub async fn create_upstream(app: &Router, caller: &Caller, body: &str) -> serde_json::Value {
    let (status, _, json) = send(app, caller, "POST", "/oagw/v1/upstreams", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "upstream creation: {json}");
    json
}

/// Creates a route against an upstream and returns the stored document.
pub async fn create_route(app: &Router, caller: &Caller, body: &str) -> serde_json::Value {
    let (status, _, json) = send(app, caller, "POST", "/oagw/v1/routes", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "route creation: {json}");
    json
}

/// An upstream document pointed at a plain-HTTP local endpoint.
///
/// The loopback host is not registrable, so the alias is always explicit; the
/// caller chooses it and uses it on the proxy path.
#[must_use]
pub fn upstream_body(alias: &str, port: u16) -> String {
    serde_json::json!({
        "enabled": true,
        "alias": alias,
        "server": { "endpoints": [
            { "scheme": "http", "host": "127.0.0.1", "port": port }
        ]},
        "protocol": PROTOCOL_HTTP,
    })
    .to_string()
}

/// A route document matching `path` with `GET` and appending the suffix.
#[must_use]
pub fn route_body(upstream_id: &str, path: &str) -> String {
    serde_json::json!({
        "enabled": true,
        "upstream_id": upstream_id,
        "match": {
            "http": {
                "methods": ["GET"],
                "path": path,
                "query_allowlist": [],
                "path_suffix_mode": "append"
            }
        }
    })
    .to_string()
}

/// Convenience: header value from a string literal.
#[must_use]
pub fn header(value: &str) -> HeaderValue {
    HeaderValue::from_str(value).expect("header value")
}

/// The no-op auth built-in's GTS id.
#[must_use]
pub fn noop_auth() -> &'static str {
    oagw::domain::gts_helpers::AUTH_NOOP
}

/// The API-key auth built-in's GTS id.
#[must_use]
pub fn apikey_auth() -> &'static str {
    oagw::domain::gts_helpers::AUTH_APIKEY
}

/// The OAuth2 client-credentials auth built-in's GTS id.
#[must_use]
pub fn oauth2_auth() -> &'static str {
    oagw::domain::gts_helpers::AUTH_OAUTH2
}

/// The request-id transform built-in's GTS id.
#[must_use]
pub fn request_id_transform() -> &'static str {
    oagw::domain::gts_helpers::TRANSFORM_REQUEST_ID
}

/// The required-headers guard built-in's GTS id.
#[must_use]
pub fn required_headers_guard() -> &'static str {
    oagw::domain::gts_helpers::GUARD_REQUIRED_HEADERS
}
