//! Transport-level tests: registered paths, status codes and wire bodies.

use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::response::Response;
use http::{Request, StatusCode};
use serde_json::{Value, json};
use toolkit::api::openapi_registry::OpenApiRegistryImpl;
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

use super::*;
use crate::domain::error::{ErrorKind, OagwError, OagwResult};
use crate::domain::gts_helpers;
use crate::domain::plugin::PluginCatalog;
use crate::domain::services::DataPlaneService;
use crate::domain::services::management::ControlPlaneService;
use crate::infra::plugin::{PluginRegistries, TokenCacheConfig};
use crate::infra::storage::{InMemoryPluginRepo, InMemoryRouteRepo, InMemoryUpstreamRepo};
use crate::infra::tenant_directory::FlatTenantDirectory;

/// Data plane double: echoes the resolution inputs so transport-level routing
/// can be asserted without an upstream.
struct EchoDataPlane;

#[async_trait]
impl DataPlaneService for EchoDataPlane {
    async fn execute_proxy(
        &self,
        _ctx: &SecurityContext,
        alias: &str,
        path_suffix: &str,
        request: Request<Body>,
    ) -> OagwResult<Response> {
        if alias == "boom" {
            return Err(OagwError::new(ErrorKind::RouteNotFound, "nothing here"));
        }
        let payload = json!({
            "alias": alias,
            "path_suffix": path_suffix,
            "method": request.method().as_str(),
            "query": request.uri().query().unwrap_or_default(),
        });
        Ok(Response::builder()
            .status(StatusCode::OK)
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(payload.to_string()))
            .expect("response"))
    }
}

fn tenant() -> Uuid {
    Uuid::parse_str("00000000-df51-5b42-9538-d2b56b7ee953").expect("uuid")
}

fn build_router() -> Router {
    let credstore: Arc<dyn credstore_sdk::CredStoreClientV1> =
        Arc::new(credstore_sdk::test_util::MockCredStoreClient::empty());
    let registries = Arc::new(PluginRegistries::with_builtins(
        credstore,
        TokenCacheConfig::default(),
    ));
    let catalog: Arc<dyn PluginCatalog> = registries;
    let control_plane = Arc::new(ControlPlaneService::new(
        Arc::new(InMemoryUpstreamRepo::new()),
        Arc::new(InMemoryRouteRepo::new()),
        Arc::new(InMemoryPluginRepo::new()),
        Arc::new(FlatTenantDirectory),
        catalog,
    ));
    let state = Arc::new(OagwState {
        control_plane,
        data_plane: Arc::new(EchoDataPlane),
        limiter: Arc::new(crate::infra::ratelimit::RateLimiterRegistry::new()),
    });

    let openapi = OpenApiRegistryImpl::new();
    let router = register_routes(Router::new(), &openapi, state);
    // The api-gateway's auth middleware supplies this in production.
    let ctx = SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(tenant())
        .build()
        .expect("security context");
    router.layer(axum::Extension(ctx))
}

async fn call(router: &Router, request: Request<Body>) -> (StatusCode, http::HeaderMap, Value) {
    let response = router
        .clone()
        .oneshot(request)
        .await
        .expect("router response");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("body");
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or_else(|_| {
            Value::String(String::from_utf8_lossy(&bytes).into_owned())
        })
    };
    (status, headers, body)
}

fn post(path: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("request")
}

fn get(path: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(path)
        .body(Body::empty())
        .expect("request")
}

fn upstream_body(host: &str) -> Value {
    json!({
        "server": {"endpoints": [{"scheme": "http", "host": host, "port": 80}]},
        "protocol": gts_helpers::PROTOCOL_HTTP,
    })
}

#[tokio::test]
async fn the_management_surface_is_gear_relative() {
    let router = build_router();

    // The documented absolute paths belong to a gateway with prefix `/api`;
    // the gear itself must not repeat the prefix.
    let (status, _, _) = call(&router, get("/api/oagw/v1/upstreams")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _, body) = call(&router, get("/oagw/v1/upstreams")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["items"].is_array());
}

#[tokio::test]
async fn upstream_crud_round_trips_over_the_wire() {
    let router = build_router();

    let (status, headers, created) =
        call(&router, post("/oagw/v1/upstreams", upstream_body("api.example.com"))).await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(headers.contains_key(http::header::LOCATION));
    assert_eq!(created["alias"], "api.example.com");
    let id = created["id"].as_str().expect("id").to_owned();

    // Both the bare UUID and the anonymous GTS identifier address the resource.
    for path_id in [
        id.clone(),
        format!("{}{id}", gts_helpers::UPSTREAM_TYPE),
    ] {
        let (status, _, body) =
            call(&router, get(&format!("/oagw/v1/upstreams/{path_id}"))).await;
        assert_eq!(status, StatusCode::OK, "{path_id}");
        assert_eq!(body["id"], id);
    }

    // The GET body is accepted verbatim by PUT.
    let put = Request::builder()
        .method("PUT")
        .uri(format!("/oagw/v1/upstreams/{id}"))
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(created.to_string()))
        .expect("request");
    let (status, _, _) = call(&router, put).await;
    assert_eq!(status, StatusCode::OK, "a GET body must round-trip through PUT");

    let delete = Request::builder()
        .method("DELETE")
        .uri(format!("/oagw/v1/upstreams/{id}"))
        .body(Body::empty())
        .expect("request");
    let (status, _, _) = call(&router, delete).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _, _) = call(&router, get(&format!("/oagw/v1/upstreams/{id}"))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn validation_failures_are_rfc_9457_problem_documents() {
    let router = build_router();
    let (status, headers, body) = call(
        &router,
        post("/oagw/v1/upstreams", json!({"protocol": gts_helpers::PROTOCOL_HTTP})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        headers
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/problem+json")
    );
    assert_eq!(
        headers
            .get(crate::util::ERROR_SOURCE_HEADER)
            .and_then(|v| v.to_str().ok()),
        Some("gateway")
    );
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
    assert_eq!(body["status"], 400);
    assert_eq!(body["instance"], "/oagw/v1/upstreams");
}

#[tokio::test]
async fn unknown_members_are_rejected() {
    let router = build_router();
    let mut body = upstream_body("api.example.com");
    body["surprise"] = json!(true);
    let (status, _, _) = call(&router, post("/oagw/v1/upstreams", body)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn routes_and_plugins_are_registered() {
    let router = build_router();
    let (_, _, upstream) =
        call(&router, post("/oagw/v1/upstreams", upstream_body("api.example.com"))).await;
    let upstream_id = upstream["id"].as_str().expect("id");

    let (status, _, route) = call(
        &router,
        post(
            "/oagw/v1/routes",
            json!({
                "upstream_id": upstream_id,
                "match": {"http": {"methods": ["GET"], "path": "/v1"}},
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(route["match_type"], "http");
    assert_eq!(route["upstream_id"], upstream_id);

    let (status, _, listed) = call(&router, get("/oagw/v1/routes")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["items"].as_array().expect("items").len(), 1);

    let (status, _, plugin) = call(
        &router,
        post(
            "/oagw/v1/plugins",
            json!({
                "name": "validator",
                "plugin_type": "guard",
                "source_code": "def on_request(ctx):\n    return ctx.next()\n",
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let plugin_id = plugin["id"].as_str().expect("gts id").to_owned();
    assert!(plugin_id.starts_with("gts.cf.core.oagw.guard_plugin.v1~"));
    // Source code is never echoed in the resource projection.
    assert!(plugin.get("source_code").is_none());

    let (status, headers, body) = call(
        &router,
        get(&format!("/oagw/v1/plugins/{plugin_id}/source")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        headers
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.starts_with("text/plain"))
    );
    assert_eq!(body, json!("def on_request(ctx):\n    return ctx.next()\n"));
}

#[tokio::test]
async fn plugins_have_no_replace_verb() {
    let router = build_router();
    let (_, _, plugin) = call(
        &router,
        post(
            "/oagw/v1/plugins",
            json!({"name": "p", "plugin_type": "transform", "source_code": "x"}),
        ),
    )
    .await;
    let id = plugin["id"].as_str().expect("id");
    let put = Request::builder()
        .method("PUT")
        .uri(format!("/oagw/v1/plugins/{id}"))
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .expect("request");
    let (status, _, _) = call(&router, put).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn list_endpoints_apply_the_odata_members() {
    let router = build_router();
    for host in ["a.example.com", "b.example.com", "c.example.com"] {
        call(&router, post("/oagw/v1/upstreams", upstream_body(host))).await;
    }

    let (_, _, body) = call(&router, get("/oagw/v1/upstreams?$top=2")).await;
    assert_eq!(body["items"].as_array().expect("items").len(), 2);

    let (_, _, body) = call(
        &router,
        get("/oagw/v1/upstreams?$filter=alias%20eq%20%27b.example.com%27"),
    )
    .await;
    let items = body["items"].as_array().expect("items");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["alias"], "b.example.com");

    let (_, _, body) = call(&router, get("/oagw/v1/upstreams?$select=alias")).await;
    let first = body["items"][0].as_object().expect("object");
    assert_eq!(first.len(), 1);
    assert!(first.contains_key("alias"));

    let (status, _, _) = call(&router, get("/oagw/v1/upstreams?$top=nope")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn the_proxy_route_accepts_every_method_and_both_shapes() {
    let router = build_router();

    let (status, _, body) = call(&router, get("/oagw/v1/proxy/mock")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["alias"], "mock");
    assert_eq!(body["path_suffix"], "");

    for method in ["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD"] {
        let request = Request::builder()
            .method(method)
            .uri("/oagw/v1/proxy/mock/v1/chat/completions?a=1")
            .body(Body::empty())
            .expect("request");
        let response = router.clone().oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK, "{method}");
    }

    let (status, _, body) =
        call(&router, get("/oagw/v1/proxy/mock/v1/chat/completions?a=1")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["path_suffix"], "/v1/chat/completions");
    assert_eq!(body["query"], "a=1");
}

#[tokio::test]
async fn a_data_plane_error_is_rendered_as_problem_json() {
    let router = build_router();
    let (status, headers, body) = call(&router, get("/oagw/v1/proxy/boom/x")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        headers
            .get(crate::util::ERROR_SOURCE_HEADER)
            .and_then(|v| v.to_str().ok()),
        Some("gateway")
    );
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
    );
    assert_eq!(body["instance"], "/oagw/v1/proxy/boom/x");
}

#[tokio::test]
async fn cors_preflight_is_answered_before_upstream_resolution() {
    let router = build_router();
    // `boom` would fail resolution — the preflight must never get that far.
    let request = Request::builder()
        .method("OPTIONS")
        .uri("/oagw/v1/proxy/boom/users")
        .header(http::header::ORIGIN, "https://app.example.com")
        .header(http::header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
        .header(http::header::ACCESS_CONTROL_REQUEST_HEADERS, "Content-Type")
        .body(Body::empty())
        .expect("request");
    let response = router.oneshot(request).await.expect("response");

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let headers = response.headers();
    assert_eq!(
        headers
            .get(http::header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .and_then(|v| v.to_str().ok()),
        Some("https://app.example.com")
    );
    assert_eq!(
        headers
            .get(http::header::ACCESS_CONTROL_ALLOW_METHODS)
            .and_then(|v| v.to_str().ok()),
        Some("POST")
    );
    assert_eq!(
        headers
            .get(http::header::ACCESS_CONTROL_MAX_AGE)
            .and_then(|v| v.to_str().ok()),
        Some("86400")
    );
    assert!(headers.contains_key(http::header::VARY));
}

#[tokio::test]
async fn every_documented_endpoint_is_registered() {
    let router = build_router();
    let cases: &[(&str, &str, StatusCode)] = &[
        ("GET", "/oagw/v1/upstreams", StatusCode::OK),
        ("GET", "/oagw/v1/routes", StatusCode::OK),
        ("GET", "/oagw/v1/plugins", StatusCode::OK),
        (
            "GET",
            "/oagw/v1/upstreams/00000000-0000-0000-0000-000000000001",
            StatusCode::NOT_FOUND,
        ),
        (
            "GET",
            "/oagw/v1/routes/00000000-0000-0000-0000-000000000001",
            StatusCode::NOT_FOUND,
        ),
        (
            "GET",
            "/oagw/v1/plugins/00000000-0000-0000-0000-000000000001",
            StatusCode::NOT_FOUND,
        ),
        (
            "GET",
            "/oagw/v1/plugins/00000000-0000-0000-0000-000000000001/source",
            StatusCode::NOT_FOUND,
        ),
        ("GET", "/oagw/v1/proxy/mock", StatusCode::OK),
        ("GET", "/oagw/v1/proxy/mock/deep/path", StatusCode::OK),
    ];
    for (method, path, expected) in cases {
        let request = Request::builder()
            .method(*method)
            .uri(*path)
            .body(Body::empty())
            .expect("request");
        let response = router.clone().oneshot(request).await.expect("response");
        assert_eq!(response.status(), *expected, "{method} {path}");
    }
}
