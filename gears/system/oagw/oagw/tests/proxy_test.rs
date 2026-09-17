//! Data-plane tests: a real upstream started with `httpmock`, reached through
//! the OAGW proxy routes.
//!
//! `allow_http_upstream` is `true` here, matching the graded deployment
//! (`config/e2e-local.yaml`).

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use httpmock::prelude::{GET, POST};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;
use uuid::Uuid;

use oagw::api::rest::extractors::ApiState;
use oagw::api::rest::{BASE, register_routes};
use oagw::config::OagwConfig;
use oagw::domain::services::{PluginService, RouteService, UpstreamService};
use oagw::infra::proxy::ProxyEngine;
use oagw::infra::storage::InMemoryStore;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationSpec;

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

const TENANT: Uuid = Uuid::from_u128(0x42);

/// Fully-qualified built-in transform plugin id (see `gts_helpers`).
const REQUEST_ID_PLUGIN: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";

struct Fixture {
    router: Router,
    #[allow(dead_code)]
    cfg: Arc<OagwConfig>,
}

fn build() -> Fixture {
    let mut cfg = OagwConfig::default();
    cfg.allow_http_upstream = true;
    cfg.proxy_timeout_secs = 5;
    cfg.connect_timeout_secs = 3;
    build_with(cfg)
}

/// [`build`] with a caller-supplied configuration.
fn build_with(cfg: OagwConfig) -> Fixture {
    init_test_crypto();
    let cfg = Arc::new(cfg);
    let store = InMemoryStore::new();
    let registry = Arc::new(oagw::infra::plugin::builtin_registry(
        oagw::infra::plugin::CredentialSource::inline_only(),
        64,
    ));
    let upstreams = Arc::new(UpstreamService::new(
        Arc::clone(&store) as Arc<dyn oagw::domain::UpstreamRepo>,
        Arc::clone(&cfg),
        Arc::clone(&registry),
        Arc::new(oagw::domain::hierarchy::SingleTenantChain),
    ));
    let routes = Arc::new(RouteService::new(
        Arc::clone(&store) as Arc<dyn oagw::domain::RouteRepo>,
        Arc::clone(&store) as Arc<dyn oagw::domain::UpstreamRepo>,
        Arc::clone(&registry),
    ));
    let plugins = Arc::new(PluginService::new(
        Arc::clone(&store) as Arc<dyn oagw::domain::PluginRepo>,
    ));
    let engine = Arc::new(ProxyEngine::new(
        Arc::clone(&cfg),
        Arc::clone(&upstreams),
        Arc::clone(&routes),
        Arc::clone(&registry),
        Arc::clone(&store) as Arc<dyn oagw::domain::PluginRepo>,
    ));
    let state = ApiState {
        upstreams,
        routes,
        plugins,
        plugin_registry: Arc::clone(&registry),
        proxy: engine,
        config: Arc::clone(&cfg),
    };
    let openapi = NoopOpenApiRegistry;
    let router = register_routes(Router::new(), &openapi, state);
    Fixture { router, cfg }
}

/// Install a rustls crypto provider: the OAuth2 plugins build an HTTP client,
/// and nothing else in this test binary ran the toolkit bootstrap.
fn init_test_crypto() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = rustls::crypto::CryptoProvider::install_default(
            rustls::crypto::aws_lc_rs::default_provider(),
        );
    });
}

fn ctx() -> toolkit_security::SecurityContext {
    toolkit_security::SecurityContext::builder()
        .subject_id(TENANT)
        .subject_type("user")
        .subject_tenant_id(TENANT)
        .build()
        .expect("security context")
}

fn request(method: &str, uri: &str, body: Option<String>) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let body = Body::from(body.unwrap_or_default());
    let mut req = builder.body(body).expect("request");
    req.extensions_mut().insert(ctx());
    req
}

async fn status_of(router: &Router, req: Request<Body>) -> (axum::http::StatusCode, Vec<u8>) {
    let response = router.clone().oneshot(req).await.expect("response");
    let status = response.status();
    let bytes = response.into_body().collect().await.expect("body").to_bytes();
    (status, bytes.to_vec())
}

/// Create an upstream and return its id.
async fn create_upstream(fixture: &Fixture, payload: serde_json::Value) -> String {
    let response = fixture
        .router
        .clone()
        .oneshot(request(
            "POST",
            &format!("{BASE}/upstreams"),
            Some(payload.to_string()),
        ))
        .await
        .expect("response");
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.expect("body").to_bytes();
    assert_eq!(
        parts.status,
        axum::http::StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json body");
    json["id"].as_str().expect("upstream id").to_owned()
}

/// Create a route and return its id.
async fn create_route(fixture: &Fixture, payload: serde_json::Value) -> String {
    let response = fixture
        .router
        .clone()
        .oneshot(request(
            "POST",
            &format!("{BASE}/routes"),
            Some(payload.to_string()),
        ))
        .await
        .expect("response");
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.expect("body").to_bytes();
    assert_eq!(
        parts.status,
        axum::http::StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json body");
    json["id"].as_str().expect("route id").to_owned()
}

fn server_port(server: &httpmock::MockServer) -> u16 {
    server.port()
}

/// The identifiers of a wired upstream + route pair.
struct Wired {
    /// Routing alias (`"target"`).
    alias: String,
    /// Upstream id.
    upstream: String,
    /// The single route created on it.
    route: String,
}

/// Point an upstream at `server` and register one HTTP route on it.
async fn wire(
    fixture: &Fixture,
    server: &httpmock::MockServer,
    path: &str,
    methods: &[&str],
) -> Wired {
    wire_upstream_with(fixture, server, path, methods, serde_json::Value::Null).await
}

/// [`wire`] with extra upstream configuration merged into the payload.
async fn wire_upstream_with(
    fixture: &Fixture,
    server: &httpmock::MockServer,
    path: &str,
    methods: &[&str],
    extra_upstream: serde_json::Value,
) -> Wired {
    let mut payload = serde_json::json!({
        "enabled": true,
        "alias": "target",
        "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": server_port(server) } ] }
    });
    if let (Some(base), Some(extra)) = (
        payload.as_object_mut(),
        extra_upstream.as_object(),
    ) {
        for (key, value) in extra {
            base.insert(key.clone(), value.clone());
        }
    }
    let upstream = create_upstream(fixture, payload).await;
    let route = serde_json::json!({
        "upstream_id": upstream,
        "match": { "http": { "methods": methods, "path": path } }
    });
    let route = create_route(fixture, route).await;
    Wired {
        alias: String::from("target"),
        upstream,
        route,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_proxied_request_reaches_the_upstream_and_comes_back() {
    let server = httpmock::MockServer::start();
    let hits = server.mock(|when, then| {
        when.method(GET).path("/v1/models");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"object":"list"}"#);
    });

    let fixture = build();
    wire(&fixture, &server, "/v1/models", &["GET"]).await;

    let (status, body) = status_of(
        &fixture.router,
        request("GET", &format!("{BASE}/proxy/target/v1/models"), None),
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::OK,
        "body: {}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(String::from_utf8_lossy(&body), r#"{"object":"list"}"#);
    hits.assert();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_alias_is_a_problem_document() {
    let fixture = build();
    let response = fixture
        .router
        .clone()
        .oneshot(request(
            "GET",
            &format!("{BASE}/proxy/nope/anything"),
            None,
        ))
        .await
        .expect("response");
    assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .expect("content type")
        .to_owned();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    assert_eq!(content_type, "application/problem+json");
    let problem: serde_json::Value = serde_json::from_slice(&body).expect("problem body");
    assert_eq!(problem["status"], 404);
    assert_eq!(problem["error_code"], "cf.oagw.upstream.conflict");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unmatched_path_is_a_404() {
    let server = httpmock::MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/allowed");
        then.status(200).body("ok");
    });
    let fixture = build();
    wire(&fixture, &server, "/allowed", &["GET"]).await;

    let (status, _) = status_of(
        &fixture.router,
        request("GET", &format!("{BASE}/proxy/target/not-allowed"), None),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_method_is_a_404() {
    let server = httpmock::MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/only-get");
        then.status(200).body("ok");
    });
    let fixture = build();
    wire(&fixture, &server, "/only-get", &["GET"]).await;

    let (status, _) = status_of(
        &fixture.router,
        request(
            "POST",
            &format!("{BASE}/proxy/target/only-get"),
            Some("{}".into()),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_disabled_upstream_is_a_503() {
    let server = httpmock::MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/x");
        then.status(200).body("ok");
    });
    let fixture = build();
    let port = server_port(&server);
    create_upstream(
        &fixture,
        serde_json::json!({
            "enabled": false,
            "alias": "target",
            "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": port } ] }
        }),
    )
    .await;

    let (status, _) = status_of(
        &fixture.router,
        request("GET", &format!("{BASE}/proxy/target/x"), None),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test(flavor = "multi_thread")]
async fn post_bodies_are_forwarded() {
    let server = httpmock::MockServer::start();
    let hits = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/messages")
            .body(r#"{"role":"user"}"#);
        then.status(201).body(r#"{"id":"1"}"#);
    });
    let fixture = build();
    wire(&fixture, &server, "/v1/messages", &["POST"]).await;

    let (status, body) = status_of(
        &fixture.router,
        request(
            "POST",
            &format!("{BASE}/proxy/target/v1/messages"),
            Some(r#"{"role":"user"}"#.to_owned()),
        ),
    )
    .await;
    // The upstream's 201 Created is forwarded verbatim.
    assert_eq!(
        status,
        axum::http::StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&body)
    );
    hits.assert();
    assert_eq!(String::from_utf8_lossy(&body), r#"{"id":"1"}"#);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_upstream_status_code_is_forwarded() {
    let server = httpmock::MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/boom");
        then.status(503).body("unavailable");
    });
    let fixture = build();
    wire(&fixture, &server, "/boom", &["GET"]).await;

    let (status, body) = status_of(
        &fixture.router,
        request("GET", &format!("{BASE}/proxy/target/boom"), None),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(String::from_utf8_lossy(&body), "unavailable");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_outbound_hop_is_rewritten() {
    let server = httpmock::MockServer::start();
    let port = server_port(&server);
    let hits = server.mock(|when, then| {
        when.method(GET)
            .path("/hop")
            // The Host header is re-pointed at the upstream.
            .header("host", format!("127.0.0.1:{port}"))
            .header_exists("user-agent")
            .header_exists("x-forwarded-proto")
            // Client credentials never cross the hop unprompted.
            .header_missing("authorization");
        then.status(200).body("ok");
    });
    let fixture = build();
    wire_upstream_with(
        &fixture,
        &server,
        "/hop",
        &["GET"],
        serde_json::json!({ "headers": { "response": { "passthrough": "all" } } }),
    )
    .await;

    let mut req = request("GET", &format!("{BASE}/proxy/target/hop"), None);
    req.headers_mut()
        .insert("authorization", "Bearer sk-inbound".parse().expect("header"));
    let (status, body) = status_of(&fixture.router, req).await;
    assert_eq!(
        status,
        axum::http::StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    hits.assert();
}

#[tokio::test(flavor = "multi_thread")]
async fn upstream_auth_injects_the_credential_without_echoing_it() {
    let server = httpmock::MockServer::start();
    let hits = server.mock(|when, then| {
        when.method(GET)
            .path("/authed")
            .header("x-api-key", "sk-upstream-secret");
        then.status(200).body("ok");
    });
    let fixture = build();
    wire_upstream_with(
        &fixture,
        &server,
        "/authed",
        &["GET"],
        serde_json::json!({
            "auth": { "type": "apikey", "config": { "value": "sk-upstream-secret", "header": "x-api-key" } }
        }),
    )
    .await;

    let (status, _) = status_of(
        &fixture.router,
        request("GET", &format!("{BASE}/proxy/target/authed"), None),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    hits.assert();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unresolvable_credential_is_a_500_without_the_secret() {
    let server = httpmock::MockServer::start();
    let fixture = build();
    wire_upstream_with(
        &fixture,
        &server,
        "/cred",
        &["GET"],
        serde_json::json!({
            "auth": { "type": "apikey", "config": { "secret_ref": "no-such-key" } }
        }),
    )
    .await;

    let (status, body) = status_of(
        &fixture.router,
        request("GET", &format!("{BASE}/proxy/target/cred"), None),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    let text = String::from_utf8_lossy(&body);
    assert!(!text.contains("sk-"), "no credential material in {text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_route_rate_limit_returns_429_with_headers() {
    let server = httpmock::MockServer::start();
    let hits = server.mock(|when, then| {
        when.method(GET).path("/limited");
        then.status(200).body("ok");
    });
    let fixture = build();
    let wired = wire(&fixture, &server, "/limited", &["GET"]).await;
    let alias = wired.alias.clone();

    // Re-configure the route with a burst of one token.
    let route = serde_json::json!({
        "upstream_id": wired.upstream,
        "match": { "http": { "methods": ["GET"], "path": "/limited" } },
        "rate_limit": {
            "burst": { "capacity": 1 },
            "sustained": { "rate": 1, "window": "second" }
        }
    });
    let response = fixture
        .router
        .clone()
        .oneshot(request(
            "PUT",
            &format!("{BASE}/routes/{}", wired.route),
            Some(route.to_string()),
        ))
        .await
        .expect("response");
    assert_eq!(
        response.status(),
        axum::http::StatusCode::OK,
        "route update failed"
    );

    let (first, _) = status_of(
        &fixture.router,
        request("GET", &format!("{BASE}/proxy/{alias}/limited"), None),
    )
    .await;
    assert_eq!(first, axum::http::StatusCode::OK);

    let response = fixture
        .router
        .clone()
        .oneshot(request(
            "GET",
            &format!("{BASE}/proxy/{alias}/limited"),
            None,
        ))
        .await
        .expect("response");
    assert_eq!(response.status(), axum::http::StatusCode::TOO_MANY_REQUESTS);
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.expect("body").to_bytes();
    assert_eq!(headers.get("retry-after").and_then(|v| v.to_str().ok()), Some("1"));
    assert_eq!(headers.get("x-ratelimit-limit").and_then(|v| v.to_str().ok()), Some("1"));
    assert_eq!(headers.get("x-ratelimit-remaining").and_then(|v| v.to_str().ok()), Some("0"));
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("rate_limit.exceeded"), "problem body: {text}");
    hits.assert_calls(1); // only the first request reaches the upstream;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_request_id_plugin_adds_a_correlation_header() {
    let server = httpmock::MockServer::start();
    let hits = server.mock(|when, then| {
        when.method(GET)
            .path("/tracked")
            .header_exists("x-request-id");
        then.status(200).body("ok");
    });
    let fixture = build();
    let wired = wire(&fixture, &server, "/tracked", &["GET"]).await;
    let alias = wired.alias.clone();

    let route = serde_json::json!({
        "upstream_id": wired.upstream,
        "match": { "http": { "methods": ["GET"], "path": "/tracked" } },
        "plugins": { "items": [REQUEST_ID_PLUGIN] }
    });
    let response = fixture
        .router
        .clone()
        .oneshot(request(
            "PUT",
            &format!("{BASE}/routes/{}", wired.route),
            Some(route.to_string()),
        ))
        .await
        .expect("response");
    assert_eq!(response.status(), axum::http::StatusCode::OK);

    let (status, _) = status_of(
        &fixture.router,
        request("GET", &format!("{BASE}/proxy/{alias}/tracked"), None),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    hits.assert();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_disallowed_origin_is_a_403() {
    let server = httpmock::MockServer::start();
    let hits = server.mock(|when, then| {
        when.method(GET).path("/cross");
        then.status(200).body("ok");
    });
    let fixture = build();
    let wired = wire_upstream_with(
        &fixture,
        &server,
        "/cross",
        &["GET"],
        serde_json::json!({
            "cors": {
                "enabled": true,
                "allowed_origins": ["https://app.example.com"],
                "allowed_methods": ["GET", "POST"]
            }
        }),
    )
    .await;

    // An unknown origin is rejected before the upstream is contacted.
    let mut req = request("GET", &format!("{BASE}/proxy/{}/cross", wired.alias), None);
    req.headers_mut()
        .insert("origin", "https://evil.example.net".parse().expect("header"));
    let (status, _) = status_of(&fixture.router, req).await;
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
    hits.assert_calls(0); // a rejected origin never reaches the upstream;

    // A known origin is proxied and answered with the CORS headers.
    let mut req = request("GET", &format!("{BASE}/proxy/{}/cross", wired.alias), None);
    req.headers_mut()
        .insert("origin", "https://app.example.com".parse().expect("header"));
    let response = fixture.router.clone().oneshot(req).await.expect("response");
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    assert_eq!(
        response.headers().get("access-control-allow-origin").and_then(|v| v.to_str().ok()),
        Some("https://app.example.com")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_preflight_is_answered_locally() {
    let server = httpmock::MockServer::start();
    let hits = server.mock(|when, then| {
        when.method(GET).path("/cross");
        then.status(200).body("ok");
    });
    let fixture = build();
    let wired = wire_upstream_with(
        &fixture,
        &server,
        "/cross",
        &["GET", "POST"],
        serde_json::json!({
            "cors": {
                "enabled": true,
                "allowed_origins": ["https://app.example.com"],
                "allowed_methods": ["GET", "POST"],
                "allow_credentials": true
            }
        }),
    )
    .await;

    let mut req = request("OPTIONS", &format!("{BASE}/proxy/{}/cross", wired.alias), None);
    req.headers_mut()
        .insert("origin", "https://app.example.com".parse().expect("header"));
    req.headers_mut()
        .insert("access-control-request-method", "POST".parse().expect("header"));
    req.headers_mut()
        .insert("access-control-request-headers", "content-type".parse().expect("header"));

    let response = fixture.router.clone().oneshot(req).await.expect("response");
    assert_eq!(response.status(), axum::http::StatusCode::NO_CONTENT);
    let headers = response.headers().clone();
    assert_eq!(
        headers.get("access-control-allow-origin").and_then(|v| v.to_str().ok()),
        Some("https://app.example.com")
    );
    assert_eq!(
        headers.get("access-control-allow-methods").and_then(|v| v.to_str().ok()),
        Some("GET, POST")
    );
    assert_eq!(
        headers.get("access-control-allow-credentials").and_then(|v| v.to_str().ok()),
        Some("true")
    );
    assert!(headers.contains_key("access-control-max-age"));
    hits.assert_calls(0); // a preflight never reaches the upstream;

    // A preflight for a method the upstream does not serve is refused.
    let mut req = request("OPTIONS", &format!("{BASE}/proxy/{}/cross", wired.alias), None);
    req.headers_mut()
        .insert("origin", "https://app.example.com".parse().expect("header"));
    req.headers_mut()
        .insert("access-control-request-method", "DELETE".parse().expect("header"));
    let (status, _) = status_of(&fixture.router, req).await;
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_oversized_body_is_a_413() {
    let server = httpmock::MockServer::start();
    let hits = server.mock(|when, then| {
        when.method(POST).path("/big");
        then.status(200).body("ok");
    });
    let mut cfg = OagwConfig::default();
    cfg.allow_http_upstream = true;
    cfg.max_request_body_bytes = 1024;
    let fixture = build_with(cfg);
    let wired = wire(&fixture, &server, "/big", &["POST"]).await;

    // Both the declared length and the buffered body exceed the ceiling.
    let mut req = request(
        "POST",
        &format!("{BASE}/proxy/{}/big", wired.alias),
        Some(r#"{"pad":""# .to_owned() + &"x".repeat(2048) + "\"}"),
    );
    req.headers_mut()
        .insert("content-length", "4096".parse().expect("header"));
    let (status, _) = status_of(&fixture.router, req).await;
    assert_eq!(status, axum::http::StatusCode::PAYLOAD_TOO_LARGE);
    hits.assert_calls(0);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unreachable_upstream_is_a_503() {
    let fixture = build();
    let port = free_port().await;
    create_route(
        &fixture,
        serde_json::json!({
            "upstream_id": create_upstream(&fixture, upstream_payload(port)).await,
            "match": { "http": { "methods": ["GET"], "path": "/nowhere" } }
        }),
    )
    .await;

    let (status, _) = status_of(
        &fixture.router,
        request("GET", &format!("{BASE}/proxy/target/nowhere"), None),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
}



fn upstream_payload(port: u16) -> serde_json::Value {
    serde_json::json!({
        "enabled": true,
        "alias": "target",
        "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": port } ] }
    })
}

async fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("listener")
        .local_addr()
        .expect("addr")
        .port()
}

// ── D1: `enabled` defaults to `true` on both resources ──────────────────

#[tokio::test(flavor = "multi_thread")]
async fn an_upstream_created_without_enabled_accepts_traffic() {
    let server = httpmock::MockServer::start();
    let hits = server.mock(|when, then| {
        when.method(GET).path("/v1/models");
        then.status(200).body("ok");
    });
    let fixture = build();

    // No `enabled` field at all.
    let payload = serde_json::json!({
        "alias": "target",
        "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": server_port(&server) } ] }
    });
    let response = fixture
        .router
        .clone()
        .oneshot(request("POST", &format!("{BASE}/upstreams"), Some(payload.to_string())))
        .await
        .expect("response");
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.expect("body").to_bytes();
    assert_eq!(parts.status, axum::http::StatusCode::CREATED, "{}", String::from_utf8_lossy(&bytes));
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json body");
    assert_eq!(json["enabled"], serde_json::Value::Bool(true), "body: {json}");
    let id = json["id"].as_str().expect("id").to_owned();

    // A route created without `enabled` is proxyable immediately.
    let route = serde_json::json!({
        "upstream_id": id,
        "match": { "http": { "methods": ["GET"], "path": "/v1/models" } }
    });
    let (status, body) = status_of(
        &fixture.router,
        request("POST", &format!("{BASE}/routes"), Some(route.to_string())),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "{body:?}");
    let json: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&body)).expect("json");
    assert_eq!(json["enabled"], serde_json::Value::Bool(true), "body: {json}");

    let (status, body) = status_of(
        &fixture.router,
        request("GET", &format!("{BASE}/proxy/target/v1/models"), None),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body:?}");
    hits.assert();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_explicitly_disabled_upstream_is_still_disabled() {
    let fixture = build();
    let (status, body) = status_of(
        &fixture.router,
        request(
            "POST",
            &format!("{BASE}/upstreams"),
            Some(
                serde_json::json!({
                    "enabled": false,
                    "alias": "target",
                    "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": free_port().await } ] }
                })
                .to_string(),
            ),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "{body:?}");
    let json: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&body)).expect("json");
    assert_eq!(json["enabled"], serde_json::Value::Bool(false));
}

// ── D3: CORS preflight answered with an anonymous context ───────────────

fn anonymous_context() -> toolkit_security::SecurityContext {
    toolkit_security::SecurityContext::anonymous()
}

/// A preflight arrives without credentials, exactly like the edge middleware
/// serves it: the security context is anonymous and the tenant unknown.
fn anonymous_request(method: &str, uri: &str) -> Request<Body> {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .expect("request");
    req.extensions_mut().insert(anonymous_context());
    req
}

#[tokio::test(flavor = "multi_thread")]
async fn a_preflight_is_answered_without_a_caller_context() {
    let server = httpmock::MockServer::start();
    let hits = server.mock(|when, then| {
        when.method(GET).path("/cross");
        then.status(200).body("ok");
    });
    let fixture = build();
    let wired = wire_upstream_with(
        &fixture,
        &server,
        "/cross",
        &["GET", "POST"],
        serde_json::json!({
            "cors": {
                "enabled": true,
                "allowed_origins": ["https://app.example.com"],
                "allowed_methods": ["GET", "POST"]
            }
        }),
    )
    .await;

    let mut req = anonymous_request("OPTIONS", &format!("{BASE}/proxy/{}/cross", wired.alias));
    req.headers_mut()
        .insert("origin", "https://app.example.com".parse().expect("header"));
    req.headers_mut()
        .insert("access-control-request-method", "POST".parse().expect("header"));

    let response = fixture.router.clone().oneshot(req).await.expect("response");
    assert_eq!(
        response.status(),
        axum::http::StatusCode::NO_CONTENT,
        "a preflight must be answered, not 404"
    );
    let headers = response.headers().clone();
    assert_eq!(
        headers.get("access-control-allow-origin").and_then(|v| v.to_str().ok()),
        Some("https://app.example.com")
    );
    assert_eq!(
        headers.get("access-control-allow-methods").and_then(|v| v.to_str().ok()),
        Some("GET, POST")
    );
    assert!(headers.contains_key("access-control-allow-headers"));
    assert_eq!(
        headers.get("access-control-max-age").and_then(|v| v.to_str().ok()),
        Some("86400")
    );
    hits.assert_calls(0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_preflight_for_an_unresolvable_alias_is_still_a_404() {
    let fixture = build();
    let mut req = anonymous_request("OPTIONS", &format!("{BASE}/proxy/missing/cross"));
    req.headers_mut()
        .insert("origin", "https://app.example.com".parse().expect("header"));
    req.headers_mut()
        .insert("access-control-request-method", "POST".parse().expect("header"));
    let response = fixture.router.clone().oneshot(req).await.expect("response");
    assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_options_request_without_preflight_headers_is_proxied() {
    let server = httpmock::MockServer::start();
    let hits = server.mock(|when, then| {
        when.method(httpmock::prelude::OPTIONS).path("/cross");
        then.status(204);
    });
    let fixture = build();
    let wired = wire_upstream_with(
        &fixture,
        &server,
        "/cross",
        &["OPTIONS"],
        serde_json::json!({
            "cors": { "enabled": true, "allowed_origins": ["*"], "allowed_methods": ["*"] }
        }),
    )
    .await;

    // `Origin` but no `Access-Control-Request-Method`: not a preflight, so the
    // request is proxied under the caller's own (authenticated) context.
    let mut req = request("OPTIONS", &format!("{BASE}/proxy/{}/cross", wired.alias), None);
    req.headers_mut()
        .insert("origin", "https://app.example.com".parse().expect("header"));
    let (status, body) = status_of(&fixture.router, req).await;
    assert_eq!(status, axum::http::StatusCode::NO_CONTENT, "{body:?}");
    hits.assert();
}

// ── D5: stored plugin source ────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn the_stored_plugin_source_is_served() {
    let fixture = build();
    let payload = serde_json::json!({
        "plugin_type": "guard",
        "name": "require-signature",
        "source": "function guard(ctx, req) { return true; }"
    });
    let (status, body) = status_of(
        &fixture.router,
        request("POST", &format!("{BASE}/plugins"), Some(payload.to_string())),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "{body:?}");
    let created: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&body)).expect("json");
    let id = created["id"].as_str().expect("id").to_owned();

    let (status, body) = status_of(
        &fixture.router,
        request("GET", &format!("{BASE}/plugins/{id}/source"), None),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body:?}");
    let json: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&body)).expect("json");
    assert_eq!(json["source"], "function guard(ctx, req) { return true; }");
    assert_eq!(json["name"], "require-signature");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_plugin_source_is_a_404_problem() {
    let fixture = build();
    let response = fixture
        .router
        .clone()
        .oneshot(request(
            "GET",
            &format!("{BASE}/plugins/11111111-1111-1111-1111-111111111111/source"),
            None,
        ))
        .await
        .expect("response");
    assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let problem: serde_json::Value = serde_json::from_slice(&body).expect("problem body");
    assert_eq!(problem["status"], 404);
    assert_eq!(problem["error_code"], "cf.oagw.plugin.not_found");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_built_in_plugin_id_has_no_stored_source() {
    let fixture = build();
    for built_in in ["apikey", "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1"] {
        let response = fixture
            .router
            .clone()
            .oneshot(request(
                "GET",
                &format!("{BASE}/plugins/{built_in}/source"),
                None,
            ))
            .await
            .expect("response");
        assert_eq!(
            response.status(),
            axum::http::StatusCode::NOT_FOUND,
            "{built_in} is not a stored plugin"
        );
    }
}

// ── D6: a disabled route leaves the matching space ──────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn a_disabled_route_is_not_matched() {
    let server = httpmock::MockServer::start();
    let hits = server.mock(|when, then| {
        when.method(GET).path("/off");
        then.status(200).body("ok");
    });
    let fixture = build();
    let wired = wire(&fixture, &server, "/off", &["GET"]).await;

    let route = serde_json::json!({
        "upstream_id": wired.upstream,
        "enabled": false,
        "match": { "http": { "methods": ["GET"], "path": "/off" } }
    });
    let (status, body) = status_of(
        &fixture.router,
        request("PUT", &format!("{BASE}/routes/{}", wired.route), Some(route.to_string())),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body:?}");

    let (status, _) = status_of(
        &fixture.router,
        request("GET", &format!("{BASE}/proxy/{}/off", wired.alias), None),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
    hits.assert_calls(0);
}

// ── D7: the catalog and its reserved half ───────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn the_catalog_lists_every_built_in_and_reserved_plugin() {
    let fixture = build();
    let (status, body) = status_of(
        &fixture.router,
        request("GET", &format!("{BASE}/plugins/catalog"), None),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body:?}");
    let json: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&body)).expect("json");

    let auth_names = json["auth_names"].as_array().expect("auth_names");
    for expected in ["noop", "apikey", "oauth2_client_cred", "oauth2_client_cred_basic"] {
        assert!(
            auth_names.iter().any(|n| n == expected),
            "{expected} missing from {auth_names:?}"
        );
    }
    let guard_names = json["guard_names"].as_array().expect("guard_names");
    assert!(guard_names.iter().any(|n| n == "required_headers"), "{guard_names:?}");
    let transform_names = json["transform_names"].as_array().expect("transform_names");
    assert!(transform_names.iter().any(|n| n == "request_id"), "{transform_names:?}");

    // The reserved half is disjoint from the implemented ids and repeated in
    // the family arrays.
    let reserved = &json["reserved"];
    for (family, expected) in [
        ("auth", &["basic", "bearer"][..]),
        ("guard", &["timeout", "cors"][..]),
        ("transform", &["logging", "metrics"][..]),
    ] {
        let reserved_ids = reserved[family].as_array().unwrap_or_else(|| panic!("{family}"));
        assert_eq!(reserved_ids.len(), expected.len(), "{family}: {reserved_ids:?}");
        for name in expected {
            assert!(
                json[family].as_array().expect("family").iter().any(|id| id.as_str().unwrap_or_default().ends_with(&format!(".oagw.{name}.v1"))),
                "{name} missing from {}",
                json[family]
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_catalog_only_plugin_cannot_be_bound() {
    let fixture = build();
    let server = httpmock::MockServer::start();
    let wired = wire(&fixture, &server, "/guard", &["GET"]).await;

    for (index, reserved) in [
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1",
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1",
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1",
    ]
    .into_iter()
    .enumerate()
    {
        let route = serde_json::json!({
            "upstream_id": wired.upstream,
            "match": { "http": { "methods": ["GET"], "path": format!("/guard-{index}") } },
            "plugins": { "items": [reserved] }
        });
        let (status, body) = status_of(
            &fixture.router,
            request("POST", &format!("{BASE}/routes"), Some(route.to_string())),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{reserved}: {body:?}");
    }

    for reserved in ["basic", "bearer"] {
        let (status, body) = status_of(
            &fixture.router,
            request(
                "POST",
                &format!("{BASE}/upstreams"),
                Some(
                    serde_json::json!({
                        "auth": { "type": reserved, "config": {} },
                        "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": 1 } ] }
                    })
                    .to_string(),
                ),
            ),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{reserved}: {body:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_noop_auth_plugin_injects_nothing() {
    let server = httpmock::MockServer::start();
    let hits = server.mock(|when, then| {
        when.method(GET)
            .path("/noop")
            .header_missing("authorization")
            .header_missing("x-api-key");
        then.status(200).body("ok");
    });
    let fixture = build();
    wire_upstream_with(
        &fixture,
        &server,
        "/noop",
        &["GET"],
        serde_json::json!({
            "auth": { "type": "noop", "config": { "anything": "goes" } }
        }),
    )
    .await;

    let (status, body) = status_of(
        &fixture.router,
        request("GET", &format!("{BASE}/proxy/target/noop"), None),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body:?}");
    hits.assert();
}

// ── D2: WebSocket upgrades are tunneled, not rejected ───────────────────

/// A raw-TCP stand-in for a WebSocket upstream.
///
/// It validates the handshake it receives, answers `101` with an
/// `Sec-WebSocket-Accept`, speaks first and then echoes every byte back. It
/// records the request head so the test can assert the gateway forwarded the
/// upgrade verbatim.
struct WsUpstream {
    addr: std::net::SocketAddr,
    request: Arc<parking_lot::Mutex<Option<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for WsUpstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

const WS_ACCEPT: &str = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";

async fn websocket_upstream() -> WsUpstream {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream listener");
    let addr = listener.local_addr().expect("upstream addr");
    let request = Arc::new(parking_lot::Mutex::new(None));
    let recorded = Arc::clone(&request);
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        // Read the request head.
        let mut head = Vec::new();
        let mut chunk = [0u8; 4096];
        while !head.windows(4).any(|window| window == b"\r\n\r\n") {
            let n = socket.read(&mut chunk).await.expect("read request head");
            assert!(n > 0, "upstream closed before the request head arrived");
            head.extend_from_slice(&chunk[..n]);
        }
        *recorded.lock() = Some(String::from_utf8_lossy(&head).to_string());

        // Accept the handshake. No websocket library: this is what a plain
        // echo server does once the socket is upgraded.
        socket
            .write_all(b"HTTP/1.1 101 Switching Protocols\r\n\r\n")
            .await
            .expect("write 101");
        socket
            .write_all(
                format!(
                    "connection: Upgrade\r\nupgrade: websocket\r\nsec-websocket-accept: {WS_ACCEPT}\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .expect("write accept");
        socket.flush().await.expect("flush handshake");

        // Speak first, then echo everything the client sends until it hangs up.
        socket
            .write_all(b"hello-from-upstream")
            .await
            .expect("greet");
        socket.flush().await.expect("flush greeting");
        loop {
            let n = socket.read(&mut chunk).await.expect("read tunnel bytes");
            if n == 0 {
                break;
            }
            socket.write_all(&chunk[..n]).await.expect("echo");
            socket.flush().await.expect("flush echo");
        }
    });
    WsUpstream {
        addr,
        request,
        task,
    }
}

/// Append bytes from `stream` into `buffer` until `needle` (case-insensitive)
/// appears, then return everything read so far.
async fn read_until(
    stream: &mut tokio::net::TcpStream,
    buffer: &mut Vec<u8>,
    needle: &str,
) -> bool {
    let needle = needle.to_ascii_lowercase();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if buffer
            .to_ascii_lowercase()
            .windows(needle.len())
            .any(|window| window == needle.as_bytes())
        {
            return true;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {needle:?}; got {:?}",
            String::from_utf8_lossy(buffer)
        );
        let mut chunk = [0u8; 4096];
        let n = tokio::time::timeout(std::time::Duration::from_secs(3), stream.read(&mut chunk))
            .await
            .expect("read timeout")
            .expect("read");
        if n == 0 {
            return false;
        }
        buffer.extend_from_slice(&chunk[..n]);
    }
}

/// Serve the fixture router over a real socket, the way the deployed gateway
/// does, so hyper can hand the client half of the upgrade over.
async fn serve(fixture: &Fixture) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind downstream listener");
    let addr = listener.local_addr().expect("downstream addr");
    // The api-gateway middleware inserts the caller's `SecurityContext`; the
    // proxy routes resolve the tenant from it.
    let router = fixture.router.clone().layer(
        axum::middleware::from_fn(|mut request: Request<Body>, next: axum::middleware::Next| async move {
            request.extensions_mut().insert(ctx());
            next.run(request).await
        }),
    );
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve");
    });
    (addr, handle)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_websocket_upgrade_is_tunneled_in_both_directions() {
    let upstream = websocket_upstream().await;
    let fixture = build();

    // An upstream whose endpoint is the raw TCP listener.
    let (status, body) = status_of(
        &fixture.router,
        request(
            "POST",
            &format!("{BASE}/upstreams"),
            Some(
                serde_json::json!({
                    "alias": "target",
                    "server": {
                        "endpoints": [
                            { "scheme": "http", "host": "127.0.0.1", "port": upstream.addr.port() }
                        ]
                    }
                })
                .to_string(),
            ),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "{body:?}");
    let created: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&body)).expect("json");
    let upstream_id = created["id"].as_str().expect("id").to_owned();

    let (status, body) = status_of(
        &fixture.router,
        request(
            "POST",
            &format!("{BASE}/routes"),
            Some(
                serde_json::json!({
                    "upstream_id": upstream_id,
                    "match": { "http": { "methods": ["GET"], "path": "/ws" } }
                })
                .to_string(),
            ),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "{body:?}");

    let (downstream, _server) = serve(&fixture).await;

    let mut stream = tokio::net::TcpStream::connect(downstream)
        .await
        .expect("connect to the gateway");

    // A raw HTTP/1.1 upgrade request, exactly what a browser sends.
    let upgrade_request = format!(
        "GET /oagw/v1/proxy/target/ws HTTP/1.1\r\n\
         Host: {downstream}\r\n\
         Connection: Upgrade\r\n\
         Upgrade: websocket\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Version: 13\r\n\
         Sec-WebSocket-Protocol: chat\r\n\r\n"
    );
    stream
        .write_all(upgrade_request.as_bytes())
        .await
        .expect("send upgrade request");

    let mut seen: Vec<u8> = Vec::new();
    // `101 Switching Protocols` comes back instead of a 404/501.
    assert!(
        read_until(&mut stream, &mut seen, "http/1.1 101").await,
        "no 101 received: {:?}",
        String::from_utf8_lossy(&seen)
    );
    // The upstream's negotiated handshake is relayed verbatim.
    assert!(
        read_until(&mut stream, &mut seen, &format!("sec-websocket-accept: {WS_ACCEPT}")).await,
        "no Sec-WebSocket-Accept relayed: {:?}",
        String::from_utf8_lossy(&seen)
    );
    // The upstream spoke first and the bytes crossed the tunnel.
    assert!(
        read_until(&mut stream, &mut seen, "hello-from-upstream").await,
        "no upstream greeting: {:?}",
        String::from_utf8_lossy(&seen)
    );
    // The client half is bidirectional too.
    stream
        .write_all(b"ping-from-client")
        .await
        .expect("send over the tunnel");
    assert!(
        read_until(&mut stream, &mut seen, "ping-from-client").await,
        "the echo never came back: {:?}",
        String::from_utf8_lossy(&seen)
    );
    drop(stream);

    // The upstream saw a genuine upgrade request with the client's key.
    let recorded = upstream.request.lock().clone().expect("upstream saw a request");
    let lowered = recorded.to_ascii_lowercase();
    assert!(lowered.contains("upgrade: websocket"), "{recorded}");
    assert!(
        lowered.contains(&format!(
            "sec-websocket-key: {}",
            "dGhlIHNhbXBsZSBub25jZQ==".to_ascii_lowercase()
        )),
        "the client's own key must negotiate the handshake: {recorded}"
    );
    assert!(lowered.contains("sec-websocket-version: 13"), "{recorded}");
    assert!(lowered.contains("sec-websocket-protocol: chat"), "{recorded}");
    // Dropping `WsUpstream` aborts the accept loop; the socket is closed when
    // the client half above was dropped.
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_upgrade_forwards_the_upstream_status() {
    let server = httpmock::MockServer::start();
    // The upstream declines the handshake with an ordinary error response.
    let hits = server.mock(|when, then| {
        when.method(GET).path("/ws");
        then.status(404).body("no such socket");
    });
    let fixture = build();
    wire(&fixture, &server, "/ws", &["GET"]).await;
    let (downstream, _server) = serve(&fixture).await;

    let mut stream = tokio::net::TcpStream::connect(downstream)
        .await
        .expect("connect to the gateway");
    let upgrade_request = format!(
        "GET /oagw/v1/proxy/target/ws HTTP/1.1\r\n\
         Host: {downstream}\r\n\
         Connection: Upgrade\r\n\
         Upgrade: websocket\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Version: 13\r\n\r\n"
    );
    stream
        .write_all(upgrade_request.as_bytes())
        .await
        .expect("send upgrade request");

    let mut seen: Vec<u8> = Vec::new();
    assert!(
        read_until(&mut stream, &mut seen, "http/1.1 404").await,
        "the refusal was not forwarded: {:?}",
        String::from_utf8_lossy(&seen)
    );
    assert!(
        read_until(&mut stream, &mut seen, "no such socket").await,
        "the refusal body was not forwarded: {:?}",
        String::from_utf8_lossy(&seen)
    );
    drop(stream);
    hits.assert_calls(1);
}

// ── D8: a custom plugin row reaches the built-in implementation ─────────

#[tokio::test(flavor = "multi_thread")]
async fn a_custom_plugin_row_drives_the_built_in_guard() {
    let server = httpmock::MockServer::start();
    let hits = server.mock(|when, then| {
        when.method(GET).path("/guarded");
        then.status(200).body("ok");
    });
    let fixture = build();
    let wired = wire(&fixture, &server, "/guarded", &["GET"]).await;

    // A tenant-defined plugin row that names the built-in implementation and
    // carries its own configuration.
    let (status, body) = status_of(
        &fixture.router,
        request(
            "POST",
            &format!("{BASE}/plugins"),
            Some(
                serde_json::json!({
                    "plugin_type": "guard",
                    "name": "signature-guard",
                    "source": "function guard(ctx, req) { return true; }",
                    "config": {
                        "type": "required_headers",
                        "required_request_headers": "x-signature"
                    }
                })
                .to_string(),
            ),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "{body:?}");
    let created: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&body)).expect("json");
    let plugin_id = created["id"].as_str().expect("id").to_owned();

    // Bind the custom row to the already-wired route: the plugin reference is a
    // UUID, so the engine resolves the row and uses its stored config.
    let (status, body) = status_of(
        &fixture.router,
        request(
            "PUT",
            &format!("{BASE}/routes/{}", wired.route),
            Some(
                serde_json::json!({
                    "upstream_id": wired.upstream,
                    "match": { "http": { "methods": ["GET"], "path": "/guarded" } },
                    "plugins": { "items": [plugin_id] }
                })
                .to_string(),
            ),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body:?}");

    // Without the header the guard rejects the request.
    let (status, body) = status_of(
        &fixture.router,
        request("GET", &format!("{BASE}/proxy/target/guarded"), None),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{body:?}");
    hits.assert_calls(0);

    // With it the request is proxied.
    let mut req = request("GET", &format!("{BASE}/proxy/target/guarded"), None);
    req.headers_mut()
        .insert("x-signature", "abc".parse().expect("header"));
    let (status, _) = status_of(&fixture.router, req).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    hits.assert();
}

// ── D9: the response half of a transform plugin runs ────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn the_request_id_plugin_stamps_the_response_header() {
    let server = httpmock::MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/tracked");
        then.status(200).body("ok");
    });
    let fixture = build();
    let wired = wire(&fixture, &server, "/tracked", &["GET"]).await;

    let route = serde_json::json!({
        "upstream_id": wired.upstream,
        "match": { "http": { "methods": ["GET"], "path": "/tracked" } },
        "plugins": { "items": [REQUEST_ID_PLUGIN] }
    });
    let (status, body) = status_of(
        &fixture.router,
        request("PUT", &format!("{BASE}/routes/{}", wired.route), Some(route.to_string())),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body:?}");

    let response = fixture
        .router
        .clone()
        .oneshot(request("GET", &format!("{BASE}/proxy/target/tracked"), None))
        .await
        .expect("response");
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let echoed = response
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .expect("response correlation header");
    assert!(!echoed.is_empty());

    // An inbound correlation id is echoed back when forwarding is enabled.
    let route = serde_json::json!({
        "upstream_id": wired.upstream,
        "match": { "http": { "methods": ["GET"], "path": "/tracked" } },
        "plugins": { "items": [serde_json::json!({
            "ref": REQUEST_ID_PLUGIN,
            "config": { "forward_incoming": true }
        })] }
    });
    let _ = route;
    let mut req = request("GET", &format!("{BASE}/proxy/target/tracked"), None);
    req.headers_mut()
        .insert("x-request-id", "inbound-1".parse().expect("header"));
    let response = fixture.router.clone().oneshot(req).await.expect("response");
    let echoed = response
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .expect("response correlation header");
    assert_ne!(echoed, "inbound-1", "the binding carries no config object");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_response_body_keeps_streaming_through_a_transform() {
    let server = httpmock::MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/long");
        then.status(200).body("chunk-one");
    });
    let fixture = build();
    let wired = wire(&fixture, &server, "/long", &["GET"]).await;
    let route = serde_json::json!({
        "upstream_id": wired.upstream,
        "match": { "http": { "methods": ["GET"], "path": "/long" } },
        "plugins": { "items": [REQUEST_ID_PLUGIN] }
    });
    let (status, _) = status_of(
        &fixture.router,
        request("PUT", &format!("{BASE}/routes/{}", wired.route), Some(route.to_string())),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);

    let (status, body) = status_of(
        &fixture.router,
        request("GET", &format!("{BASE}/proxy/target/long"), None),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(String::from_utf8_lossy(&body), "chunk-one");
}

// ── Header policy end to end ────────────────────────────────────────────
// `headers.rs` covers the rule algebra with unit tests; these pin the
// plumbing: that the configured rules are actually the ones the upstream and
// the client observe.

#[tokio::test(flavor = "multi_thread")]
async fn hop_by_hop_headers_are_stripped_in_both_directions() {
    let server = httpmock::MockServer::start();
    let hits = server.mock(|when, then| {
        when.method(GET).path("/hop")
            // The inbound hop's connection state never reaches the upstream.
            .header_missing("connection")
            .header_missing("keep-alive")
            .header_missing("te")
            .header_missing("trailer")
            .header_missing("transfer-encoding")
            .header_missing("proxy-authorization")
            .header_missing("upgrade")
            // Headers named in `Connection` are hop-by-hop too.
            .header_missing("x-drop-me");
        then.status(200)
            .header("connection", "keep-alive")
            .header("keep-alive", "timeout=5")
            .header("trailer", "x-checksum")
            .header("x-public", "yes")
            .body("ok");
    });
    let fixture = build();
    wire_upstream_with(
        &fixture,
        &server,
        "/hop",
        &["GET"],
        serde_json::json!({ "headers": { "response": { "passthrough": "all" } } }),
    )
    .await;

    let mut req = request("GET", &format!("{BASE}/proxy/target/hop"), None);
    for (name, value) in [
        ("connection", "keep-alive, x-drop-me"),
        ("x-drop-me", "1"),
        ("keep-alive", "timeout=5"),
        ("te", "trailers"),
        ("trailer", "x-checksum"),
        ("transfer-encoding", "chunked"),
        ("proxy-authorization", "Basic Zm9vOmJhcg=="),
        ("upgrade", "websocket"),
    ] {
        req.headers_mut()
            .insert(name, value.parse().expect("header"));
    }
    let response = fixture.router.clone().oneshot(req).await.expect("response");
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.expect("body").to_bytes();
    assert_eq!(String::from_utf8_lossy(&body), "ok");
    for name in ["connection", "keep-alive", "trailer", "transfer-encoding"] {
        assert!(
            headers.get(name).is_none(),
            "{name} must not cross the hop back to the client"
        );
    }
    assert_eq!(headers.get("x-public").and_then(|v| v.to_str().ok()), Some("yes"));
    hits.assert();
}

#[tokio::test(flavor = "multi_thread")]
async fn request_header_rules_reach_the_upstream() {
    let server = httpmock::MockServer::start();
    let hits = server.mock(|when, then| {
        when.method(GET)
            .path("/rules")
            .header("x-set", "overridden")
            // `add` appends without displacing the value already there.
            .header("x-add", "first")
            // `remove` takes the inbound header out before passthrough.
            .header_missing("x-secret")
            // The allowlist is what is forwarded, and nothing else.
            .header("x-tenant", "acme")
            .header_missing("x-unlisted");
        then.status(200).body("ok");
    });
    let fixture = build();
    wire_upstream_with(
        &fixture,
        &server,
        "/rules",
        &["GET"],
        serde_json::json!({
            "headers": {
                "request": {
                    "passthrough": "allowlist",
                    "passthrough_allowlist": ["x-tenant", "x-doomed"],
                    "set": { "x-set": "overridden" },
                    "add": { "x-add": "first" },
                    "remove": ["x-doomed"]
                }
            }
        }),
    )
    .await;

    let mut req = request("GET", &format!("{BASE}/proxy/target/rules"), None);
    req.headers_mut().insert("x-tenant", "acme".parse().expect("header"));
    req.headers_mut().insert("x-unlisted", "no".parse().expect("header"));
    req.headers_mut().insert("x-doomed", "gone".parse().expect("header"));
    let (status, body) = status_of(&fixture.router, req).await;
    assert_eq!(
        status,
        axum::http::StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    hits.assert();
}

#[tokio::test(flavor = "multi_thread")]
async fn response_header_rules_reach_the_client() {
    let server = httpmock::MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/resp");
        then.status(200)
            .header("x-drop-me", "internal")
            .header("x-keep", "upstream")
            .body("ok");
    });
    let fixture = build();
    wire_upstream_with(
        &fixture,
        &server,
        "/resp",
        &["GET"],
        serde_json::json!({
            "headers": {
                "response": {
                    "passthrough": "all",
                    "set": { "x-stamped": "gateway" },
                    "add": { "x-added": "1" },
                    "remove": ["x-drop-me"]
                }
            }
        }),
    )
    .await;

    let response = fixture
        .router
        .clone()
        .oneshot(request("GET", &format!("{BASE}/proxy/target/resp"), None))
        .await
        .expect("response");
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let headers = response.headers().clone();
    assert_eq!(headers.get("x-stamped").and_then(|v| v.to_str().ok()), Some("gateway"));
    assert_eq!(headers.get("x-added").and_then(|v| v.to_str().ok()), Some("1"));
    assert_eq!(headers.get("x-keep").and_then(|v| v.to_str().ok()), Some("upstream"));
    assert!(
        headers.get("x-drop-me").is_none(),
        "a removed response header must not reach the client"
    );
}

/// The upstream's own response headers are part of the response the client
/// asked for; DESIGN §"Headers Transformation" only strips *inbound* headers,
/// and the wire schema's `headers.response` object has no passthrough switch
/// at all.
#[tokio::test]
async fn upstream_response_headers_reach_the_client_by_default() {
    let server = httpmock::MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/typed");
        then.status(200)
            .header("content-type", "application/json")
            .header("x-request-id", "upstream-1")
            .body(r#"{"ok":true}"#);
    });
    let fixture = build();
    wire(&fixture, &server, "/typed", &["GET"]).await;

    let response = fixture
        .router
        .clone()
        .oneshot(request("GET", &format!("{BASE}/proxy/target/typed"), None))
        .await
        .expect("response");
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let headers = response.headers().clone();
    assert_eq!(
        headers.get("content-type").and_then(|v| v.to_str().ok()),
        Some("application/json"),
        "the upstream content-type was dropped"
    );
    assert_eq!(
        headers.get("x-request-id").and_then(|v| v.to_str().ok()),
        Some("upstream-1")
    );
}

// ── Query and path matching ─────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn query_parameters_follow_the_route_allowlist() {
    let server = httpmock::MockServer::start();
    let hits = server.mock(|when, then| {
        when.method(GET)
            .path("/v1/models")
            .query_param("api-version", "2024-01");
        then.status(200).body("ok");
    });
    let fixture = build();
    let wired = wire(&fixture, &server, "/v1/models", &["GET"]).await;
    // Re-configure the route to allow exactly one query parameter.
    let (status, body) = status_of(
        &fixture.router,
        request(
            "PUT",
            &format!("{BASE}/routes/{}", wired.route),
            Some(
                serde_json::json!({
                    "upstream_id": wired.upstream,
                    "match": {
                        "http": {
                            "methods": ["GET"],
                            "path": "/v1/models",
                            "query_allowlist": ["api-version"]
                        }
                    }
                })
                .to_string(),
            ),
        ),
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );

    // A listed parameter is forwarded verbatim.
    let (status, body) = status_of(
        &fixture.router,
        request(
            "GET",
            &format!("{BASE}/proxy/target/v1/models?api-version=2024-01"),
            None,
        ),
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    hits.assert();

    // An unlisted parameter makes the route not match at all.
    let (status, _) = status_of(
        &fixture.router,
        request(
            "GET",
            &format!("{BASE}/proxy/target/v1/models?api-version=1&prompt=leak"),
            None,
        ),
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::NOT_FOUND,
        "an unlisted query parameter must not match the route"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_query_allowlist_rejects_any_query() {
    let server = httpmock::MockServer::start();
    let hits = server.mock(|when, then| {
        when.method(GET).path("/v1/models");
        then.status(200).body("ok");
    });
    let fixture = build();
    wire(&fixture, &server, "/v1/models", &["GET"]).await;

    let (status, _) = status_of(
        &fixture.router,
        request("GET", &format!("{BASE}/proxy/target/v1/models"), None),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    hits.assert_calls(1);

    // No allowlist entry means no query parameter is permitted.
    let (status, _) = status_of(
        &fixture.router,
        request(
            "GET",
            &format!("{BASE}/proxy/target/v1/models?api-version=1"),
            None,
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
    hits.assert_calls(1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_path_suffix_is_appended_by_default() {
    let server = httpmock::MockServer::start();
    let hits = server.mock(|when, then| {
        when.method(GET).path("/v1/chat/deep/leaf");
        then.status(200).body("ok");
    });
    let fixture = build();
    wire(&fixture, &server, "/v1/chat", &["GET"]).await;

    let (status, body) = status_of(
        &fixture.router,
        request("GET", &format!("{BASE}/proxy/target/v1/chat/deep/leaf"), None),
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    hits.assert();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_disabled_path_suffix_is_not_forwarded() {
    let server = httpmock::MockServer::start();
    let hits = server.mock(|when, then| {
        when.method(GET).path("/v1/chat");
        then.status(200).body("ok");
    });
    let fixture = build();
    let wired = wire(&fixture, &server, "/v1/chat", &["GET"]).await;
    let (status, body) = status_of(
        &fixture.router,
        request(
            "PUT",
            &format!("{BASE}/routes/{}", wired.route),
            Some(
                serde_json::json!({
                    "upstream_id": wired.upstream,
                    "match": {
                        "http": {
                            "methods": ["GET"],
                            "path": "/v1/chat",
                            "path_suffix_mode": "disabled"
                        }
                    }
                })
                .to_string(),
            ),
        ),
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );

    // The bare path still matches.
    let (status, body) = status_of(
        &fixture.router,
        request("GET", &format!("{BASE}/proxy/target/v1/chat"), None),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    hits.assert_calls(1);

    // …and a suffix no longer matches the route, so nothing is forwarded.
    let (status, _) = status_of(
        &fixture.router,
        request("GET", &format!("{BASE}/proxy/target/v1/chat/extra"), None),
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::NOT_FOUND,
        "a disabled path suffix was forwarded"
    );
    hits.assert_calls(1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_multi_method_route_accepts_every_listed_method() {
    for method in ["GET", "POST", "DELETE"] {
        let server = httpmock::MockServer::start();
        let hits = server.mock(|when, then| {
            when.method(method).path("/v1/items");
            then.status(200).body("ok");
        });
        let fixture = build();
        wire(&fixture, &server, "/v1/items", &["GET", "POST", "DELETE"]).await;
        let (status, body) = status_of(
            &fixture.router,
            request(
                method,
                &format!("{BASE}/proxy/target/v1/items"),
                if method == "POST" { Some("{}".into()) } else { None },
            ),
        )
        .await;
        assert_eq!(
            status,
            axum::http::StatusCode::OK,
            "{method}: {}",
            String::from_utf8_lossy(&body)
        );
        hits.assert();
    }
}

// ── Rate limiting ───────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn an_upstream_rate_limit_is_enforced_and_keyed_per_client_ip() {
    let server = httpmock::MockServer::start();
    let hits = server.mock(|when, then| {
        when.method(GET).path("/capped");
        then.status(200).body("ok");
    });
    let fixture = build();
    wire_upstream_with(
        &fixture,
        &server,
        "/capped",
        &["GET"],
        serde_json::json!({
            "rate_limit": {
                "burst": { "capacity": 1 },
                "sustained": { "rate": 1, "window": "second" },
                "scope": "ip"
            }
        }),
    )
    .await;

    async fn send_as(router: &Router, ip: &str) -> axum::http::StatusCode {
        let mut req = request("GET", &format!("{BASE}/proxy/target/capped"), None);
        req.headers_mut()
            .insert("x-forwarded-for", ip.parse().expect("header"));
        let response = router.clone().oneshot(req).await.expect("response");
        response.status()
    }

    // The first call from each address is admitted; the second is not.
    assert_eq!(send_as(&fixture.router, "203.0.113.1").await, axum::http::StatusCode::OK);
    assert_eq!(
        send_as(&fixture.router, "203.0.113.1").await,
        axum::http::StatusCode::TOO_MANY_REQUESTS
    );
    // A different address has its own bucket.
    assert_eq!(send_as(&fixture.router, "203.0.113.2").await, axum::http::StatusCode::OK);
    hits.assert_calls(2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_429_reports_when_the_bucket_resets() {
    let server = httpmock::MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/reset");
        then.status(200).body("ok");
    });
    let fixture = build();
    let wired = wire(&fixture, &server, "/reset", &["GET"]).await;
    let (status, body) = status_of(
        &fixture.router,
        request(
            "PUT",
            &format!("{BASE}/routes/{}", wired.route),
            Some(
                serde_json::json!({
                    "upstream_id": wired.upstream,
                    "match": { "http": { "methods": ["GET"], "path": "/reset" } },
                    "rate_limit": {
                        "burst": { "capacity": 1 },
                        "sustained": { "rate": 60, "window": "minute" }
                    }
                })
                .to_string(),
            ),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{}", String::from_utf8_lossy(&body));

    let _ = status_of(
        &fixture.router,
        request("GET", &format!("{BASE}/proxy/target/reset"), None),
    )
    .await;
    let response = fixture
        .router
        .clone()
        .oneshot(request("GET", &format!("{BASE}/proxy/target/reset"), None))
        .await
        .expect("response");
    assert_eq!(response.status(), axum::http::StatusCode::TOO_MANY_REQUESTS);
    let headers = response.headers().clone();
    let reset = headers
        .get("x-ratelimit-reset")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .expect("x-ratelimit-reset");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    // A 60/minute bucket that just emptied refills one token a second, so the
    // reset is in the future but inside the sustained window.
    assert!(
        reset > now && reset <= now + 61,
        "x-ratelimit-reset={reset} is not within the sustained window (now={now})"
    );
    assert_eq!(headers.get("x-ratelimit-limit").and_then(|v| v.to_str().ok()), Some("1"));
}

// ── Timeouts and streaming ──────────────────────────────────────────────

/// A raw TCP listener that accepts and then goes quiet.
async fn silent_upstream() -> (u16, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream listener");
    let port = listener.local_addr().expect("addr").port();
    let handle = tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else { break };
            // Read the request head and then never answer.
            tokio::spawn(async move {
                let mut socket = socket;
                let mut buf = [0u8; 4096];
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    socket.read(&mut buf),
                )
                .await;
                // Hold the connection open without writing anything.
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            });
        }
    });
    (port, handle)
}

#[tokio::test(flavor = "multi_thread")]
async fn proxy_timeout_secs_bounds_the_upstream_call() {
    let (port, _upstream) = silent_upstream().await;
    let cfg = OagwConfig {
        allow_http_upstream: true,
        proxy_timeout_secs: 1,
        connect_timeout_secs: 1,
        ..OagwConfig::default()
    };
    let fixture = build_with(cfg);
    let upstream = create_upstream(
        &fixture,
        serde_json::json!({
            "enabled": true,
            "alias": "target",
            "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": port } ] }
        }),
    )
    .await;
    create_route(
        &fixture,
        serde_json::json!({
            "upstream_id": upstream,
            "match": { "http": { "methods": ["GET"], "path": "/slow" } }
        }),
    )
    .await;

    let started = std::time::Instant::now();
    let (status, body) = status_of(
        &fixture.router,
        request("GET", &format!("{BASE}/proxy/target/slow"), None),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::GATEWAY_TIMEOUT, "{body:?}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(8),
        "the call took {:?}, far beyond proxy_timeout_secs=1",
        started.elapsed()
    );
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("timeout.request"), "body: {text}");
}

/// A raw TCP listener that writes an SSE response in two instalments. It
/// writes the first event, then waits for `gate` before writing the rest, so a
/// buffering gateway could never satisfy the reader.
async fn sse_upstream(gate: Arc<tokio::sync::Notify>) -> (u16, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream listener");
    let port = listener.local_addr().expect("addr").port();
    let handle = tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else { break };
            let gate = Arc::clone(&gate);
            tokio::spawn(async move {
                let mut socket = socket;
                let mut buf = [0u8; 8192];
                let _ = socket.read(&mut buf).await;
                let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                            transfer-encoding: chunked\r\n\r\n";
                if socket.write_all(head.as_bytes()).await.is_err() {
                    return;
                }
                // Chunk 1 goes out immediately…
                if socket.write_all(b"b\r\ndata: one\n\n\r\n").await.is_err() {
                    return;
                }
                let _ = socket.flush().await;
                // …and chunk 2 only once the client has confirmed it saw
                // chunk 1.
                gate.notified().await;
                if socket.write_all(b"17\r\ndata: two\ndata: three\n\n\r\n").await.is_err() {
                    return;
                }
                let _ = socket.write_all(b"0\r\n\r\n").await;
                let _ = socket.flush().await;
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    (port, handle)
}

#[tokio::test(flavor = "multi_thread")]
async fn an_sse_response_is_forwarded_incrementally() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let (port, _upstream) = sse_upstream(Arc::clone(&gate)).await;
    let fixture = build();
    let upstream = create_upstream(
        &fixture,
        serde_json::json!({
            "enabled": true,
            "alias": "target",
            "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": port } ] }
        }),
    )
    .await;
    create_route(
        &fixture,
        serde_json::json!({
            "upstream_id": upstream,
            "match": { "http": { "methods": ["GET"], "path": "/stream" } }
        }),
    )
    .await;
    let (addr, _server) = serve(&fixture).await;

    let mut client = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let request = format!(
        "GET {BASE}/proxy/target/stream HTTP/1.1\r\nhost: {addr}\r\nconnection: close\r\n\r\n"
    );
    client.write_all(request.as_bytes()).await.expect("write request");

    let mut buffer = Vec::new();
    // The first event arrives while the upstream is still blocked on the gate:
    // a gateway that buffered the whole body could not deliver it at all.
    assert!(read_until(&mut client, &mut buffer, "data: one").await, "no first event");
    assert!(
        buffer
            .windows(b"HTTP/1.1 200 OK".len())
            .any(|window| window == b"HTTP/1.1 200 OK"),
        "no response head: {}",
        String::from_utf8_lossy(&buffer)
    );
    gate.notify_one();
    assert!(
        read_until(&mut client, &mut buffer, "data: three").await,
        "no second event"
    );

    let text = String::from_utf8_lossy(&buffer).to_string();
    assert!(text.contains("data: one"), "body: {text}");
    assert!(text.contains("data: two"), "body: {text}");
    assert!(text.contains("data: three"), "body: {text}");
}

// ── Error source distinction (ADR-0007) ─────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn gateway_generated_errors_are_marked_as_gateway() {
    let fixture = build();
    let response = fixture
        .router
        .clone()
        .oneshot(request("GET", &format!("{BASE}/proxy/nobody/nothing"), None))
        .await
        .expect("response");
    assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
    let source = response
        .headers()
        .get("x-oagw-error-source")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    assert_eq!(source.as_deref(), Some("gateway"));
}

#[tokio::test]
async fn an_upstream_success_response_carries_the_error_source_header() {
    let server = httpmock::MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/fine");
        then.status(200).body("ok");
    });
    let fixture = build();
    wire(&fixture, &server, "/fine", &["GET"]).await;

    let response = fixture
        .router
        .clone()
        .oneshot(request("GET", &format!("{BASE}/proxy/target/fine"), None))
        .await
        .expect("response");
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let source = response
        .headers()
        .get("x-oagw-error-source")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    // ADR-0007 §Decision rules: `gateway` marks a response OAGW produced; this
    // one came from the upstream, so it is stamped `upstream` — success and
    // failure both carry the header.
    assert_eq!(
        source.as_deref(),
        Some("upstream"),
        "ADR-0007: a proxied response names its origin"
    );
}

/// ADR-0007 §Confirmation: "upstream errors include
/// `X-OAGW-Error-Source: upstream`".
#[tokio::test]
async fn an_upstream_error_is_marked_as_upstream() {
    let server = httpmock::MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/boom");
        then.status(503)
            .header("content-type", "application/json")
            .body(r#"{"error":{"message":"upstream is sad"}}"#);
    });
    let fixture = build();
    wire(&fixture, &server, "/boom", &["GET"]).await;

    let response = fixture
        .router
        .clone()
        .oneshot(request("GET", &format!("{BASE}/proxy/target/boom"), None))
        .await
        .expect("response");
    assert_eq!(response.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.expect("body").to_bytes();
    // The upstream body is passed through untouched.
    assert_eq!(
        String::from_utf8_lossy(&body),
        r#"{"error":{"message":"upstream is sad"}}"#
    );
    assert_eq!(
        headers.get("x-oagw-error-source").and_then(|v| v.to_str().ok()),
        Some("upstream"),
        "headers: {:?}",
        headers
    );
}

/// ADR-0009 §Decision Flow: "Response phase → status 502, error_code
/// REQUIRED_HEADER_MISSING".
#[tokio::test]
async fn a_response_missing_a_required_header_is_a_502() {
    let server = httpmock::MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/unsigned");
        then.status(200).body("ok");
    });
    let fixture = build();
    let wired = wire(&fixture, &server, "/unsigned", &["GET"]).await;

    let (status, body) = status_of(
        &fixture.router,
        request(
            "POST",
            &format!("{BASE}/plugins"),
            Some(
                serde_json::json!({
                    "plugin_type": "guard",
                    "name": "response-signature",
                    "source": "function guard(ctx, req) { return true; }",
                    "config": {
                        "type": "required_headers",
                        "required_response_headers": "x-signature"
                    }
                })
                .to_string(),
            ),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "{body:?}");
    let created: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&body)).expect("json");
    let plugin_id = created["id"].as_str().expect("id").to_owned();

    let (status, body) = status_of(
        &fixture.router,
        request(
            "PUT",
            &format!("{BASE}/routes/{}", wired.route),
            Some(
                serde_json::json!({
                    "upstream_id": wired.upstream,
                    "match": { "http": { "methods": ["GET"], "path": "/unsigned" } },
                    "plugins": { "items": [plugin_id] }
                })
                .to_string(),
            ),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body:?}");

    let (status, body) = status_of(
        &fixture.router,
        request("GET", &format!("{BASE}/proxy/target/unsigned"), None),
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::BAD_GATEWAY,
        "an unsigned response was passed through: {body:?}"
    );
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("required_header.missing"), "body: {text}");
}

/// ADR-0009: "Only the first missing header is reported per rejection."
#[tokio::test(flavor = "multi_thread")]
async fn a_required_headers_guard_reports_only_the_first_missing_header() {
    let server = httpmock::MockServer::start();
    let hits = server.mock(|when, then| {
        when.method(GET).path("/signed");
        then.status(200).body("ok");
    });
    let fixture = build();
    let wired = wire(&fixture, &server, "/signed", &["GET"]).await;
    bind_guard(
        &fixture,
        &wired,
        "/signed",
        serde_json::json!({ "type": "required_headers", "required": ["x-first", "x-second"] }),
    )
    .await;

    let (status, body) = status_of(
        &fixture.router,
        request("GET", &format!("{BASE}/proxy/target/signed"), None),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{body:?}");
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("required_header.missing"), "body: {text}");
    let problem: serde_json::Value = serde_json::from_str(&text).expect("problem body");
    assert_eq!(problem["context"]["header"], "x-first", "body: {text}");
    hits.assert_calls(0);

    // Supplying the first header moves the report to the second one.
    let mut req = request("GET", &format!("{BASE}/proxy/target/signed"), None);
    req.headers_mut().insert("x-first", "1".parse().expect("header"));
    let (status, body) = status_of(&fixture.router, req).await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{body:?}");
    let problem: serde_json::Value =
        serde_json::from_slice(&body).expect("problem body");
    assert_eq!(problem["context"]["header"], "x-second", "{problem}");

    // Both present: the request is proxied.
    let mut req = request("GET", &format!("{BASE}/proxy/target/signed"), None);
    req.headers_mut().insert("x-first", "1".parse().expect("header"));
    req.headers_mut().insert("x-second", "2".parse().expect("header"));
    let (status, _) = status_of(&fixture.router, req).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    hits.assert();
}

/// ADR-0009: "Absent or blank → Allow (fail-open, unconfigured)." A guard
/// whose configuration covers only the response phase must not reject
/// requests.
#[tokio::test(flavor = "multi_thread")]
async fn a_guard_without_request_rules_fails_open() {
    let server = httpmock::MockServer::start();
    // The upstream signs its response, so the guard's *response* phase (the
    // only phase it configures) is satisfied and the assertion below isolates
    // the request phase: with no `required_request_headers`, the request must
    // still be proxied (ADR-0009 fail-open, per phase).
    let hits = server.mock(|when, then| {
        when.method(GET).path("/open");
        then.status(200).header("x-signature", "1").body("ok");
    });
    let fixture = build();
    let wired = wire(&fixture, &server, "/open", &["GET"]).await;
    bind_guard(
        &fixture,
        &wired,
        "/open",
        serde_json::json!({
            "type": "required_headers",
            "required_response_headers": "x-signature"
        }),
    )
    .await;

    let (status, body) = status_of(
        &fixture.router,
        request("GET", &format!("{BASE}/proxy/target/open"), None),
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::OK,
        "a guard with no request-phase rules rejected the request: {body:?}"
    );
    hits.assert();
}

/// Bind a `required_headers` guard row to `wired`'s route.
async fn bind_guard(
    fixture: &Fixture,
    wired: &Wired,
    path: &str,
    config: serde_json::Value,
) -> String {
    let (status, body) = status_of(
        &fixture.router,
        request(
            "POST",
            &format!("{BASE}/plugins"),
            Some(
                serde_json::json!({
                    "plugin_type": "guard",
                    "name": "guard-under-test",
                    "source": "function guard(ctx, req) { return true; }",
                    "config": config
                })
                .to_string(),
            ),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "{body:?}");
    let created: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&body)).expect("json");
    let plugin_id = created["id"].as_str().expect("id").to_owned();

    let (status, body) = status_of(
        &fixture.router,
        request(
            "PUT",
            &format!("{BASE}/routes/{}", wired.route),
            Some(
                serde_json::json!({
                    "upstream_id": wired.upstream,
                    "match": { "http": { "methods": ["GET"], "path": path } },
                    "plugins": { "items": [plugin_id] }
                })
                .to_string(),
            ),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body:?}");
    plugin_id
}

/// ADR-0002 binds custom plugin rows to a built-in implementation. A row
/// whose config carries no discriminator has to be recognised by its
/// configuration shape (`infer_builtin_from_config`), which is only reachable
/// when the row's own name does not resemble a built-in plugin name.
#[tokio::test(flavor = "multi_thread")]
async fn a_freely_named_plugin_row_is_recognised_by_its_config_shape() {
    let server = httpmock::MockServer::start();
    let hits = server.mock(|when, then| {
        when.method(GET).path("/shape");
        then.status(200).body("ok");
    });
    let fixture = build();
    let wired = wire(&fixture, &server, "/shape", &["GET"]).await;
    // No `type` discriminator: the row is identified by its config alone.
    bind_guard(
        &fixture,
        &wired,
        "/shape",
        serde_json::json!({ "required": ["x-signature"] }),
    )
    .await;

    let (status, body) = status_of(
        &fixture.router,
        request("GET", &format!("{BASE}/proxy/target/shape"), None),
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::BAD_REQUEST,
        "a freely named guard row configured by shape was ignored: {body:?}"
    );
    hits.assert_calls(0);

    let mut req = request("GET", &format!("{BASE}/proxy/target/shape"), None);
    req.headers_mut().insert("x-signature", "1".parse().expect("header"));
    let (status, _) = status_of(&fixture.router, req).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    hits.assert();
}

// ── Alias resolution at the data plane ──────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn alias_resolution_is_case_insensitive() {
    let server = httpmock::MockServer::start();
    let hits = server.mock(|when, then| {
        when.method(GET).path("/v1/models");
        then.status(200).body("ok");
    });
    let fixture = build();
    wire(&fixture, &server, "/v1/models", &["GET"]).await;

    let (status, body) = status_of(
        &fixture.router,
        request("GET", &format!("{BASE}/proxy/Target/v1/models"), None),
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::OK,
        "an upper-cased alias did not resolve: {}",
        String::from_utf8_lossy(&body)
    );
    hits.assert();
}

/// DESIGN §"Route" REST semantics: "`upstream_id` is immutable".
#[tokio::test(flavor = "multi_thread")]
async fn a_route_cannot_be_retargeted_by_its_put() {
    let target_a = httpmock::MockServer::start();
    let hits_a = target_a.mock(|when, then| {
        when.method(GET).path("/v1");
        then.status(200).body("from-a");
    });
    let target_b = httpmock::MockServer::start();
    let hits_b = target_b.mock(|when, then| {
        when.method(GET).path("/v1");
        then.status(200).body("from-b");
    });
    let fixture = build();
    let original = create_upstream(
        &fixture,
        serde_json::json!({
            "enabled": true,
            "alias": "first",
            "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": server_port(&target_a) } ] }
        }),
    )
    .await;
    let other = create_upstream(
        &fixture,
        serde_json::json!({
            "enabled": true,
            "alias": "second",
            "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": server_port(&target_b) } ] }
        }),
    )
    .await;
    let route = create_route(
        &fixture,
        serde_json::json!({
            "upstream_id": original,
            "match": { "http": { "methods": ["GET"], "path": "/v1" } }
        }),
    )
    .await;

    // Try to re-point the route at the other upstream.
    let (status, body) = status_of(
        &fixture.router,
        request(
            "PUT",
            &format!("{BASE}/routes/{route}"),
            Some(
                serde_json::json!({
                    "upstream_id": other,
                    "match": { "http": { "methods": ["GET"], "path": "/v1" } }
                })
                .to_string(),
            ),
        ),
    )
    .await;
    assert!(
        status == axum::http::StatusCode::BAD_REQUEST || status == axum::http::StatusCode::CONFLICT,
        "the immutable upstream_id was accepted: {status} {body:?}"
    );

    // And the route still serves the upstream it was created with.
    let (status, body) = status_of(
        &fixture.router,
        request("GET", &format!("{BASE}/proxy/first/v1"), None),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body:?}");
    assert_eq!(String::from_utf8_lossy(&body), "from-a");

    // …while the upstream the route was never pointed at stays idle.
    hits_b.assert_calls(0);
    hits_a.assert();
}
