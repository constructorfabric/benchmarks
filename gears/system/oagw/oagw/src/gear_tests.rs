// Created: 2026-09-04 by Constructor Tech
//! Tests of the gear wiring: `Gear::init` and the gear-relative
//! `RestApiCapability::register_rest`.
//!
//! The control plane and the data plane are exercised through the router the
//! gear mounts, with the `SecurityContext` the gateway middleware injects; one
//! test drives a real proxied request into a throwaway in-process upstream.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use toolkit::Gear;
use toolkit::GearCtx;
use toolkit::RestApiCapability;
use toolkit::api::{OpenApiInfo, OpenApiRegistryImpl};
use toolkit::config::ConfigProvider;
use tower::ServiceExt;
use uuid::Uuid;

use super::{OagwGear, validate_config};
use crate::config::{DEFAULT_MAX_BODY_BYTES, OagwConfig, SsrfPolicy};
use crate::dataplane::proxy::{PROXY_ALIAS_PREFIX, PROXY_ROUTE_PATH};
use crate::error::{ERROR_SOURCE_GATEWAY, ERROR_SOURCE_HEADER};

/// Fixed tenant of every request in this module.
fn tenant_id() -> Uuid {
    Uuid::from_u128(0x0A6D)
}

/// A `SecurityContext` bound to [`tenant_id`], as the auth middleware would
/// inject it.
fn context() -> toolkit_security::SecurityContext {
    toolkit_security::SecurityContext::builder()
        .subject_id(Uuid::from_u128(0x5EB))
        .subject_tenant_id(tenant_id())
        .build()
        .unwrap()
}

/// A `ConfigProvider` serving the `gears.<name>` sections of a configuration.
struct GearConfigs(serde_json::Value);

impl ConfigProvider for GearConfigs {
    fn get_gear_config(&self, gear_name: &str) -> Option<&serde_json::Value> {
        self.0.get(gear_name)
    }
}

/// A `GearCtx` for the `oagw` gear over the `gears` sections of `gears`.
fn gear_ctx(gears: Value) -> GearCtx {
    GearCtx::new(
        OagwGear::MODULE_NAME,
        Uuid::new_v4(),
        Arc::new(GearConfigs(gears)),
        Arc::new(toolkit::ClientHub::new()),
        Default::default(),
    )
}

/// The `gears.oagw.config` block of `config/e2e-local.yaml`.
fn e2e_gears() -> Value {
    json!({
        "oagw": {
            "config": {
                "proxy_timeout_secs": 2,
                "allow_http_upstream": true,
                "ssrf_policy": { "enabled": false },
            },
        },
    })
}

/// An initialized gear over `gears`, with the context that initialized it.
async fn wired(gears: Value) -> (GearCtx, OagwGear) {
    let ctx = gear_ctx(gears);
    let gear = OagwGear::default();
    gear.init(&ctx).await.unwrap();
    (ctx, gear)
}

/// The router the gear mounts, with the registry it published its operations
/// on.
fn gear_router(ctx: &GearCtx, gear: &OagwGear) -> (Router, OpenApiRegistryImpl) {
    let openapi = OpenApiRegistryImpl::new();
    let router = gear.register_rest(ctx, Router::new(), &openapi).unwrap();
    (router, openapi)
}

/// Sends a request carrying the tenant's `SecurityContext`.
async fn call(
    router: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> axum::response::Response {
    let mut builder = Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let raw = match body {
        Some(value) => serde_json::to_vec(&value).unwrap(),
        None => Vec::new(),
    };
    let mut request = builder.body(Body::from(raw)).unwrap();
    request.extensions_mut().insert(context());
    router.clone().oneshot(request).await.unwrap()
}

/// The whole body of a response, as JSON.
async fn body_json(response: &mut axum::response::Response) -> Value {
    let bytes = to_bytes(
        std::mem::replace(response.body_mut(), Body::empty()),
        1 << 20,
    )
    .await
    .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

/// The whole body of a response, as bytes.
async fn body_bytes(response: &mut axum::response::Response) -> Vec<u8> {
    to_bytes(
        std::mem::replace(response.body_mut(), Body::empty()),
        1 << 20,
    )
    .await
    .unwrap()
    .to_vec()
}

/// Asserts `response` is an `application/problem+json` body carrying `status`
/// and `code`, and returns it.
///
/// The management surface re-exposes `error_code` under `code`; the data plane
/// only sets `error_code`, so both spellings are accepted here.
async fn assert_problem(response: &mut axum::response::Response, status: u16, code: &str) -> Value {
    assert_eq!(response.status().as_u16(), status);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/problem+json"
    );
    let body = body_json(response).await;
    assert_eq!(body["status"], status, "unexpected problem body: {body}");
    let reported = body["code"]
        .as_str()
        .or_else(|| body["error_code"].as_str())
        .unwrap_or_default();
    assert_eq!(reported, code, "unexpected problem body: {body}");
    body
}

/// An upstream body for a hostname endpoint: the pool auto-derives its alias
/// (`docs/PRD.md` §5.5), so the body carries no `alias`.
fn hostname_body(host: &str) -> Value {
    json!({
        "protocol": "http",
        "server": { "endpoints": [ { "scheme": "http", "host": host } ] },
    })
}

/// An upstream body for an `http` endpoint, with the explicit alias an
/// IP-based pool requires (`docs/PRD.md` §5.5).
fn upstream_body(alias: Option<&str>, host: &str, port: u16) -> Value {
    let mut body = json!({
        "protocol": "http",
        "server": {
            "endpoints": [ { "scheme": "http", "host": host, "port": port } ],
        },
    });
    if let Some(alias) = alias {
        body["alias"] = Value::String(String::from(alias));
    }
    body
}

/// Creates an upstream through the mounted router.
///
/// Returns the status, the `Location` header and the body.
async fn create_upstream(router: &Router, body: Value) -> (StatusCode, String, Value) {
    let mut response = call(router, "POST", "/oagw/v1/upstreams", Some(body)).await;
    let status = response.status();
    let location = response
        .headers()
        .get("location")
        .map(|value| value.to_str().unwrap().to_owned())
        .unwrap_or_default();
    (status, location, body_json(&mut response).await)
}

/// Creates one upstream and one route, returning the upstream id.
async fn seed_route(
    router: &Router,
    alias: Option<&str>,
    host: &str,
    port: u16,
    path: &str,
) -> Uuid {
    let (status, location, body) = create_upstream(router, upstream_body(alias, host, port)).await;
    assert_eq!(status, StatusCode::CREATED, "upstream rejected: {body}");
    let id = location.rsplit('/').next().unwrap();
    let upstream_id = Uuid::parse_str(id).unwrap();

    let response = call(
        router,
        "POST",
        "/oagw/v1/routes",
        Some(json!({
            "upstream_id": upstream_id,
            "match": { "http": { "methods": ["GET"], "path": path } },
        })),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    upstream_id
}

// ------------------------------------------------------------------- identity

#[test]
fn the_gear_is_declared_under_the_oagw_config_key() {
    assert_eq!(OagwGear::MODULE_NAME, "oagw");
}

#[test]
fn the_proxy_route_is_gear_relative() {
    assert_eq!(PROXY_ROUTE_PATH, "/oagw/v1/proxy/{*alias}");
    assert!(!PROXY_ROUTE_PATH.starts_with("/api"));
    assert!(PROXY_ALIAS_PREFIX.starts_with("/oagw/v1/proxy/"));
}

// ----------------------------------------------------------------------- init

#[tokio::test]
async fn init_loads_the_gear_configuration_of_the_host() {
    let (_ctx, gear) = wired(e2e_gears()).await;

    let config = gear.config().unwrap();
    assert_eq!(config.proxy_timeout_secs, 2);
    assert!(config.allow_http_upstream);
    assert!(!config.ssrf_enabled());
    assert_eq!(config.max_body_bytes, DEFAULT_MAX_BODY_BYTES);
    assert!(gear.data_plane().is_some());
}

#[tokio::test]
async fn init_falls_back_to_the_documented_defaults() {
    let (_ctx, gear) = wired(json!({})).await;

    assert_eq!(*gear.config().unwrap(), OagwConfig::default());
    let plane = gear.data_plane().unwrap();
    assert!(!plane.is_cancelled());
    assert_eq!(plane.config().proxy_timeout_secs, 30);
}

#[test]
fn init_reads_the_documented_defaults() {
    let defaults = OagwConfig::default();
    assert_eq!(defaults.proxy_timeout_secs, 30);
    assert!(!defaults.allow_http_upstream);
    assert_eq!(defaults.ssrf_policy, SsrfPolicy { enabled: false });
    assert_eq!(defaults.max_body_bytes, DEFAULT_MAX_BODY_BYTES);
}

#[tokio::test]
async fn init_rejects_a_zero_proxy_timeout_and_wires_nothing() {
    let gear = OagwGear::default();
    let ctx = gear_ctx(json!({ "oagw": { "config": { "proxy_timeout_secs": 0 } } }));

    let error = gear.init(&ctx).await.unwrap_err().to_string();
    assert!(error.contains("proxy_timeout_secs"), "unexpected: {error}");
    assert!(gear.config().is_none());
    assert!(gear.data_plane().is_none());
}

#[tokio::test]
async fn init_rejects_a_zero_body_limit() {
    let gear = OagwGear::default();
    let ctx = gear_ctx(json!({ "oagw": { "config": { "max_body_bytes": 0 } } }));

    let error = gear.init(&ctx).await.unwrap_err().to_string();
    assert!(error.contains("max_body_bytes"), "unexpected: {error}");
    assert!(gear.data_plane().is_none());
}

#[tokio::test]
async fn init_rejects_an_unparsable_config_section() {
    let gear = OagwGear::default();
    let ctx = gear_ctx(json!({ "oagw": { "config": { "proxy_timeout_secs": "two" } } }));

    let error = gear.init(&ctx).await.unwrap_err().to_string();
    assert!(
        error.contains("gears.oagw.config"),
        "the error must name the configuration key: {error}"
    );
    assert!(gear.data_plane().is_none());
}

#[test]
fn validate_config_accepts_the_e2e_values() {
    let config = OagwConfig {
        proxy_timeout_secs: 2,
        allow_http_upstream: true,
        ssrf_policy: SsrfPolicy { enabled: false },
        max_body_bytes: DEFAULT_MAX_BODY_BYTES,
    };
    assert!(validate_config(&config).is_ok());
}

#[tokio::test]
async fn a_duplicate_init_keeps_the_installed_wiring() {
    let (ctx, gear) = wired(e2e_gears()).await;
    let first = gear.data_plane().unwrap();

    gear.init(&ctx).await.unwrap();

    assert!(Arc::ptr_eq(&first, &gear.data_plane().unwrap()));
}

#[tokio::test]
async fn the_data_plane_reports_the_host_shutdown() {
    let (ctx, gear) = wired(e2e_gears()).await;
    let plane = gear.data_plane().unwrap();
    assert!(!plane.is_cancelled());

    ctx.cancellation_token().cancel();

    assert!(plane.is_cancelled());
}

// -------------------------------------------------------------- register_rest

#[tokio::test]
async fn register_rest_before_init_is_an_error() {
    let gear = OagwGear::default();
    let ctx = gear_ctx(json!({}));

    let error = gear
        .register_rest(&ctx, Router::new(), &OpenApiRegistryImpl::new())
        .unwrap_err()
        .to_string();

    assert!(error.contains("not initialized"), "unexpected: {error}");
}

#[tokio::test]
async fn register_rest_publishes_the_management_operations() {
    let (ctx, gear) = wired(e2e_gears()).await;
    let (_router, openapi) = gear_router(&ctx, &gear);

    let doc = openapi.build_openapi(&OpenApiInfo::default()).unwrap();
    let paths: Vec<&str> = doc.paths.paths.keys().map(String::as_str).collect();
    for expected in [
        "/oagw/v1/upstreams",
        "/oagw/v1/upstreams/{id}",
        "/oagw/v1/routes",
        "/oagw/v1/plugins",
    ] {
        assert!(paths.contains(&expected), "missing {expected} in {paths:?}");
    }
}

#[tokio::test]
async fn register_rest_mounts_the_management_surface() {
    let (ctx, gear) = wired(e2e_gears()).await;
    let (router, _openapi) = gear_router(&ctx, &gear);

    // An empty list on a fresh control plane.
    let mut response = call(&router, "GET", "/oagw/v1/upstreams", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(&mut response).await, json!([]));

    // Create: 201 with a Location header. A hostname pool auto-derives its
    // alias, so the body carries no `alias`.
    let (status, location, body) = create_upstream(&router, hostname_body("api.example.com")).await;
    assert_eq!(status, StatusCode::CREATED, "unexpected body: {body}");
    assert!(location.starts_with("/oagw/v1/upstreams/"), "{location}");
    assert_eq!(body["alias"], "api.example.com");

    // The list holds exactly the created upstream.
    let mut response = call(&router, "GET", "/oagw/v1/upstreams", None).await;
    let listed = body_json(&mut response).await;
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(listed[0]["alias"], "api.example.com");

    // Read by id, then the unknown id.
    let id = Uuid::parse_str(location.rsplit('/').next().unwrap()).unwrap();
    let mut response = call(&router, "GET", &format!("/oagw/v1/upstreams/{id}"), None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(&mut response).await["id"], body["id"]);

    let unknown = format!("/oagw/v1/upstreams/{}", Uuid::new_v4());
    let mut response = call(&router, "GET", &unknown, None).await;
    assert_problem(&mut response, 404, "RouteNotFound").await;

    // The alias is unique per tenant.
    let (status, _location, body) =
        create_upstream(&router, hostname_body("api.example.com")).await;
    assert_eq!(status, StatusCode::CONFLICT, "unexpected body: {body}");
}

#[tokio::test]
async fn register_rest_accepts_an_http_scheme_endpoint() {
    let (ctx, gear) = wired(e2e_gears()).await;
    let (router, _openapi) = gear_router(&ctx, &gear);

    // `http` is a legal endpoint scheme: `allow_http_upstream` only gates the
    // plaintext egress the data plane makes.
    let (status, _location, body) = create_upstream(
        &router,
        upstream_body(Some("mock.internal"), "127.0.0.1", 18099),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "unexpected body: {body}");
    assert_eq!(body["server"]["endpoints"][0]["scheme"], "http");
}

#[tokio::test]
async fn register_rest_mounts_the_route_and_plugin_surfaces() {
    let (ctx, gear) = wired(e2e_gears()).await;
    let (router, _openapi) = gear_router(&ctx, &gear);
    seed_route(&router, Some("mock.internal"), "127.0.0.1", 18099, "/v1").await;

    let mut response = call(&router, "GET", "/oagw/v1/routes", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let routes = body_json(&mut response).await;
    assert_eq!(routes.as_array().unwrap().len(), 1);
    assert_eq!(routes[0]["match"]["http"]["path"], "/v1");

    let mut response = call(&router, "GET", "/oagw/v1/plugins", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let plugins = body_json(&mut response).await;
    assert!(
        plugins.as_array().unwrap().len() >= 3,
        "the built-in catalog is served: {plugins}"
    );
}

#[tokio::test]
async fn register_rest_mounts_the_proxy_surface() {
    let (ctx, gear) = wired(e2e_gears()).await;
    let (router, _openapi) = gear_router(&ctx, &gear);

    // An unknown alias is a *gateway* problem of the proxy surface: the route
    // answers instead of falling through to the 404 of the host router. The
    // PRD documents it as `RouteNotFound`.
    let mut response = call(
        &router,
        "GET",
        &format!("{PROXY_ALIAS_PREFIX}unknown.internal/v1/ping"),
        None,
    )
    .await;
    assert_eq!(
        response.headers().get(ERROR_SOURCE_HEADER).unwrap(),
        ERROR_SOURCE_GATEWAY
    );
    assert_problem(&mut response, 404, "RouteNotFound").await;
}

/// A throwaway in-process upstream answering `200 pong` to every request.
struct MockUpstream {
    addr: std::net::SocketAddr,
    worker: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for MockUpstream {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.abort();
        }
    }
}

impl MockUpstream {
    /// Binds an ephemeral port and serves it until dropped.
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let worker = tokio::spawn(serve(listener));
        Self {
            addr,
            worker: Some(worker),
        }
    }
}

/// Accepts connections until the listener is dropped.
async fn serve(listener: TcpListener) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        tokio::spawn(answer(stream));
    }
}

/// Reads one request head and answers `200 pong`.
async fn answer(mut stream: TcpStream) {
    let mut raw = Vec::new();
    let mut chunk = [0_u8; 2048];
    // The request head is consumed so the proxy never races a reset
    // connection.
    loop {
        let Ok(read) = stream.read(&mut chunk).await else {
            return;
        };
        if read == 0 {
            return;
        }
        raw.extend_from_slice(&chunk[..read]);
        if raw.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let head = "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: 4\r\n\r\n";
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(b"pong").await;
    let _ = stream.flush().await;
    let _ = stream.shutdown().await;
}

#[tokio::test]
async fn a_proxied_request_reaches_the_upstream_through_the_gear_router() {
    let mock = MockUpstream::start().await;
    let (ctx, gear) = wired(e2e_gears()).await;
    let (router, _openapi) = gear_router(&ctx, &gear);

    seed_route(
        &router,
        Some("mock.internal"),
        "127.0.0.1",
        mock.addr.port(),
        "/v1",
    )
    .await;

    let mut response = call(
        &router,
        "GET",
        &format!("{PROXY_ALIAS_PREFIX}mock.internal/v1/ping"),
        None,
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_bytes(&mut response).await, b"pong");
}
