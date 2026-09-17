// Created: 2026-09-03 by Constructor Tech
//! Shared harness for the OAGW integration tests.
//!
//! Builds the axum router directly (`register_routes`) and drives it with
//! `Router::oneshot`; the gateway itself never needs a TCP listener. Requests
//! carry a `SecurityContext` extension, which is what the handlers use to
//! scope every operation to a tenant.
#![allow(
    dead_code,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::doc_markdown,
    clippy::missing_panics_doc,
    clippy::too_many_lines,
    clippy::unused_async,
    clippy::large_types_passed_by_value,
    clippy::module_name_repetitions,
    clippy::future_not_send,
    clippy::option_if_let_else
)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, Response, StatusCode};
use bytes::Bytes;
use http_body_util::BodyExt;
use tower::ServiceExt;
use uuid::Uuid;

use oagw::api::handlers::Services;
use oagw::api::routes::register_routes;
use oagw::config::OagwConfig;
use oagw::domain::service::{ControlPlaneService, FlatHierarchy};
use oagw::infra::plugins::PluginRegistry;
use oagw::infra::secrets::SecretResolver;
use oagw::infra::storage::MemoryStore;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationSpec;
use toolkit_security::SecurityContext;

/// Owner of every resource created by the tests by default.
pub const TENANT_A: Uuid = Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_0A01);
/// Second tenant, used to prove that management resources are tenant scoped.
pub const TENANT_B: Uuid = Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_0B02);

/// `X-OAGW-Error-Source` — set to `gateway` on every gateway-originated error.
pub const ERROR_SOURCE_HEADER: &str = "x-oagw-error-source";
/// Header value identifying a gateway-originated response.
pub const ERROR_SOURCE_GATEWAY: &str = "gateway";

/// GTS identifier prefix shared by every OAGW error type.
pub const ERROR_TYPE_PREFIX: &str = "gts.cf.core.errors.err.v1~cf.oagw.";

/// Canonical GTS identifier of the HTTP upstream protocol.
pub const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
/// Canonical GTS identifier of the gRPC upstream protocol.
pub const PROTOCOL_GRPC: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1";

// ── Noop OpenAPI registry ───────────────────────────────────────────────

/// Registry that records nothing; the tests never inspect the OpenAPI document.
struct NoopOpenApiRegistry;

impl OpenApiRegistry for NoopOpenApiRegistry {
    fn register_operation(&self, _spec: &OperationSpec) {}

    fn ensure_schema_raw(
        &self,
        name: &str,
        _schemas: Vec<(String, utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>)>,
    ) -> String {
        name.to_owned()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// ── Router factory ──────────────────────────────────────────────────────

/// Build the OAGW router over in-memory stores.
///
/// `allow_http` maps onto `oagw.config.allow_http_upstream`, which decides
/// whether plaintext `http` endpoints are accepted.
pub fn app(allow_http: bool) -> Router {
    let config = OagwConfig {
        allow_http_upstream: allow_http,
        ..OagwConfig::default()
    };
    let control = Arc::new(ControlPlaneService::new(
        Arc::new(MemoryStore::default()),
        Arc::new(MemoryStore::default()),
        Arc::new(MemoryStore::default()),
        config.allow_http_upstream,
    ));
    let data = Arc::new(oagw::infra::proxy::service::DataPlaneService::new(
        control.clone(),
        Arc::new(FlatHierarchy),
        Arc::new(PluginRegistry::builtin()),
        SecretResolver::absent(),
        None,
        config.clone(),
    ));
    register_routes(
        Router::new(),
        &NoopOpenApiRegistry,
        Services { control, data, config },
    )
}

// ── Request builder ─────────────────────────────────────────────────────

/// An outgoing request against the OAGW router.
pub struct Outgoing {
    method: Method,
    uri: String,
    tenant: Option<Uuid>,
    body: Option<Body>,
    headers: Vec<(&'static str, String)>,
}

impl Outgoing {
    /// Start a request for tenant [`TENANT_A`].
    #[must_use]
    pub fn new(method: Method, uri: impl Into<String>) -> Self {
        Self {
            method,
            uri: uri.into(),
            tenant: Some(TENANT_A),
            body: None,
            headers: Vec::new(),
        }
    }

    /// Send the request as `tenant` instead of [`TENANT_A`].
    #[must_use]
    pub fn tenant(mut self, tenant: Uuid) -> Self {
        self.tenant = Some(tenant);
        self
    }

    /// Send the request without a `SecurityContext` extension.
    #[must_use]
    pub fn unauthenticated(mut self) -> Self {
        self.tenant = None;
        self
    }

    /// Send a JSON body.
    #[must_use]
    pub fn json(mut self, value: serde_json::Value) -> Self {
        self.body = Some(Body::from(value.to_string()));
        self.header("content-type", "application/json")
    }

    /// Attach a request header.
    #[must_use]
    pub fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }

    /// Drive the request through `app` with `Router::oneshot`.
    pub async fn send(self, app: Router) -> Response<Body> {
        let mut builder = Request::builder().method(self.method).uri(self.uri);
        for (name, value) in self.headers {
            builder = builder.header(name, value);
        }
        let mut request = builder
            .body(self.body.unwrap_or_else(Body::empty))
            .expect("well-formed test request");
        if let Some(tenant) = self.tenant {
            let ctx = SecurityContext::builder()
                .subject_id(tenant)
                .subject_type("user")
                .subject_tenant_id(tenant)
                .build()
                .expect("valid security context");
            request.extensions_mut().insert(ctx);
        }
        app.oneshot(request).await.expect("router answers")
    }
}

/// Convenience: `POST` a JSON document.
#[must_use]
pub fn post(uri: impl Into<String>, body: serde_json::Value) -> Outgoing {
    Outgoing::new(Method::POST, uri).json(body)
}

/// Convenience: `PUT` a JSON document.
#[must_use]
pub fn put(uri: impl Into<String>, body: serde_json::Value) -> Outgoing {
    Outgoing::new(Method::PUT, uri).json(body)
}

// ── Response helpers ────────────────────────────────────────────────────

/// Read the whole response body.
pub async fn read_body(response: &mut Response<Body>) -> Bytes {
    BodyExt::collect(std::mem::take(response.body_mut()))
        .await
        .expect("response body")
        .to_bytes()
}

/// Parse the response body as JSON.
pub async fn read_json(response: &mut Response<Body>) -> serde_json::Value {
    let bytes = read_body(response).await;
    serde_json::from_slice(&bytes).expect("response body is JSON")
}

/// Parsed RFC 9457 problem document.
pub struct Problem {
    /// Parsed body.
    pub body: serde_json::Value,
    /// Response status.
    pub status: StatusCode,
    /// Response headers.
    pub headers: axum::http::HeaderMap,
}

impl Problem {
    /// `context.fields` (the platform envelope preserves `context`).
    #[must_use]
    pub fn fields(&self) -> Vec<String> {
        self.body["context"]["fields"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `true` when `context.fields` contains `name`.
    #[must_use]
    pub fn names_field(&self, name: &str) -> bool {
        self.fields().iter().any(|f| f == name)
    }

    /// RFC 9457 `detail`.
    #[must_use]
    pub fn detail(&self) -> &str {
        self.body["detail"].as_str().unwrap_or_default()
    }
}

/// Assert that `response` is an RFC 9457 gateway problem document.
///
/// Checks the `X-OAGW-Error-Source: gateway` header, the `application/problem+json`
/// content type, the GTS `type` prefix, `title`, `status`, the `detail`
/// fragment and the `context.fields` entries.
pub async fn expect_problem(
    mut response: Response<Body>,
    expected_status: u16,
    expected_title: &str,
    detail_contains: &str,
    fields: &[&str],
) -> Problem {
    let status = response.status();
    assert_eq!(
        status.as_u16(),
        expected_status,
        "unexpected status for {detail_contains}"
    );
    assert_eq!(
        response.headers().get(ERROR_SOURCE_HEADER),
        Some(&axum::http::HeaderValue::from_static(ERROR_SOURCE_GATEWAY)),
        "gateway errors must be tagged with {ERROR_SOURCE_HEADER}"
    );
    let content_type = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        content_type.starts_with("application/problem+json"),
        "expected problem+json, got '{content_type}'"
    );
    let bytes = read_body(&mut response).await;
    let body: serde_json::Value =
        serde_json::from_slice(&bytes).expect("problem body is JSON");
    assert_eq!(
        body["status"].as_u64(),
        Some(u64::from(expected_status)),
        "problem status field must mirror the HTTP status"
    );
    let problem_type = body["type"].as_str().unwrap_or_default();
    assert!(
        problem_type.starts_with(ERROR_TYPE_PREFIX),
        "problem 'type' must be an OAGW GTS identifier, got '{problem_type}'"
    );
    let title = body["title"].as_str().unwrap_or_default();
    assert_eq!(
        title, expected_title,
        "problem title mismatch, detail: {}",
        body["detail"]
    );
    let detail = body["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains(detail_contains),
        "problem detail '{detail}' does not mention '{detail_contains}'"
    );
    for field in fields {
        assert!(
            body["context"]["fields"]
                .as_array()
                .is_some_and(|items| items
                    .iter()
                    .any(|f| f.as_str() == Some(field))),
            "problem context.fields {:?} does not name '{field}'",
            body["context"]
        );
    }
    Problem {
        body,
        status,
        headers: response.headers().clone(),
    }
}

/// Assert a successful JSON response and return the parsed body.
pub async fn expect_json(mut response: Response<Body>, expected_status: u16) -> serde_json::Value {
    assert_eq!(response.status().as_u16(), expected_status);
    let content_type = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        content_type.starts_with("application/json"),
        "expected JSON, got '{content_type}'"
    );
    read_json(&mut response).await
}

/// Assert an empty response (204).
pub async fn expect_no_content(mut response: Response<Body>) {
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let bytes = read_body(&mut response).await;
    assert!(bytes.is_empty(), "204 must not carry a body");
}

/// Split a response into status and body bytes.
pub async fn drain(response: Response<Body>) -> (StatusCode, Bytes) {
    let (parts, body) = response.into_parts();
    let bytes = BodyExt::collect(body).await.expect("body").to_bytes();
    (parts.status, bytes)
}
