// Created: 2026-09-03 by Constructor Tech
//! Shared fixtures of the OAGW integration tests.
//!
//! The routers under test are the real `full_router` surface, exercised
//! through `tower::ServiceExt::oneshot` so the wiring (extension injection,
//! gear-relative paths, problem documents) is covered end to end.

#![allow(dead_code)]

use std::sync::Arc;

use axum::Router;
use http::header::CONTENT_TYPE;
use http::{HeaderMap, Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

use oagw::model::{Endpoint, EndpointScheme, HttpMatch, HttpMethod, PathSuffixMode, RouteMatch, ServerConfig};
use oagw::state::OagwState;

/// The tenant every fixture authenticates as.
#[must_use]
pub fn tenant() -> Uuid {
    Uuid::new_v4()
}

/// A security context bound to `tenant`.
#[must_use]
pub fn security_context(tenant: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_type("service")
        .subject_tenant_id(tenant)
        .token_scopes(vec!["*".to_owned()])
        .build()
        .expect("security context")
}

/// A state with the plaintext-upstream allowance of the e2e configuration.
#[must_use]
pub fn state(allow_http: bool) -> Arc<OagwState> {
    let config = oagw::config::OagwConfig {
        allow_http_upstream: allow_http,
        proxy_timeout_secs: 5,
        ..oagw::config::OagwConfig::default()
    };
    state_with(config)
}

/// A state built from an explicit gear configuration.
#[must_use]
pub fn state_with(config: oagw::config::OagwConfig) -> Arc<OagwState> {
    Arc::new(OagwState::new(config))
}

/// The real REST surface with the caller identity layered on.
///
/// # Panics
/// Panics when the OpenAPI registration fails, which would be an authoring
/// defect rather than a runtime condition.
pub fn router(state: Arc<OagwState>, sec: SecurityContext) -> Router {
    let openapi = toolkit::OpenApiRegistryImpl::new();
    oagw::api::full_router(state, &openapi)
        .expect("router")
        .layer(axum::Extension(sec))
}

/// Sends a JSON request to the router and returns the status, headers and body.
///
/// # Panics
/// Panics when the service, the JSON encoding or the body buffering fails.
pub async fn send_json(
    app: Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, HeaderMap, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header(CONTENT_TYPE, "application/json");
    }
    let request = builder
        .body(axum::body::Body::from(
            body.map_or_else(String::new, |value| value.to_string()),
        ))
        .expect("request");
    let response = app.oneshot(request).await.expect("response");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.expect("body").to_bytes();
    let document = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, headers, document)
}

/// Sends a request with an opaque body and returns the status and body bytes.
///
/// # Panics
/// Panics when the service or the body buffering fails.
pub async fn send_bytes(
    app: Router,
    method: Method,
    uri: &str,
    body: &'static [u8],
) -> (StatusCode, HeaderMap, bytes::Bytes) {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .body(axum::body::Body::from(body))
        .expect("request");
    let response = app.oneshot(request).await.expect("response");
    let status = response.status();
    let headers = response.headers().clone();
    let payload = response.into_body().collect().await.expect("body").to_bytes();
    (status, headers, payload)
}

/// A plaintext endpoint pointing at the local mock server.
#[must_use]
pub fn http_endpoint(host: &str, port: u16) -> Endpoint {
    Endpoint {
        scheme: EndpointScheme::Http,
        host: host.to_owned(),
        port: Some(port),
    }
}

/// A TLS endpoint pointing at `host`.
#[must_use]
pub fn https_endpoint(host: &str, port: u16) -> Endpoint {
    Endpoint {
        scheme: EndpointScheme::Https,
        host: host.to_owned(),
        port: Some(port),
    }
}

/// An upstream creation payload with a single endpoint.
#[must_use]
pub fn upstream_input(endpoints: Vec<Endpoint>) -> Value {
    json!({
        "server": ServerConfig { endpoints },
        "protocol": "http"
    })
}

/// An upstream creation payload as raw JSON.
#[must_use]
pub fn upstream_payload(endpoints: Value) -> Value {
    json!({ "server": { "endpoints": endpoints }, "protocol": "http" })
}

/// The alias the API derives for a single-endpoint upstream.
#[must_use]
pub fn alias_of(endpoint: &Endpoint) -> String {
    format!("{}:{}", endpoint.host, endpoint.port())
}

/// An HTTP match rule as the API accepts it.
#[must_use]
pub fn route_match_value(methods: Vec<&str>, path: &str) -> Value {
    json!({
        "http": {
            "methods": methods,
            "path": path,
            "path_suffix_mode": "append"
        }
    })
}

/// The HTTP match model of a route, for direct store fixtures.
#[must_use]
pub fn route_match(methods: Vec<HttpMethod>, path: &str) -> RouteMatch {
    RouteMatch {
        http: Some(HttpMatch {
            methods,
            path: path.to_owned(),
            query_allowlist: Vec::new(),
            path_suffix_mode: PathSuffixMode::Append,
        }),
        grpc: None,
    }
}

/// A `ServerConfig` fixture.
#[must_use]
pub fn server(endpoints: Vec<Endpoint>) -> ServerConfig {
    ServerConfig { endpoints }
}

/// The GTS identifier of a stored upstream.
#[must_use]
pub fn upstream_gts(id: Uuid) -> String {
    oagw::gts::upstream_id(id)
}

/// Asserts the problem-document framing and returns its `type` member.
///
/// # Panics
/// Panics when the response is not a gateway problem document of
/// `expected_status`.
pub fn assert_problem(status: StatusCode, headers: &HeaderMap, document: &Value, expected: u16) -> String {
    assert_eq!(u16::from(status), expected, "body: {document}");
    assert_eq!(
        headers
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default(),
        "application/problem+json"
    );
    assert_eq!(
        headers
            .get("x-oagw-error-source")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default(),
        "gateway"
    );
    assert_eq!(document["status"], expected);
    let kind = document["type"].as_str().unwrap_or_default().to_owned();
    assert!(
        kind.starts_with("gts.cf.core.errors.err.v1~cf.oagw."),
        "type: {kind}"
    );
    assert!(document["title"].is_string());
    assert!(document["detail"].is_string());
    kind
}

/// The `type` member of a response that is not framed as a problem document.
#[must_use]
pub fn error_type(document: &Value) -> &str {
    document["type"].as_str().unwrap_or_default()
}

/// Asserts the gateway framing of an error response without assuming a status.
///
/// Returns the `type` member of the problem document.
///
/// # Panics
/// Panics when the response is not a gateway problem document.
pub fn assert_gateway_problem(status: StatusCode, headers: &HeaderMap, document: &Value) -> String {
    let code = u16::from(status);
    assert!(
        (400..600).contains(&code),
        "status: {code} body: {document}"
    );
    assert_eq!(document["status"], code, "body: {document}");
    assert_eq!(
        headers
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default(),
        "application/problem+json"
    );
    assert_eq!(
        headers
            .get("x-oagw-error-source")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default(),
        "gateway"
    );
    assert!(document["title"].is_string());
    assert!(document["detail"].is_string());
    let kind = document["type"].as_str().unwrap_or_default().to_owned();
    assert!(
        kind.starts_with("gts.cf.core.errors.err.v1~cf.oagw."),
        "type: {kind}"
    );
    kind
}
