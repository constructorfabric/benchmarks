#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(dead_code)] // shared harness: each test binary uses a different subset

//! Shared harness for the OAGW integration tests.
//!
//! Builds an in-process `OagwState` (control plane) mounted behind the real
//! management-API router, with a fixed `SecurityContext` supplied as an
//! `axum::Extension` — mirroring how api-gateway injects the authenticated
//! subject on live requests. The data plane proxies to [`httpmock`] upstreams
//! over real HTTP (the test config enables `allow_http_upstream`).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::Value;
use tower::ServiceExt;

use oagw::config::OagwConfig;
use oagw::state::OagwState;
use toolkit::api::OpenApiRegistry;
use toolkit_security::SecurityContext;
use uuid::Uuid;

/// A no-op OpenAPI registry for mounting the operation builder routes.
#[derive(Default)]
pub struct NoopOpenApiRegistry;

impl OpenApiRegistry for NoopOpenApiRegistry {
    fn register_operation(&self, _spec: &toolkit::api::OperationSpec) {}
    fn ensure_schema_raw(
        &self,
        root_name: &str,
        _schemas: Vec<(String, utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>)>,
    ) -> String {
        root_name.to_owned()
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// A fresh tenant id for each test group.
pub fn tenant() -> Uuid {
    Uuid::new_v4()
}

/// An authenticated security context bound to `tenant`.
pub fn sec(tenant: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_type("user")
        .subject_tenant_id(tenant)
        .build()
        .expect("security context builds")
}

/// A data-plane-enabled state with HTTP upstreams allowed (spec default
/// `allow_http_upstream: false`, but proxying to local `http://` mock servers
/// in tests requires it — same toggle e2e-local.yaml uses).
pub fn test_state(_tenant: Uuid) -> Arc<OagwState> {
    let config = OagwConfig {
        allow_http_upstream: Some(true),
        proxy_timeout_secs: Some(5),
        ..Default::default()
    };
    Arc::new(OagwState::new(config).expect("oagw state builds"))
}

/// The full management + proxy router for `state`, authenticating every
/// request as `sec`.
pub fn router(state: Arc<OagwState>, sec: SecurityContext) -> Router {
    let registry = NoopOpenApiRegistry;
    oagw::api::register_routes(axum::Router::new(), &registry, state).layer(axum::Extension(sec))
}

/// Perform an HTTP call against `app`.
pub async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    body: Body,
) -> (StatusCode, axum::http::HeaderMap, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    let req = builder.body(body).expect("request builds");
    let resp = app.clone().oneshot(req).await.expect("oneshot succeeds");
    let status = resp.status();
    let headers_out = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body reads");
    let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, headers_out, value)
}

/// JSON POST helper.
pub async fn post_json(app: &Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let (status, _headers, value) = call(
        app,
        "POST",
        uri,
        &[("content-type", "application/json")],
        axum::body::Body::from(body.to_string()),
    )
    .await;
    (status, value)
}

/// JSON PUT helper.
pub async fn put_json(app: &Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let (status, _headers, value) = call(
        app,
        "PUT",
        uri,
        &[("content-type", "application/json")],
        axum::body::Body::from(body.to_string()),
    )
    .await;
    (status, value)
}

// ---------------------------------------------------------------------------
// Fixture builders
// ---------------------------------------------------------------------------

/// Upstream create body hitting `server` (a local mock) at `port` with an
/// explicit alias.
pub fn upstream_body(host: &str, port: Option<u16>, alias: Option<&str>) -> Value {
    let endpoint = match port {
        Some(p) => serde_json::json!({ "scheme": "http", "host": host, "port": p }),
        None => serde_json::json!({ "scheme": "https", "host": host }),
    };
    let mut body = serde_json::json!({
        "enabled": true,
        "server": { "endpoints": [ endpoint ] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "plugins": { "sharing": "inherit", "items": [] },
    });
    if let Some(a) = alias {
        body["alias"] = serde_json::json!(a);
    }
    body
}

/// Route create body matching `path` for `methods` against `upstream_id`.
pub fn route_body(upstream_id: Uuid, path: &str, methods: &[&str]) -> Value {
    serde_json::json!({
        "tags": [],
        "upstream_id": upstream_id.to_string(),
        "match": {
            "http": {
                "methods": methods,
                "path": path
            }
        },
        "plugins": { "sharing": "inherit", "items": [] },
    })
}
