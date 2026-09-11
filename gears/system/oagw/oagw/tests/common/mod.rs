//! Shared test harness for the `oagw` integration suite.
//!
//! Every test file builds its own router (or two, sharing one [`OagwState`],
//! for tenant-isolation tests) via [`build_router`] / [`router_for_tenant`],
//! drives it through [`tower::ServiceExt::oneshot`], and reads the result
//! through [`send`].

// This module is test-support code only (never built into the shipped
// crate); `unwrap`/`expect` here are exactly what `allow-unwrap-in-tests` /
// `allow-expect-in-tests` in `/app/clippy.toml` are meant to permit, but that
// heuristic only recognises `#[test]`-attributed functions themselves, not
// the helpers a test calls into — hence the explicit allow.
#![allow(dead_code, clippy::expect_used, clippy::unwrap_used)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::{Extension, Router};
use bytes::Bytes;
use credstore_sdk::CredStoreClientV1;
use http::{HeaderMap, Method};
use oagw::api::rest::routes::register_routes;
use oagw::api::rest::state::OagwState;
use oagw::config::{OagwConfig, SsrfPolicyConfig};
use std::sync::Arc;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationSpec;
use toolkit_http::HttpClient;
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use utoipa::openapi::RefOr;
use utoipa::openapi::schema::Schema;
use uuid::Uuid;

/// A no-op `OpenAPI` registry: the suite only needs the router side effects of
/// registration, never the generated document.
pub struct NoopRegistry;

impl OpenApiRegistry for NoopRegistry {
    fn register_operation(&self, _spec: &OperationSpec) {}

    fn ensure_schema_raw(&self, name: &str, _schemas: Vec<(String, RefOr<Schema>)>) -> String {
        name.to_owned()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Fixed, non-nil tenant used by every test unless isolation is under test.
#[must_use]
pub fn tenant_a() -> Uuid {
    Uuid::from_u128(0x1111_1111_1111_1111_1111_1111_1111_1111)
}

/// A second, distinct tenant for cross-tenant isolation assertions.
#[must_use]
pub fn tenant_b() -> Uuid {
    Uuid::from_u128(0x2222_2222_2222_2222_2222_2222_2222_2222)
}

/// Build a `SecurityContext` for the given tenant with a fresh random subject.
#[must_use]
pub fn security_context_for(tenant: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(tenant)
        .build()
        .expect("subject_id and subject_tenant_id are both set")
}

/// The gear configuration the graded deployment runs with: plaintext
/// upstreams allowed, the SSRF policy check still runs but does not enforce
/// (loopback mock servers are otherwise rejected), and generous cache/timeout
/// defaults.
#[must_use]
pub fn base_config() -> OagwConfig {
    OagwConfig {
        proxy_timeout_secs: 30,
        allow_http_upstream: true,
        ssrf_policy: SsrfPolicyConfig { enabled: false },
        token_cache_ttl_secs: 300,
        token_cache_capacity: 10_000,
    }
}

/// [`base_config`] with a caller-supplied proxy timeout, for the streaming
/// tests that must prove the timeout bounds only the response head.
#[must_use]
pub fn config_with_timeout(secs: u64) -> OagwConfig {
    OagwConfig {
        proxy_timeout_secs: secs,
        ..base_config()
    }
}

/// An `HttpClient` suitable for tests: retries disabled so mock-server
/// assertions on call counts and timing are deterministic.
fn test_http_client() -> HttpClient {
    HttpClient::builder()
        .retry(None)
        .build()
        .expect("http client builds without TLS material")
}

/// Build a fresh router and its shared state, with `tenant_a`'s security
/// context layered on so handlers see an authenticated identity.
pub fn build_router(config: OagwConfig) -> (Router, Arc<OagwState>) {
    build_router_with_cred_store(config, None)
}

/// Like [`build_router`], but with a credential store wired in (for the
/// `apikey` auth-plugin tests that resolve a `secret_ref`).
pub fn build_router_with_cred_store(
    config: OagwConfig,
    cred_store: Option<Arc<dyn CredStoreClientV1>>,
) -> (Router, Arc<OagwState>) {
    let state = Arc::new(OagwState::new(config, test_http_client(), cred_store));
    let router = router_for_tenant(&state, tenant_a());
    (router, state)
}

/// Build a second router sharing `state`'s store but scoped to a different
/// tenant's security context — the vehicle for cross-tenant isolation tests.
pub fn router_for_tenant(state: &Arc<OagwState>, tenant: Uuid) -> Router {
    register_routes(Router::new(), &NoopRegistry, state.clone())
        .layer(Extension(security_context_for(tenant)))
}

/// A collected response: status, headers and the fully-buffered body.
///
/// Streaming tests (SSE / `WebSocket`) do not use this — they need the body
/// as an incremental stream and read it directly.
pub struct TestResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub bytes: Bytes,
}

impl TestResponse {
    /// Parse the body as JSON.
    ///
    /// # Panics
    /// Panics with the raw body text if it is not valid JSON — a clearer
    /// failure than a generic `serde_json` error for a misbehaving handler.
    #[must_use]
    pub fn json(&self) -> serde_json::Value {
        if self.bytes.is_empty() {
            return serde_json::Value::Null;
        }
        serde_json::from_slice(&self.bytes).unwrap_or_else(|e| {
            panic!(
                "response body is not valid JSON: {e}; status={}; body={}",
                self.status,
                self.text()
            )
        })
    }

    /// The body decoded as UTF-8 (lossily, so a failure never panics here).
    #[must_use]
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }

    /// A header value as `&str`, or `None` if absent / not valid UTF-8.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

/// Drive `req` through `router` (cloned, so the router can be reused across
/// several requests in one test) and collect the full response.
pub async fn send(router: &Router, req: Request<Body>) -> TestResponse {
    let response = router
        .clone()
        .oneshot(req)
        .await
        .expect("router is infallible");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = http_body_util::BodyExt::collect(response.into_body())
        .await
        .expect("response body reads without a transport error")
        .to_bytes();
    TestResponse {
        status,
        headers,
        bytes,
    }
}

/// Build a request with a JSON body and the matching `Content-Type`.
#[must_use]
pub fn json_request(method: Method, uri: &str, body: &serde_json::Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("well-formed request")
}

/// Build a bodyless request.
#[must_use]
pub fn empty_request(method: Method, uri: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .expect("well-formed request")
}

/// Start building a request, for callers that need extra headers or a
/// non-JSON body.
#[must_use]
pub fn request(method: Method, uri: &str) -> http::request::Builder {
    Request::builder().method(method).uri(uri)
}

/// `POST` a JSON body to `path` and assert the setup step itself succeeded,
/// returning the decoded response body.
///
/// # Panics
/// Panics (with the response body in the message) if the create did not
/// return `201`. This is deliberate: a broken setup step should fail loudly
/// at the point of the mistake, not surface as a confusing assertion failure
/// three lines later in the test proper.
pub async fn create(router: &Router, path: &str, body: &serde_json::Value) -> serde_json::Value {
    let resp = send(router, json_request(Method::POST, path, body)).await;
    assert_eq!(
        resp.status,
        StatusCode::CREATED,
        "setup POST {path} failed: {}",
        resp.text()
    );
    resp.json()
}
