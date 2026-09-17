//! HTTP-level tests for the OAGW control plane (and the minimal data plane).
//!
//! Uses the `Router::oneshot` pattern: the gear's routes are registered into a
//! plain `axum::Router` with a no-op OpenAPI registry and a caller
//! [`SecurityContext`] injected per request, so the assertions cover the wire
//! shapes — status codes, problem documents and header contracts — rather than
//! the services alone.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::doc_markdown)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use httpmock::prelude::MockServer;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationSpec;
use toolkit_security::SecurityContext;

use oagw::config::{OagwConfig, SsrfPolicy};
use oagw::domain::plugin::PluginRegistry;
use oagw::domain::services::{ProxyService, RouteService, UpstreamService};
use oagw::gear::OagwState;
use oagw::infra::plugin::builtin::register_builtins;
use oagw::infra::storage::memory::InMemoryRepositories;

// ── Test scaffolding ────────────────────────────────────────────────────

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

fn config() -> OagwConfig {
    OagwConfig {
        proxy_timeout_secs: 5,
        // The e2e deployment runs with plaintext upstreams allowed; the model
        // must accept `http` endpoints either way.
        allow_http_upstream: true,
        ssrf_policy: SsrfPolicy::default(),
        ..OagwConfig::default()
    }
}

fn state() -> Arc<OagwState> {
    let repos = InMemoryRepositories::new().into_repos();
    let mut plugins = PluginRegistry::new();
    register_builtins(&mut plugins);
    Arc::new(OagwState {
        upstreams: UpstreamService::new(&repos),
        routes: RouteService::new(&repos),
        proxy: ProxyService::try_new(repos, config(), plugins).expect("proxy service"),
        config: Arc::new(config()),
    })
}

/// One router per test: the in-memory repositories live in the state.
fn router() -> Router {
    let openapi = NoopOpenApiRegistry;
    oagw::api::rest::routes::register_routes(Router::new(), &openapi, state())
}

fn make_ctx(tenant_id: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::now_v7())
        .subject_tenant_id(tenant_id)
        .build()
        .expect("valid SecurityContext")
}

fn request(method: &str, uri: &str, body: Option<Value>) -> Request<Body> {
    request_as(Uuid::nil(), method, uri, body)
}

fn request_as(tenant: Uuid, method: &str, uri: &str, body: Option<Value>) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let payload = body
        .map(|value| serde_json::to_vec(&value).expect("serialisable body"))
        .unwrap_or_default();
    let mut req = builder.body(Body::from(payload)).expect("request");
    req.extensions_mut().insert(make_ctx(tenant));
    req
}

async fn call(method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    call_on(router(), Uuid::nil(), method, uri, body).await
}

async fn call_on(
    router: Router,
    tenant: Uuid,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let response = router
        .oneshot(request_as(tenant, method, uri, body))
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).to_string()));
    (status, value)
}

/// `protocol` is required by `docs/schemas/upstream.v1.schema.json`.
const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

const TENANT: Uuid = Uuid::nil();

fn upstream_body(server: &MockServer) -> Value {
    json!({
        "enabled": true,
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [{ "scheme": "http", "host": "127.0.0.1", "port": server.port() }] },
        "alias": "upstream.test",
        "tags": ["e2e"]
    })
}

fn route_body(upstream_id: &str) -> Value {
    json!({
        "upstream_id": upstream_id,
        "match": { "http": { "methods": ["GET"], "path": "/v1" } }
    })
}

// ── Upstream CRUD ───────────────────────────────────────────────────────

#[tokio::test]
async fn upstream_crud_round_trips() {
    let server = MockServer::start();
    let router = router();

    // 201 create; the id is a server-generated UUID.
    let (status, created) = call_on(
        router.clone(),
        TENANT,
        "POST",
        "/oagw/v1/upstreams",
        Some(upstream_body(&server)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "created: {created}");
    let id = created["id"].as_str().expect("id").to_owned();
    assert!(Uuid::parse_str(&id).is_ok(), "server-generated UUID");
    assert_eq!(created["alias"], "upstream.test");
    assert_eq!(created["protocol"], PROTOCOL_HTTP);

    // 200 list, with the OData alias filter.
    let (status, listed) = call_on(router.clone(), TENANT, "GET", "/oagw/v1/upstreams", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed.as_array().map(Vec::len), Some(1));
    let (status, filtered) = call_on(
        router.clone(),
        TENANT,
        "GET",
        "/oagw/v1/upstreams?$filter=alias%20eq%20'upstream.test'",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(filtered.as_array().map(Vec::len), Some(1));
    let (status, empty) = call_on(
        router.clone(),
        TENANT,
        "GET",
        "/oagw/v1/upstreams?$filter=alias%20eq%20'nope.test'",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(empty.as_array().map(Vec::len), Some(0));

    // 200 read, then 404 for an unknown id.
    let (status, read) =
        call_on(router.clone(), TENANT, "GET", &format!("/oagw/v1/upstreams/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(read["id"], created["id"]);
    let (status, missing) = call_on(
        router.clone(),
        TENANT,
        "GET",
        &format!("/oagw/v1/upstreams/{}", Uuid::new_v4()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(missing["status"], 404);

    // 200 full replacement: omitted optional fields are cleared.
    let mut replacement = upstream_body(&server);
    replacement["tags"] = json!(["replaced"]);
    let (status, updated) = call_on(
        router.clone(),
        TENANT,
        "PUT",
        &format!("/oagw/v1/upstreams/{id}"),
        Some(replacement),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "replaced: {updated}");
    assert_eq!(updated["tags"], json!(["replaced"]));

    // 204 delete, then the resource is gone.
    let (status, _) =
        call_on(router.clone(), TENANT, "DELETE", &format!("/oagw/v1/upstreams/{id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call_on(router, TENANT, "GET", &format!("/oagw/v1/upstreams/{id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn http_endpoints_are_accepted_at_create_time() {
    // A plaintext endpoint is a legal roster entry: only the *connection* is
    // gated by `allow_http_upstream`, never the create. A non-standard port is
    // part of the derived alias (`hostname:port`).
    let (status, created) = call(
        "POST",
        "/oagw/v1/upstreams",
        Some(json!({
            "protocol": PROTOCOL_HTTP,
            "server": { "endpoints": [{ "scheme": "http", "host": "svc.internal", "port": 8080 }] }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["alias"], "svc.internal:8080");
    assert_eq!(created["server"]["endpoints"][0]["port"], 8080);
}

#[tokio::test]
async fn invalid_bodies_are_rejected() {
    // Empty endpoint pool → 400.
    let (status, problem) = call(
        "POST",
        "/oagw/v1/upstreams",
        Some(json!({ "protocol": PROTOCOL_HTTP, "server": { "endpoints": [] } })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");

    // Caller-supplied id → 409 immutable field.
    let (status, problem) = call(
        "POST",
        "/oagw/v1/upstreams",
        Some(json!({
            "id": Uuid::new_v4().to_string(),
            "protocol": PROTOCOL_HTTP,
            "server": { "endpoints": [{ "scheme": "https", "host": "a.example" }] }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");

    // Unknown id on PUT → 404.
    let (status, _) = call(
        "PUT",
        &format!("/oagw/v1/upstreams/{}", Uuid::new_v4()),
        Some(json!({
            "protocol": PROTOCOL_HTTP,
            "server": { "endpoints": [{ "scheme": "https", "host": "a.example" }] }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ── Route CRUD ──────────────────────────────────────────────────────────

#[tokio::test]
async fn route_crud_and_duplicate_detection() {
    let server = MockServer::start();
    let router = router();

    let (_, created) = call_on(
        router.clone(),
        TENANT,
        "POST",
        "/oagw/v1/upstreams",
        Some(upstream_body(&server)),
    )
    .await;
    let upstream_id = created["id"].as_str().unwrap().to_owned();

    let (status, route) =
        call_on(router.clone(), TENANT, "POST", "/oagw/v1/routes", Some(route_body(&upstream_id))).await;
    assert_eq!(status, StatusCode::CREATED, "{route}");
    let route_id = route["id"].as_str().unwrap().to_owned();
    assert_eq!(route["upstream_id"], json!(upstream_id));

    // The same (path, methods) triple on one upstream is a 409.
    let (status, problem) =
        call_on(router.clone(), TENANT, "POST", "/oagw/v1/routes", Some(route_body(&upstream_id))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");

    // `upstream_id` is immutable.
    let mut moved = route.clone();
    moved["upstream_id"] = json!(Uuid::new_v4().to_string());
    let (status, problem) = call_on(
        router.clone(),
        TENANT,
        "PUT",
        &format!("/oagw/v1/routes/{route_id}"),
        Some(moved),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");

    // Listing by upstream narrows the collection.
    let (status, listed) = call_on(
        router.clone(),
        TENANT,
        "GET",
        &format!("/oagw/v1/routes?$filter=upstream_id%20eq%20%27{upstream_id}%27"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed.as_array().map(Vec::len), Some(1));

    let (status, _) =
        call_on(router.clone(), TENANT, "DELETE", &format!("/oagw/v1/routes/{route_id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call_on(router, TENANT, "GET", &format!("/oagw/v1/routes/{route_id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ── Data plane ──────────────────────────────────────────────────────────

#[tokio::test]
async fn proxy_forwards_to_the_upstream_and_passes_responses_through() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::prelude::GET).path("/v1/models");
        then.status(200)
            .header("content-type", "application/json")
            .body("{\"object\":\"list\"}");
    });

    let router = router();
    let (_, created) = call_on(
        router.clone(),
        TENANT,
        "POST",
        "/oagw/v1/upstreams",
        Some(upstream_body(&server)),
    )
    .await;
    let upstream_id = created["id"].as_str().unwrap().to_owned();
    let (status, route) =
        call_on(router.clone(), TENANT, "POST", "/oagw/v1/routes", Some(route_body(&upstream_id))).await;
    assert_eq!(status, StatusCode::CREATED, "{route}");

    // The suffix after the alias is appended to the matched route path.
    let response = router
        .oneshot(request("GET", "/oagw/v1/proxy/upstream.test/v1/models", None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some("upstream")
    );
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap()["object"], "list");
}

#[tokio::test]
async fn gateway_failures_render_the_design_problem_document() {
    let server = MockServer::start();
    let router = router();
    let (_, created) = call_on(
        router.clone(),
        TENANT,
        "POST",
        "/oagw/v1/upstreams",
        Some(upstream_body(&server)),
    )
    .await;
    let _upstream_id = created["id"].as_str().unwrap().to_owned();

    // No route registered → 404 problem+json from the gateway.
    let response = router
        .clone()
        .oneshot(request("GET", "/oagw/v1/proxy/upstream.test/v1/nope", None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        response
            .headers()
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some("gateway")
    );
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("application/problem+json")
    );
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let problem: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(problem["status"], 404);
    assert_eq!(problem["type"], "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1");

    // An alias that is not in the roster → 404 from the gateway as well.
    let response = router
        .clone()
        .oneshot(request("GET", "/oagw/v1/proxy/ghost.test/v1/models", None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        response
            .headers()
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some("gateway")
    );

    // OPTIONS preflights are answered permissively without touching the
    // upstream or the tenant context.
    let response = router
        .clone()
        .oneshot(request("OPTIONS", "/oagw/v1/proxy/upstream.test/v1/models", None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("*")
    );
}

#[tokio::test]
async fn management_api_is_tenant_scoped() {
    let server = MockServer::start();
    let router = router();
    let (status, created) = call_on(
        router.clone(),
        TENANT,
        "POST",
        "/oagw/v1/upstreams",
        Some(upstream_body(&server)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = created["id"].as_str().unwrap().to_owned();

    // A different caller tenant sees neither the entry nor the collection.
    let (status, missing) = call_on(
        router.clone(),
        Uuid::now_v7(),
        "GET",
        &format!("/oagw/v1/upstreams/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{missing}");
    let (status, listed) = call_on(router, Uuid::now_v7(), "GET", "/oagw/v1/upstreams", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed.as_array().map(Vec::len), Some(0));
}
