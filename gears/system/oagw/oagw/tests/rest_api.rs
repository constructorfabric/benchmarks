//! Router-level tests for the OAGW management REST API.
//!
//! Exercises `register_routes` end-to-end with `Router::oneshot`: status
//! codes, RFC 9457 problem+json bodies, the `X-OAGW-Error-Source: gateway`
//! header on gateway errors (ADR 0007), and OData-lite list behavior.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use oagw::api::rest::routes::register_routes;
use oagw::config::OagwConfig;
use oagw::domain::service::ControlPlaneService;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationSpec;
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

/// Minimal `OpenAPI` registry — route registration only records specs.
struct NoopOpenApiRegistry;

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

fn router() -> Router {
    let service = Arc::new(ControlPlaneService::new(OagwConfig::default()));
    let registry = NoopOpenApiRegistry;
    register_routes(Router::new(), &registry, service, None, None, None)
}

fn ctx_for(tenant: u64) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(Uuid::from_u128(u128::from(tenant)))
        .build()
        .unwrap()
}

/// Put the security context into the request extensions, mirroring the host's
/// auth middleware (`toolkit` injects `Extension<SecurityContext>`; axum reads
/// it back out of `extensions_mut`).
async fn send_auth(
    router: Router,
    method: &str,
    path: &str,
    ctx: SecurityContext,
    body: Option<serde_json::Value>,
) -> (StatusCode, axum::http::HeaderMap, serde_json::Value) {
    send_auth_headered(router, method, path, ctx, &[], body).await
}

/// Like [`send_auth`], but with explicit extra request headers (e.g. a
/// caller-supplied `X-Request-ID`).
async fn send_auth_headered(
    router: Router,
    method: &str,
    path: &str,
    ctx: SecurityContext,
    extra_headers: &[(&str, &str)],
    body: Option<serde_json::Value>,
) -> (StatusCode, axum::http::HeaderMap, serde_json::Value) {
    let mut builder = Request::builder().method(method).uri(path);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    for (name, value) in extra_headers {
        builder = builder.header(*name, *value);
    }
    let mut req = builder.body(Body::empty()).unwrap();
    req.extensions_mut().insert(ctx);
    let body = body.map(|j| j.to_string().into_bytes());
    let (parts, _old) = req.into_parts();
    let new_body = if let Some(bytes) = body {
        Body::from(bytes)
    } else {
        Body::empty()
    };
    let req = Request::from_parts(parts, new_body);
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, headers, json)
}

fn host_json(host: &str, port: u16) -> serde_json::Value {
    serde_json::json!({
        "server": { "endpoints": [{ "scheme": "https", "host": host, "port": port }] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
    })
}

#[tokio::test]
async fn create_upstream_returns_201_location_and_derived_alias() {
    let app = router();
    let ctx = ctx_for(1);
    let (status, headers, json) = send_auth(
        app,
        "POST",
        "/oagw/v1/upstreams",
        ctx,
        Some(host_json("api.example.com", 443)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(headers.get("location").is_some());
    assert_eq!(json["alias"], "api.example.com");
}

#[tokio::test]
async fn duplicate_alias_is_409_problem_with_gateway_header() {
    let app = router();
    let ctx = ctx_for(1);
    let _ = send_auth(
        app.clone(),
        "POST",
        "/oagw/v1/upstreams",
        ctx.clone(),
        Some(host_json("api.example.com", 443)),
    )
    .await;
    let (status, headers, json) = send_auth(
        app,
        "POST",
        "/oagw/v1/upstreams",
        ctx,
        Some(host_json("api.example.com", 443)),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(headers.get("x-oagw-error-source").unwrap(), "gateway");
    assert_eq!(
        headers.get("content-type").unwrap(),
        "application/problem+json"
    );
    assert_eq!(json["status"], 409);
    assert!(json["detail"].as_str().unwrap().contains("already in use"));
}

#[tokio::test]
async fn get_missing_upstream_is_404_problem_with_instance() {
    let app = router();
    let ctx = ctx_for(1);
    let (status, headers, json) = send_auth(
        app,
        "GET",
        "/oagw/v1/upstreams/00000000-0000-0000-0000-000000000001",
        ctx,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(headers.get("x-oagw-error-source").unwrap(), "gateway");
    assert_eq!(
        json["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.not_found.v1"
    );
    assert!(
        json["instance"]
            .as_str()
            .unwrap()
            .starts_with("/oagw/v1/upstreams/")
    );
}

#[tokio::test]
async fn list_upstreams_supports_odata_skip_top() {
    let app = router();
    let ctx = ctx_for(1);
    for i in 0..3u64 {
        let _ = send_auth(
            app.clone(),
            "POST",
            "/oagw/v1/upstreams",
            ctx.clone(),
            Some(host_json(&format!("api{i}.example.com"), 443)),
        )
        .await;
    }
    let (status, headers, json) =
        send_auth(app, "GET", "/oagw/v1/upstreams?$top=2&$skip=1", ctx, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get("x-oagw-error-source"), None); // success: no error-source header required
    assert_eq!(json["items"].as_array().unwrap().len(), 2);
    assert_eq!(json["page_info"]["limit"], 2);
}

#[tokio::test]
async fn create_route_then_plugin_delete_in_use_is_409_with_references() {
    let app = router();
    let ctx = ctx_for(1);

    // Create an upstream, a route with that upstream, and a plugin; bind the
    // plugin to both the upstream and the route.
    let (_, _, upstream) = send_auth(
        app.clone(),
        "POST",
        "/oagw/v1/upstreams",
        ctx.clone(),
        Some(host_json("api.example.com", 443)),
    )
    .await;
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();

    let route_body = serde_json::json!({
        "upstream_id": upstream_id,
        "match": { "http": { "methods": ["GET", "POST"], "path": "/v1/chat" } }
    });
    let (status, _, route) = send_auth(
        app.clone(),
        "POST",
        "/oagw/v1/routes",
        ctx.clone(),
        Some(route_body),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let route_id = route["id"].as_str().unwrap().to_owned();

    let plugin_body = serde_json::json!({
        "name": "transform",
        "kind": "starlark",
        "source": "def handle(req):\n  return req"
    });
    let (status, _, plugin) = send_auth(
        app.clone(),
        "POST",
        "/oagw/v1/plugins",
        ctx.clone(),
        Some(plugin_body),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let plugin_id = plugin["id"].as_str().unwrap().to_owned();

    // Bind plugin to upstream + route.
    let upstream_new = serde_json::json!({
        "server": { "endpoints": [{ "scheme": "https", "host": "api.example.com", "port": 443 }] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "plugins": { "items": [plugin_id] }
    });
    let (status, _, _) = send_auth(
        app.clone(),
        "PUT",
        &format!("/oagw/v1/upstreams/{upstream_id}"),
        ctx.clone(),
        Some(upstream_new),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let route_new = serde_json::json!({
        "upstream_id": upstream_id,
        "match": { "http": { "methods": ["GET", "POST"], "path": "/v1/chat" } },
        "plugins": { "items": [plugin_id] }
    });
    let (status, _, _) = send_auth(
        app.clone(),
        "PUT",
        &format!("/oagw/v1/routes/{route_id}"),
        ctx.clone(),
        Some(route_new),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Delete must 409 with the referencing upstream and route.
    let (status, headers, json) = send_auth(
        app,
        "DELETE",
        &format!("/oagw/v1/plugins/{plugin_id}"),
        ctx,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(headers.get("x-oagw-error-source").unwrap(), "gateway");
    assert_eq!(
        json["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1"
    );
    // DESIGN extension fields are snake_case: `referenced_by` holds the
    // referencing upstream/route ids.
    assert!(
        json["referenced_by"]["upstreams"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!(upstream_id))
    );
    assert!(
        json["referenced_by"]["routes"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!(route_id))
    );
}

#[tokio::test]
async fn list_routes_filters_by_odata_filter() {
    let app = router();
    let ctx = ctx_for(1);

    // Two upstreams; routes on each. Filter by `upstream_id` (the DESIGN's
    // documented route filter example) and expect only that upstream's route.
    let (_, _, u1) = send_auth(
        app.clone(),
        "POST",
        "/oagw/v1/upstreams",
        ctx.clone(),
        Some(host_json("api1.example.com", 443)),
    )
    .await;
    let (_, _, u2) = send_auth(
        app.clone(),
        "POST",
        "/oagw/v1/upstreams",
        ctx.clone(),
        Some(host_json("api2.example.com", 443)),
    )
    .await;
    let u1_id = u1["id"].as_str().unwrap().to_owned();
    let u2_id = u2["id"].as_str().unwrap().to_owned();

    for (uid, path) in [(&u1_id, "/v1/a"), (&u2_id, "/v1/b")] {
        let body = serde_json::json!({
            "upstream_id": uid,
            "match": { "http": { "methods": ["GET"], "path": path } }
        });
        let _ = send_auth(
            app.clone(),
            "POST",
            "/oagw/v1/routes",
            ctx.clone(),
            Some(body),
        )
        .await;
    }

    let filter = format!("$filter=upstream_id%20eq%20'{u1_id}'");
    let (status, _, json) =
        send_auth(app, "GET", &format!("/oagw/v1/routes?{filter}"), ctx, None).await;
    assert_eq!(status, StatusCode::OK);
    let items = json["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["upstream_id"], u1_id);
}

#[tokio::test]
async fn proxy_route_is_registered_and_answers_404() {
    let app = router();
    let ctx = ctx_for(1);
    let (status, headers, json) =
        send_auth(app, "GET", "/oagw/v1/proxy/my-alias/some/rest", ctx, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(headers.get("x-oagw-error-source").unwrap(), "gateway");
    assert_eq!(
        json["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
    );
    assert_eq!(json["alias"], "my-alias");
}

// ---------------------------------------------------------------------------
// Rf-012 / Rf-016 regression tests (semantic review)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn put_upstream_on_ip_based_alias_without_alias_is_tolerated() {
    let app = router();
    let ctx = ctx_for(1);
    // An IP-based upstream cannot derive an alias; the create carries one
    // explicitly.
    let create_body = serde_json::json!({
        "alias": "my-ip-api",
        "server": { "endpoints": [{ "scheme": "https", "host": "127.0.0.1", "port": 443 }] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
    });
    let (status, _, created) = send_auth(
        app.clone(),
        "POST",
        "/oagw/v1/upstreams",
        ctx.clone(),
        Some(create_body),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = created["id"].as_str().unwrap().to_owned();
    assert_eq!(created["alias"], "my-ip-api");

    // PUT omitting the alias (still IP-based, so nothing to derive from):
    // the stored alias is carried over instead of failing validation.
    let put_body = serde_json::json!({
        "server": { "endpoints": [{ "scheme": "https", "host": "127.0.0.1", "port": 443 }] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
    });
    let (status, _, json) = send_auth(
        app,
        "PUT",
        &format!("/oagw/v1/upstreams/{id}"),
        ctx,
        Some(put_body),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["alias"], "my-ip-api");
}

#[tokio::test]
async fn proxy_echoes_caller_x_request_id_and_trace_id_on_gateway_error() {
    let app = router();
    let ctx = ctx_for(1);
    let (status, headers, json) = send_auth_headered(
        app,
        "GET",
        "/oagw/v1/proxy/my-alias/some/rest",
        ctx,
        &[("x-request-id", "caller-trace-42")],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // The caller-supplied id is echoed verbatim...
    assert_eq!(headers.get("x-request-id").unwrap(), "caller-trace-42");
    // ...and lands in the RFC 9457 `trace_id` member of gateway problems.
    assert_eq!(json["trace_id"], "caller-trace-42");
    assert_eq!(
        json["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
    );
}

#[tokio::test]
async fn proxy_generates_x_request_id_and_trace_id_for_gateway_errors() {
    let app = router();
    let ctx = ctx_for(1);
    let (status, headers, json) =
        send_auth(app, "GET", "/oagw/v1/proxy/nope/x", ctx, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // No caller id: a fresh correlation id is generated and echoed.
    let rid = headers.get("x-request-id").unwrap().to_str().unwrap();
    assert!(Uuid::parse_str(rid).is_ok(), "generated id must be a UUID");
    assert_eq!(json["trace_id"], rid);
}
