// Created: 2026-09-02 by Constructor Tech
//! Shared harness for the `oagw` integration tests.
//!
//! The gateway is exercised through its real REST surface: the routes are
//! registered the way the gear registers them, and the caller's identity is
//! injected the way the api-gateway's auth middleware does — as a
//! [`SecurityContext`] extension.

// Each integration-test binary compiles this module and uses a different
// subset of it, so unused helpers are expected rather than a defect.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, header};
use axum::middleware::Next;
use axum::extract::Request as ExtractRequest;
use oagw::api::handlers::{ManagementState, ProxyState};
use oagw::api::routes::register_routes;
use oagw::config::{OagwConfig, SsrfPolicy};
use oagw::domain::plugin::{AuthPluginRegistry, GuardPluginRegistry, SecretResolver, TransformPluginRegistry};
use oagw::domain::service::ControlPlane;
use oagw::domain::store::Store;
use oagw::infra::client::{NoopSecretResolver, UpstreamClient};
use oagw::infra::proxy::ProxyService;
use tokio::net::TcpListener;
use toolkit::api::openapi_registry::OpenApiRegistryImpl;
use tower::ServiceExt;
use uuid::Uuid;

/// The two tenants the tests split their resources across.
pub const TENANT_A: Uuid = Uuid::from_u128(0xa);
pub const TENANT_B: Uuid = Uuid::from_u128(0xb);

/// A tenant, as a value rather than a constant, for test-local plumbing.
#[derive(Clone, Copy, Debug)]
pub enum TestTenant {
    /// The default tenant.
    A,
    /// The other tenant.
    B,
}

impl From<TestTenant> for Uuid {
    fn from(tenant: TestTenant) -> Self {
        match tenant {
            TestTenant::A => TENANT_A,
            TestTenant::B => TENANT_B,
        }
    }
}

/// The identity the gateway would have resolved from the bearer token.
pub fn security_context(tenant_id: Uuid) -> toolkit_security::SecurityContext {
    toolkit_security::SecurityContext::builder()
        .subject_id(tenant_id)
        .subject_tenant_id(tenant_id)
        .subject_type("user")
        .build()
        .unwrap()
}

/// A gateway wired the way the gear wires it, with the caller injected.
///
/// Requests carry the caller in the `x-oagw-test-tenant` header so a single
/// router can serve both tenants.
pub async fn gateway(config: OagwConfig) -> Router {
    build(config).layer(axum::middleware::from_fn(inject_caller))
}

fn build(config: OagwConfig) -> Router {
    let store = Arc::new(Store::new());
    let secrets: Arc<dyn SecretResolver> = Arc::new(NoopSecretResolver);
    let control_plane = Arc::new(
        ControlPlane::new(store, config.list_top_default, config.list_top_max)
            .with_secrets(secrets.clone()),
    );
    let client = UpstreamClient::new(
        config.allow_http_upstream,
        Duration::from_secs(config.connect_timeout_secs),
        config.ssrf_policy.clone(),
    );
    let auth = AuthPluginRegistry::with_builtins(
        secrets,
        config.token_cache_ttl_secs,
        config.token_cache_capacity,
    );
    let proxy = Arc::new(ProxyService::new(
        control_plane.clone(),
        client,
        auth,
        GuardPluginRegistry::with_builtins(),
        TransformPluginRegistry::with_builtins(),
        config,
    ));
    let openapi = OpenApiRegistryImpl::new();
    register_routes(
        Router::new(),
        &openapi,
        ManagementState {
            control_plane,
            resolver: None,
        },
        ProxyState { proxy },
    )
}

/// Serves `router` on an ephemeral loopback port and returns its address.
pub async fn serve(router: Router) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    addr
}

async fn inject_caller(mut request: ExtractRequest, next: Next) -> axum::response::Response {
    let tenant = request
        .headers()
        .get("x-oagw-test-tenant")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| Uuid::parse_str(v).ok())
        .unwrap_or(TENANT_A);
    request.extensions_mut().insert(security_context(tenant));
    next.run(request).await
}

/// An SSRF policy that never screens, so tests may dial loopback upstreams.
pub fn no_ssrf() -> SsrfPolicy {
    SsrfPolicy {
        enabled: false,
        ..SsrfPolicy::default()
    }
}

/// The default gateway configuration, with plaintext upstreams allowed.
pub fn default_config() -> OagwConfig {
    OagwConfig {
        allow_http_upstream: true,
        ssrf_policy: SsrfPolicy {
            enabled: false,
            ..SsrfPolicy::default()
        },
        ..OagwConfig::default()
    }
}

/// A request from a named test tenant, for the tests that need both.
pub fn test_request(
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
    tenant: impl Into<Uuid>,
) -> axum::http::Request<Body> {
    request(method, path, body, tenant.into())
}

/// A management `POST`/`PUT` with a JSON body, as tenant A.
pub async fn post_json(router: &Router, method: &str, path: &str, body: serde_json::Value) -> (u16, serde_json::Value) {
    let response = router
        .clone()
        .oneshot(request(method, path, Some(body), TENANT_A))
        .await
        .unwrap();
    let status = response.status().as_u16();
    (status, json_of(response).await)
}

/// A request with the caller's tenant in the test header.
pub fn request(
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
    tenant: Uuid,
) -> axum::http::Request<Body> {
    // The OData `$filter` values carry spaces, which a URI literal may not.
    let (base, query) = match path.split_once('?') {
        Some((base, query)) => (base, Some(percent_encode(query))),
        None => (path, None),
    };
    let uri = match query {
        Some(query) => format!("{base}?{query}"),
        None => base.to_owned(),
    };
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("x-oagw-test-tenant", tenant.to_string());
    if body.is_some() {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
    }
    let payload = body
        .map(|value| Body::from(serde_json::to_vec(&value).unwrap()))
        .unwrap_or_else(Body::empty);
    builder.body(payload).unwrap()
}

/// Percent-encodes a query string so an OData expression is a legal URI.
fn percent_encode(query: &str) -> String {
    let mut out = String::with_capacity(query.len());
    for byte in query.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'$'
            | b'&' | b'=' | b'/' | b'?' | b'#' | b':' | b'@' | b'!' | b'\'' | b'(' | b')'
            | b'*' | b',' | b';' | b'[' | b']' => out.push(byte as char),
            b' ' => out.push_str("%20"),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// The response body as JSON, or `Value::Null` for an empty one.
pub async fn json_of(response: axum::response::Response) -> serde_json::Value {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    }
}

/// The response body as UTF-8 text.
pub async fn text_of(response: axum::response::Response) -> String {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

