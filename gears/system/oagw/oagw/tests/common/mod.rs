//! Shared harness for the OAGW integration tests.
//!
//! The harness assembles a real axum [`Router`] with the OAGW REST routes and
//! drives it with `tower::ServiceExt::oneshot`, so every test exercises the
//! same handler stack the host server mounts (extensions, extractors, problem
//! rendering).
// `dead_code` is allowed because this module is compiled once per test binary:
// an item exercised only by one of them looks unused to the other.
#![allow(
    dead_code,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::doc_markdown
)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderName, HeaderValue, Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use oagw::domain::plugin::{PluginError, ResolvedSecret, SecretResolver};
use oagw::domain::service::ControlPlaneService;
use oagw::infra::state::{GearState, StaticTenantChain, TenantChain};
use oagw::infra::store::InMemoryStore;
use oagw::proxy::data_plane::DataPlane;
use oagw::proxy::ratelimit::{RateLimiter, SharedRateLimiter};
use toolkit::api::OpenApiRegistryImpl;
use toolkit_security::SecurityContext;

/// Default tenant used by the single-tenant harness.
pub const TENANT: uuid::Uuid = uuid::Uuid::from_u128(0x0a1);
/// A second tenant, used to assert cross-tenant isolation.
pub const OTHER_TENANT: uuid::Uuid = uuid::Uuid::from_u128(0x0a2);

/// A security context bound to [`TENANT`].
#[must_use]
pub fn tenant_context() -> SecurityContext {
    context_for(TENANT)
}

/// A security context for an arbitrary tenant.
#[must_use]
pub fn context_for(tenant: uuid::Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(uuid::Uuid::from_u128(0xbeef))
        .subject_type("user")
        .subject_tenant_id(tenant)
        .build()
        .expect("security context")
}

/// An unauthenticated context (no tenant, no subject).
#[must_use]
pub fn anonymous_context() -> SecurityContext {
    SecurityContext::anonymous()
}

/// Assembled gear, shared by every router built from it.
pub struct Harness {
    state: GearState,
    service: Arc<ControlPlaneService>,
    plane: Arc<DataPlane>,
    limiter: SharedRateLimiter,
}

impl Harness {
    /// Builds a harness with the default configuration.
    #[must_use]
    pub fn new() -> Self {
        Self::with_config(oagw::config::OagwConfig::default())
    }

    /// Builds a harness over an explicit configuration and the fail-closed
    /// credential resolver.
    #[must_use]
    pub fn with_config(config: oagw::config::OagwConfig) -> Self {
        Self::build(config, Arc::new(StaticTenantChain), Arc::new(FailSecrets))
    }

    /// Builds a harness over an explicit configuration, credential resolver and
    /// tenant-chain provider.
    #[must_use]
    pub fn build(
        config: oagw::config::OagwConfig,
        tenants: Arc<dyn TenantChain>,
        secrets: Arc<dyn SecretResolver>,
    ) -> Self {
        let store = InMemoryStore::new();
        let state = GearState::from_parts(config.clone(), store.clone(), secrets, tenants);
        let limiter = SharedRateLimiter::new(Arc::new(RateLimiter::new()));
        let plane = Arc::new(DataPlane::new(state.clone(), limiter.clone()));
        let service = Arc::new(ControlPlaneService::new(store.clone()));
        Self {
            state,
            service,
            plane,
            limiter,
        }
    }

    /// Builds a router bound to `security`.
    ///
    /// The host server injects the authenticated identity as an extension; the
    /// tests do the same so the handlers see a realistic stack.
    pub fn router(&self, security: SecurityContext) -> Router {
        oagw::api::rest::routes::register_routes(
            Router::new(),
            &OpenApiRegistryImpl::new(),
            self.state.clone(),
            (*self.service).clone(),
            Arc::clone(&self.plane),
            self.limiter.clone(),
        )
        .layer(axum::Extension(security))
    }

    /// Sends a JSON request through the router.
    ///
    /// # Panics
    ///
    /// Panics when the router service itself fails (never expected).
    pub async fn json(
        &self,
        security: SecurityContext,
        method: Method,
        path: &str,
        body: Value,
    ) -> axum::response::Response {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("request");
        self.send(security, request).await
    }

    /// Sends a raw request through the router.
    ///
    /// # Panics
    ///
    /// Panics when the router service itself fails (never expected).
    pub async fn send(
        &self,
        security: SecurityContext,
        request: Request<Body>,
    ) -> axum::response::Response {
        self.router(security)
            .oneshot(request)
            .await
            .expect("response")
    }
}

impl Default for Harness {
    fn default() -> Self {
        Self::new()
    }
}

/// `X-OAGW-Error-Source` response header name.
pub const ERROR_SOURCE: &str = "x-oagw-error-source";

/// Reads the whole body as UTF-8 text.
///
/// The response is left intact (status, headers and a buffered body), so a
/// caller may inspect headers after reading the body.
///
/// # Panics
///
/// Panics when the body cannot be buffered or decoded.
pub async fn text(response: &mut axum::response::Response) -> String {
    let taken = std::mem::take(response);
    let (parts, body) = taken.into_parts();
    let bytes = body.collect().await.expect("body").to_bytes();
    *response = axum::response::Response::from_parts(parts, Body::from(bytes.clone()));
    String::from_utf8(bytes.to_vec()).expect("utf-8 body")
}

/// Reads the whole body as JSON.
///
/// # Panics
///
/// Panics when the body is not valid JSON.
pub async fn json(response: &mut axum::response::Response) -> Value {
    let raw = text(response).await;
    serde_json::from_str(&raw).unwrap_or_else(|_| panic!("json body, got: {raw}"))
}

/// Asserts the RFC 9457 envelope of a gateway-originated failure and returns
/// the parsed body.
///
/// # Panics
///
/// Panics on any deviation from the problem contract.
pub async fn assert_problem(
    response: &mut axum::response::Response,
    expected_status: StatusCode,
) -> Value {
    assert_eq!(response.status(), expected_status, "status mismatch");
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/problem+json"),
        "gateway failures are always problem+json"
    );
    assert_eq!(
        response
            .headers()
            .get(ERROR_SOURCE)
            .and_then(|value| value.to_str().ok()),
        Some("gateway"),
        "gateway failures are stamped with the gateway error source"
    );
    let body = json(response).await;
    for member in ["type", "title", "status", "detail"] {
        assert!(
            body.get(member).is_some(),
            "problem body missing '{member}': {body}"
        );
    }
    assert_eq!(body["status"], expected_status.as_u16());
    body
}

/// Builds a request with the supplied method and headers.
#[must_use]
pub fn request(method: Method, path: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(path);
    for (name, value) in headers {
        builder = builder.header(
            HeaderName::from_bytes(name.as_bytes()).expect("header name"),
            HeaderValue::from_bytes(value.as_bytes()).expect("header value"),
        );
    }
    builder.body(Body::empty()).expect("request")
}

/// Credential resolver that never resolves anything (fail closed).
pub struct FailSecrets;

#[async_trait::async_trait]
impl SecretResolver for FailSecrets {
    async fn resolve(
        &self,
        _ctx: &SecurityContext,
        _reference: &str,
    ) -> Result<Option<ResolvedSecret>, PluginError> {
        Ok(None)
    }
}

/// In-memory credential resolver backed by `(reference, value)` pairs.
pub struct FixedSecrets(pub Vec<(String, String)>);

#[async_trait::async_trait]
impl SecretResolver for FixedSecrets {
    async fn resolve(
        &self,
        _ctx: &SecurityContext,
        reference: &str,
    ) -> Result<Option<ResolvedSecret>, PluginError> {
        let bare = reference.strip_prefix("cred://").unwrap_or(reference);
        Ok(self
            .0
            .iter()
            .any(|(key, _)| key == bare)
            .then(|| {
                self.0
                    .iter()
                    .find(|(key, _)| key == bare)
                    .map(|(_, value)| ResolvedSecret::new(value.clone()))
            })
            .flatten())
    }
}

/// A tenant chain with a fixed shape (descendant → root).
pub struct FixedChain(pub Vec<uuid::Uuid>);

#[async_trait::async_trait]
impl TenantChain for FixedChain {
    async fn chain(
        &self,
        _ctx: &SecurityContext,
    ) -> Result<Vec<uuid::Uuid>, oagw::domain::error::DomainError> {
        Ok(self.0.clone())
    }
}
