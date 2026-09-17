//! Router-level integration test for the `oagw` gear.
//!
//! The gear is initialized the way the host server does it — through a
//! `GearCtx` whose client hub carries the three hard dependencies — and the
//! routes it registers are then driven end to end: an upstream and a route are
//! created over the management API and a request is proxied through the same
//! router to a live echo upstream.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

use oagw::OagwGear;
use oagw::api::rest::handlers::proxy::PROXY_PREFIX;
use oagw::config::OagwConfig;
use toolkit::Gear;
use toolkit::GearCtx;
use toolkit::RestApiCapability;
use toolkit::config::ConfigProvider;
use toolkit_security::SecurityContext;

/// The tenant every request in this module is issued from.
static TENANT: Uuid = Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_00ca);

/// A config provider carrying the `oagw` section of `config/e2e-local.yaml`.
struct GearConfig {
    oagw: Value,
}

impl ConfigProvider for GearConfig {
    fn get_gear_config(&self, gear_name: &str) -> Option<&Value> {
        (gear_name == "oagw").then_some(&self.oagw)
    }
}

/// A `GearCtx` wired like the host server: every hard dependency resolvable
/// and the e2e config in place.
fn ctx() -> GearCtx {
    let hub = Arc::new(toolkit::ClientHub::new());
    let credentials: Arc<dyn credstore_sdk::CredStoreClientV1> =
        Arc::new(credstore_sdk::test_util::MockCredStoreClient::empty());
    hub.register::<dyn credstore_sdk::CredStoreClientV1>(credentials);
    let tenants = tenant_resolver();
    hub.register::<dyn tenant_resolver_sdk::TenantResolverClient>(Arc::new(tenants));
    let registry = types_registry();
    hub.register::<dyn types_registry_sdk::TypesRegistryClient>(Arc::new(registry));
    GearCtx::new(
        "oagw",
        Uuid::new_v4(),
        Arc::new(GearConfig {
            oagw: json!({ "config": {
                "proxy_timeout_secs": 2,
                "allow_http_upstream": true,
                "ssrf_policy": { "enabled": false },
            }}),
        }),
        hub,
        tokio_util::sync::CancellationToken::new(),
    )
}

/// A tenant resolver over an empty hub: the data plane never calls it, but the
/// gear refuses to initialize without the dependency.
fn tenant_resolver() -> tenant_resolver::domain::local_client::TenantResolverLocalClient {
    let service = Arc::new(tenant_resolver::domain::Service::new(
        Arc::new(toolkit::ClientHub::new()),
        String::from("constructorfabric"),
    ));
    tenant_resolver::domain::local_client::TenantResolverLocalClient::new(service)
}

/// A real types-registry over a fresh in-memory store.
fn types_registry() -> types_registry::domain::local_client::TypesRegistryLocalClient {
    let config = types_registry::config::TypesRegistryConfig::default();
    let repo = Arc::new(types_registry::infra::InMemoryGtsRepository::new(
        config.to_gts_config(),
    ));
    let service = Arc::new(types_registry::domain::TypesRegistryService::new(
        repo, config,
    ));
    types_registry::domain::local_client::TypesRegistryLocalClient::new(service)
}

/// Echo upstream: answers every request with the method, path and body it got.
async fn echo(State(()): State<()>, request: Request<Body>) -> Response {
    let (parts, body) = request.into_parts();
    let payload = axum::body::to_bytes(body, 1 << 20)
        .await
        .unwrap_or_default();
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        json!({
            "method": parts.method.as_str(),
            "path": parts.uri.path(),
            "body": String::from_utf8_lossy(&payload),
        })
        .to_string(),
    )
        .into_response()
}

/// Bind `router` to an ephemeral loopback port and return the port.
async fn spawn(router: Router) -> u16 {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap_or_else(|error| panic!("bind: {error}"));
    let port = listener
        .local_addr()
        .unwrap_or_else(|error| panic!("local addr: {error}"))
        .port();
    tokio::spawn(async move {
        drop(axum::serve(listener, router).await);
    });
    port
}

/// An upstream spec pointing at the echo server's `/echo` route.
fn upstream_spec(port: u16) -> Value {
    json!({
        "alias": "echo",
        "description": "echo",
        "server": { "endpoints": [{ "scheme": "http", "host": "127.0.0.1", "port": port }] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
    })
}

/// Send a JSON body to the router and return status plus the decoded body.
async fn call(
    router: Router,
    method: &str,
    path: &str,
    payload: Option<Value>,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(
            payload.map_or_else(String::new, |payload| payload.to_string()),
        ))
        .unwrap_or_else(|error| panic!("request: {error}"));
    let response = router
        .oneshot(request)
        .await
        .unwrap_or_else(|error| panic!("call: {error}"));
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap_or_default();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|error| panic!("body {error}: {}", String::from_utf8_lossy(&bytes)))
    };
    (status, body)
}

/// An upstream the gear knows about, registered over the management API.
async fn register_upstream(router: Router, port: u16) -> (Router, Value) {
    let (status, created) = call(
        router.clone(),
        "POST",
        "/oagw/v1/upstreams",
        Some(upstream_spec(port)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "upstream creation: {created}");
    (router, created)
}

#[tokio::test]
async fn the_gear_serves_the_management_api_and_the_data_plane() {
    let ctx = ctx();
    let gear = OagwGear::default();
    gear.init(&ctx)
        .await
        .unwrap_or_else(|error| panic!("gear init: {error}"));

    let openapi = toolkit::api::OpenApiRegistryImpl::new();
    let router = gear
        .register_rest(&ctx, Router::new(), &openapi)
        .unwrap_or_else(|error| panic!("route registration: {error}"))
        .layer(axum::Extension(security_context()));

    let upstream = spawn(axum::Router::new().route("/echo", any(echo))).await;
    let (router, created) = register_upstream(router, upstream).await;

    // The upstream is addressable under its tenant-scoped id.
    let id = created["id"].as_str().unwrap_or_default().to_owned();
    let (status, fetched) = call(
        router.clone(),
        "GET",
        &format!("/oagw/v1/upstreams/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fetched["alias"], "echo");

    // A route narrows the data plane to one path prefix.
    let (status, route) = call(
        router.clone(),
        "POST",
        "/oagw/v1/routes",
        Some(json!({
            "upstream_id": id,
            "priority": 10,
            "match": { "http": { "path": "/echo", "methods": ["GET", "POST"] } }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "route creation: {route}");

    // ... and the request is proxied through the same router.
    let request = Request::builder()
        .method("POST")
        .uri(format!("{PROXY_PREFIX}echo/echo"))
        .header("content-type", "text/plain")
        .body(Body::from(String::from("payload")))
        .unwrap_or_else(|error| panic!("proxy request: {error}"));
    let response = router
        .oneshot(request)
        .await
        .unwrap_or_else(|error| panic!("proxy call: {error}"));
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-oagw-error-source")
            .and_then(|value| value.to_str().ok()),
        Some("upstream")
    );
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap_or_default();
    let echoed: Value =
        serde_json::from_slice(&body).unwrap_or_else(|error| panic!("echo body: {error}"));
    assert_eq!(echoed["method"], "POST");
    assert_eq!(echoed["body"], "payload");
}

/// The identity every proxied request carries.
fn security_context() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::from_u128(1))
        .subject_tenant_id(TENANT)
        .build()
        .unwrap_or_else(|error| panic!("security context: {error}"))
}

#[tokio::test]
async fn an_unknown_alias_is_a_gateway_problem_document() {
    let ctx = ctx();
    let gear = OagwGear::default();
    gear.init(&ctx)
        .await
        .unwrap_or_else(|error| panic!("gear init: {error}"));
    let openapi = toolkit::api::OpenApiRegistryImpl::new();
    let router = gear
        .register_rest(&ctx, Router::new(), &openapi)
        .unwrap_or_else(|error| panic!("route registration: {error}"))
        .layer(axum::Extension(security_context()));

    let request = Request::builder()
        .method("GET")
        .uri(format!("{PROXY_PREFIX}nope/echo"))
        .body(Body::empty())
        .unwrap_or_else(|error| panic!("proxy request: {error}"));
    let response = router
        .oneshot(request)
        .await
        .unwrap_or_else(|error| panic!("proxy call: {error}"));
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        response
            .headers()
            .get("x-oagw-error-source")
            .and_then(|value| value.to_str().ok()),
        Some("gateway")
    );
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap_or_default();
    let problem: Value =
        serde_json::from_slice(&body).unwrap_or_else(|error| panic!("problem: {error}"));
    assert_eq!(problem["context"]["alias"], "nope");
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.resource.not_found.v1"
    );
}

#[tokio::test]
async fn the_effective_config_arrives_through_the_gear_context() {
    let config: OagwConfig = serde_json::from_value(json!({
        "proxy_timeout_secs": 7,
        "allow_http_upstream": true,
        "ssrf_policy": { "enabled": false }
    }))
    .unwrap_or_else(|error| panic!("config: {error}"));
    assert_eq!(config.proxy_timeout_secs, 7);
    assert!(config.allow_http_upstream);
}
