//! Data-plane integration tests: HTTP/SSE/WebSocket proxy round-trips,
//! rate limiting, CORS, error mapping and plugin execution through the real
//! [`DataPlaneService`] + [`HyperProxyEngine`] stack.
//!
//! Upstreams are either httpmock mocks (HTTP/SSE) or minimal raw-TCP servers
//! (WebSocket).  The gateway-side WebSocket test runs a real axum server so
//! `hyper::upgrade::on` can see a live client connection.

#![allow(clippy::unwrap_used, clippy::expect_used, reason = "integration tests")]

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::{Extension, Path, Request};
use axum::http::{HeaderMap, HeaderValue, Method};
use axum::response::Response;
use http_body_util::BodyExt;
use httpmock::prelude::*;
use oagw::config::OagwConfig;
use oagw::domain::data_plane::DataPlaneService;
use oagw::domain::dto::*;
use oagw::domain::error;
use oagw::domain::hierarchy::FlatTenantHierarchy;
use oagw::domain::repo::OagwRepository;
use oagw::infra::plugin::build_registries;
use oagw::infra::proxy::{HyperProxyEngine, ProxyEngine};
use oagw::infra::storage::InMemoryRepository;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use toolkit_security::SecurityContext;
use toolkit_security::context::SecurityContextBuilder;
use uuid::Uuid;

use credstore_sdk::test_util::MockCredStoreClient;

const TENANT: &str = "11111111-1111-1111-1111-111111111111";

fn tenant() -> Uuid {
    Uuid::parse_str(TENANT).unwrap()
}

fn ctx() -> SecurityContext {
    SecurityContextBuilder::default()
        .subject_id(tenant())
        .subject_type("user")
        .subject_tenant_id(tenant())
        .token_scopes(vec!["*".to_owned()])
        .build()
        .unwrap()
}

fn base_config() -> OagwConfig {
    OagwConfig {
        proxy_timeout_secs: 5,
        allow_http_upstream: true,
        ..Default::default()
    }
}

fn build_service(
    repo: Arc<dyn OagwRepository>,
    config: &OagwConfig,
    credstore: Option<&Arc<dyn credstore_sdk::CredStoreClientV1>>,
) -> Arc<DataPlaneService> {
    let (auth, guard, transform) = build_registries(config, credstore);
    let engine: Arc<dyn ProxyEngine> = Arc::new(HyperProxyEngine::new(config.proxy_timeout_secs));
    Arc::new(DataPlaneService::new(
        repo,
        Arc::new(FlatTenantHierarchy),
        auth,
        guard,
        transform,
        engine,
        config,
    ))
}

fn seed_repo() -> Arc<InMemoryRepository> {
    Arc::new(InMemoryRepository::new())
}

fn upstream(alias: &str, host: &str, port: u16) -> Upstream {
    Upstream {
        id: Uuid::new_v4(),
        tenant_id: tenant(),
        enabled: true,
        alias: alias.to_owned(),
        tags: vec![],
        server: ServerConfig {
            endpoints: vec![Endpoint {
                scheme: "http".to_owned(),
                host: host.to_owned(),
                port,
            }],
        },
        protocol: Protocol::Http,
        auth: AuthConfig::default(),
        headers: HeadersConfig::default(),
        plugins: PluginsConfig::default(),
        rate_limit: None,
        cors: None,
        created_at: 0,
    }
}

fn route(upstream_id: Uuid, path: &str) -> Route {
    Route {
        id: Uuid::new_v4(),
        tenant_id: tenant(),
        enabled: true,
        upstream_id,
        priority: 0,
        match_config: MatchConfig {
            http: Some(HttpMatch {
                methods: vec![
                    HttpMethod::Get,
                    HttpMethod::Post,
                    HttpMethod::Put,
                    HttpMethod::Patch,
                    HttpMethod::Delete,
                ],
                path: path.to_owned(),
                query_allowlist: vec![],
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        },
        tags: vec![],
        plugins: PluginsConfig::default(),
        rate_limit: None,
        cors: None,
        created_at: 0,
    }
}

fn endpoints_for(server: &MockServer) -> (String, u16) {
    (server.host(), server.port())
}

/// Run a full proxy call and return `(status, headers, body)`.
async fn call(
    svc: &DataPlaneService,
    method: Method,
    alias: &str,
    path: &str,
    query: Option<&str>,
    headers: HeaderMap,
    body: Vec<u8>,
) -> (u16, HeaderMap, Vec<u8>) {
    let uri = match query {
        Some(q) => format!("http://gateway/api/oagw/v1/proxy/{alias}{path}?{q}"),
        None => format!("http://gateway/api/oagw/v1/proxy/{alias}{path}"),
    };
    let mut req = Request::builder()
        .method(method.clone())
        .uri(uri)
        .body(Body::from(body))
        .unwrap();
    *req.headers_mut() = headers;
    let resp = svc.proxy(&ctx(), req, alias, path).await;
    let (parts, body) = resp.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes().to_vec();
    (parts.status.as_u16(), parts.headers, bytes)
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn http_round_trip_forwards_with_host_replacement() {
    let server = MockServer::start();
    let (host, port) = endpoints_for(&server);
    let mock = server.mock(|when, then| {
        when.method(GET).path("/v1/hello");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"ok":true}"#);
    });

    let repo = seed_repo();
    repo.insert_upstream(upstream("svc", &host, port));
    let up = repo.list_upstreams(tenant()).pop().unwrap();
    repo.insert_route(route(up.id, "/v1"));
    let svc = build_service(repo.clone(), &base_config(), None);

    let (status, headers, body) = call(
        &svc,
        Method::GET,
        "svc",
        "/v1/hello",
        None,
        HeaderMap::new(),
        vec![],
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(text(&body), r#"{"ok":true}"#);
    // ADR-0007: proxied responses are attributed to the upstream.
    assert_eq!(headers.get("x-oagw-error-source").unwrap(), "upstream");
    mock.assert();
}

#[tokio::test(flavor = "multi_thread")]
async fn hop_by_hop_headers_stripped_and_host_replaced_wire() {
    let server = MockServer::start();
    let (host, port) = endpoints_for(&server);
    let authority = format!("{host}:{port}");
    let mock = server.mock(|when, then| {
        when.method(GET)
            .path("/v1/hello")
            .header("host", authority.clone())
            // connection / keep-alive must not reach the upstream.
            .is_true(|req| !req.headers().contains_key("connection"))
            // Connection-nominated headers (RFC 9110 §7.6.1) are stripped too.
            .is_true(|req| !req.headers().contains_key("x-strip-me"))
            // Target-Host is a gateway-internal header; never forwarded.
            .is_true(|req| !req.headers().contains_key("x-oagw-target-host"));
        then.status(200).body("ok");
    });

    let repo = seed_repo();
    repo.insert_upstream(upstream("svc", &host, port));
    let up = repo.list_upstreams(tenant()).pop().unwrap();
    repo.insert_route(route(up.id, "/v1"));
    let svc = build_service(repo.clone(), &base_config(), None);

    let mut headers = HeaderMap::new();
    headers.insert(
        "connection",
        HeaderValue::from_static("keep-alive, X-Strip-Me"),
    );
    headers.insert("x-strip-me", HeaderValue::from_static("drop"));
    // Valid endpoint for a single-endpoint upstream → bare hostname (no port)
    // passes Target-Host validation, then must be stripped before the wire.
    headers.insert("x-oagw-target-host", HeaderValue::from_str(&host).unwrap());
    let (status, _, body) =
        call(&svc, Method::GET, "svc", "/v1/hello", None, headers, vec![]).await;
    assert_eq!(status, 200);
    assert_eq!(text(&body), "ok");
    mock.assert();
}

#[tokio::test(flavor = "multi_thread")]
async fn sse_response_is_streamed_chunk_by_chunk() {
    let server = MockServer::start();
    let (host, port) = endpoints_for(&server);
    let body = "data: one\n\ndata: two\n\ndata: three\n\n";
    server.mock(|when, then| {
        when.method(GET).path("/events");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(body);
    });

    let repo = seed_repo();
    repo.insert_upstream(upstream("sse", &host, port));
    let up = repo.list_upstreams(tenant()).pop().unwrap();
    repo.insert_route(route(up.id, "/events"));
    let svc = build_service(repo.clone(), &base_config(), None);

    let (status, headers, got) = call(
        &svc,
        Method::GET,
        "sse",
        "/events",
        None,
        HeaderMap::new(),
        vec![],
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(headers.get("content-type").unwrap(), "text/event-stream");
    // Streamed: no content-length, full SSE payload delivered.
    assert!(headers.get("content-length").is_none());
    assert_eq!(text(&got), body);
}

#[tokio::test(flavor = "multi_thread")]
async fn rate_limit_429_includes_headers() {
    let server = MockServer::start();
    let (host, port) = endpoints_for(&server);
    server.mock(|when, then| {
        when.method(GET).path("/v1/rl");
        then.status(200).body("limited");
    });

    let repo = seed_repo();
    let mut up = upstream("rl", &host, port);
    up.rate_limit = Some(RateLimitConfig {
        sharing: Sharing::Private,
        algorithm: RateLimitAlgorithm::TokenBucket,
        sustained_rate: 10,
        sustained_window: RateLimitWindow::Second,
        burst_capacity: 1,
        scope: RateLimitScope::Tenant,
        strategy: RateLimitStrategy::Reject,
        cost: 1,
        response_headers: true,
    });
    repo.insert_upstream(up);
    let up = repo.list_upstreams(tenant()).pop().unwrap();
    repo.insert_route(route(up.id, "/v1/rl"));
    let svc = build_service(repo.clone(), &base_config(), None);

    // First request admitted and carries X-RateLimit-* headers.
    let (status, headers, _) = call(
        &svc,
        Method::GET,
        "rl",
        "/v1/rl",
        None,
        HeaderMap::new(),
        vec![],
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(headers.get("x-ratelimit-limit").unwrap(), "10");
    assert_eq!(headers.get("x-ratelimit-remaining").unwrap(), "0"); // burst 1 consumed

    // Second request exceeds the bucket → 429 with Retry-After + headers.
    let (status, headers, body) = call(
        &svc,
        Method::GET,
        "rl",
        "/v1/rl",
        None,
        HeaderMap::new(),
        vec![],
    )
    .await;
    assert_eq!(status, 429);
    assert!(headers.get("retry-after").is_some());
    assert_eq!(headers.get("x-ratelimit-limit").unwrap(), "10");
    assert_eq!(headers.get("x-ratelimit-remaining").unwrap(), "0");
    assert_eq!(headers.get("x-oagw-error-source").unwrap(), "gateway");
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["type"], error::GTS_RATE_LIMIT_EXCEEDED);
    // ADR-0007 extension fields are snake_case.
    assert_eq!(body["retry_after_secs"], 1);
    assert!(body.get("retryAfterSecs").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn cors_preflight_returns_204_echo() {
    let repo = seed_repo();
    let svc = build_service(repo.clone(), &base_config(), None);

    let mut headers = HeaderMap::new();
    headers.insert("origin", HeaderValue::from_static("https://app.example"));
    headers.insert(
        "access-control-request-method",
        HeaderValue::from_static("GET"),
    );
    headers.insert(
        "access-control-request-headers",
        HeaderValue::from_static("x-request-id"),
    );
    let (status, resp_headers, _) = call(
        &svc,
        Method::OPTIONS,
        "nonexistent",
        "/x",
        None,
        headers,
        vec![],
    )
    .await;
    assert_eq!(status, 204);
    assert_eq!(
        resp_headers.get("access-control-allow-origin").unwrap(),
        "https://app.example"
    );
    assert_eq!(
        resp_headers.get("access-control-allow-methods").unwrap(),
        "GET"
    );
    assert_eq!(
        resp_headers.get("access-control-allow-headers").unwrap(),
        "x-request-id"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn preflight_bypasses_invoke_permission_check() {
    // ADR-0004: preflight short-circuits before the `proxy:invoke` permission
    // check and the plugin pipeline.  A caller with NO invoke permission still
    // gets the permissive 204 echo.
    let server = MockServer::start();
    let (host, port) = endpoints_for(&server);
    let repo = seed_repo();
    repo.insert_upstream(upstream("perm", &host, port));
    let up = repo.list_upstreams(tenant()).pop().unwrap();
    repo.insert_route(route(up.id, "/v1"));
    let svc = build_service(repo.clone(), &base_config(), None);

    let limited = SecurityContextBuilder::default()
        .subject_id(tenant())
        .subject_type("user")
        .subject_tenant_id(tenant())
        .token_scopes(vec!["gts.cf.core.oagw.upstream.v1~:read".to_owned()])
        .build()
        .unwrap();

    let mut headers = HeaderMap::new();
    headers.insert("origin", HeaderValue::from_static("https://app.example"));
    headers.insert(
        "access-control-request-method",
        HeaderValue::from_static("GET"),
    );
    let req = Request::builder()
        .method(Method::OPTIONS)
        .uri("http://gateway/api/oagw/v1/proxy/perm/v1")
        .body(Body::empty())
        .unwrap();
    let mut req = req;
    *req.headers_mut() = headers;
    let resp = svc.proxy(&limited, req, "perm", "/v1").await;
    let (parts, _) = resp.into_parts();
    assert_eq!(parts.status.as_u16(), 204);
    assert_eq!(
        parts.headers.get("access-control-allow-origin").unwrap(),
        "https://app.example"
    );

    // A REAL (non-preflight) request without invoke permission → 403.
    let mut headers = HeaderMap::new();
    headers.insert("origin", HeaderValue::from_static("https://app.example"));
    let req = Request::builder()
        .method(Method::GET)
        .uri("http://gateway/api/oagw/v1/proxy/perm/v1")
        .body(Body::empty())
        .unwrap();
    let mut req = req;
    *req.headers_mut() = headers;
    let resp = svc.proxy(&limited, req, "perm", "/v1").await;
    assert_eq!(resp.status().as_u16(), 403);
}

#[tokio::test(flavor = "multi_thread")]
async fn cors_actual_request_enforces_origin_and_method() {
    let server = MockServer::start();
    let (host, port) = endpoints_for(&server);
    let hits = server.mock(|when, then| {
        when.method(GET).path("/v1/cors");
        then.status(200).body("cors-ok");
    });

    let repo = seed_repo();
    let mut up = upstream("cors", &host, port);
    up.cors = Some(CorsConfig {
        sharing: Sharing::Inherit,
        enabled: true,
        allowed_origins: vec!["https://app.example".into()],
        allowed_methods: vec!["GET".into()],
        expose_headers: vec!["x-expose".into()],
        allow_credentials: false,
    });
    repo.insert_upstream(up);
    let up = repo.list_upstreams(tenant()).pop().unwrap();
    repo.insert_route(route(up.id, "/v1/cors"));
    let svc = build_service(repo.clone(), &base_config(), None);

    // Disallowed origin → 403 gateway problem (Vary: Origin still present).
    let mut headers = HeaderMap::new();
    headers.insert("origin", HeaderValue::from_static("https://evil.example"));
    let (status, resp_headers, body) =
        call(&svc, Method::GET, "cors", "/v1/cors", None, headers, vec![]).await;
    assert_eq!(status, 403);
    assert_eq!(resp_headers.get("x-oagw-error-source").unwrap(), "gateway");
    assert_eq!(resp_headers.get("vary").unwrap(), "Origin");
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["type"], error::GTS_CORS_ORIGIN_NOT_ALLOWED);
    hits.assert_calls(0);

    // Allowed origin → passthrough 200 with ADR-0004 CORS response headers:
    // ACAO echoes the origin, ACAEH exposes configured headers, Vary always.
    let mut headers = HeaderMap::new();
    headers.insert("origin", HeaderValue::from_static("https://app.example"));
    let (status, resp_headers, body) =
        call(&svc, Method::GET, "cors", "/v1/cors", None, headers, vec![]).await;
    assert_eq!(status, 200);
    assert_eq!(text(&body), "cors-ok");
    assert_eq!(
        resp_headers.get("access-control-allow-origin").unwrap(),
        "https://app.example"
    );
    assert_eq!(
        resp_headers.get("access-control-expose-headers").unwrap(),
        "x-expose"
    );
    let vary = resp_headers.get("vary").unwrap().to_str().unwrap();
    assert!(vary.split(',').any(|t| t.trim() == "Origin"));
    hits.assert();
}

#[tokio::test(flavor = "multi_thread")]
async fn vary_origin_always_present_even_without_cors() {
    let server = MockServer::start();
    let (host, port) = endpoints_for(&server);
    server.mock(|when, then| {
        when.method(GET).path("/v1/plain");
        then.status(200).body("plain");
    });

    let repo = seed_repo();
    repo.insert_upstream(upstream("plain", &host, port));
    let up = repo.list_upstreams(tenant()).pop().unwrap();
    repo.insert_route(route(up.id, "/v1/plain"));
    let svc = build_service(repo.clone(), &base_config(), None);

    // No CORS configured at all → Vary: Origin still emitted (ADR-0004 §Vary).
    let (status, resp_headers, body) = call(
        &svc,
        Method::GET,
        "plain",
        "/v1/plain",
        None,
        HeaderMap::new(),
        vec![],
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(text(&body), "plain");
    assert_eq!(resp_headers.get("vary").unwrap(), "Origin");
}

#[tokio::test(flavor = "multi_thread")]
async fn upstream_error_status_passed_through_with_source() {
    let server = MockServer::start();
    let (host, port) = endpoints_for(&server);
    server.mock(|when, then| {
        when.method(GET).path("/v1/bad");
        then.status(502)
            .header("content-type", "application/json")
            .body(r#"{"error":"upstream exploded"}"#);
    });

    let repo = seed_repo();
    repo.insert_upstream(upstream("bad", &host, port));
    let up = repo.list_upstreams(tenant()).pop().unwrap();
    repo.insert_route(route(up.id, "/v1/bad"));
    let svc = build_service(repo.clone(), &base_config(), None);

    let (status, headers, body) = call(
        &svc,
        Method::GET,
        "bad",
        "/v1/bad",
        None,
        HeaderMap::new(),
        vec![],
    )
    .await;
    assert_eq!(status, 502);
    // Passthrough: upstream body preserved, attributed as upstream error.
    assert_eq!(text(&body), r#"{"error":"upstream exploded"}"#);
    assert_eq!(headers.get("x-oagw-error-source").unwrap(), "upstream");
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_alias_and_route_rejections() {
    let server = MockServer::start();
    let (host, port) = endpoints_for(&server);
    let repo = seed_repo();
    let up = upstream("svc", &host, port);
    repo.insert_upstream(up);
    let up = repo.list_upstreams(tenant()).pop().unwrap();
    repo.insert_route(route(up.id, "/v1"));
    let svc = build_service(repo.clone(), &base_config(), None);

    // Unknown alias → 404 RouteNotFound (gateway).
    let (status, headers, body) = call(
        &svc,
        Method::GET,
        "nope",
        "/v1",
        None,
        HeaderMap::new(),
        vec![],
    )
    .await;
    assert_eq!(status, 404);
    assert_eq!(headers.get("x-oagw-error-source").unwrap(), "gateway");
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["type"], error::GTS_ROUTE_NOT_FOUND);

    // No matching route path → 404.
    let (status, _, _) = call(
        &svc,
        Method::GET,
        "svc",
        "/other",
        None,
        HeaderMap::new(),
        vec![],
    )
    .await;
    assert_eq!(status, 404);

    // Method not allowed on the route → 404.
    let (status, _, _) = call(
        &svc,
        Method::HEAD,
        "svc",
        "/v1",
        None,
        HeaderMap::new(),
        vec![],
    )
    .await;
    assert_eq!(status, 404);

    // Path suffix with path_suffix_mode=disabled → 400 validation.
    let repo2 = seed_repo();
    let up2 = upstream("svc2", &host, port);
    repo2.insert_upstream(up2);
    let up2 = repo2.list_upstreams(tenant()).pop().unwrap();
    let mut r2 = route(up2.id, "/v1");
    if let Some(h) = &mut r2.match_config.http {
        h.path_suffix_mode = PathSuffixMode::Disabled;
    }
    repo2.insert_route(r2);
    let svc2 = build_service(repo2, &base_config(), None);
    let (status, _, body) = call(
        &svc2,
        Method::GET,
        "svc2",
        "/v1/extra",
        None,
        HeaderMap::new(),
        vec![],
    )
    .await;
    assert_eq!(status, 400);
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["type"], error::GTS_VALIDATION_ERROR);
}

#[tokio::test(flavor = "multi_thread")]
async fn payload_too_large_maps_to_413() {
    let server = MockServer::start();
    let (host, port) = endpoints_for(&server);
    let repo = seed_repo();
    repo.insert_upstream(upstream("svc", &host, port));
    let up = repo.list_upstreams(tenant()).pop().unwrap();
    repo.insert_route(route(up.id, "/v1"));
    let config = OagwConfig {
        max_request_body_bytes: 4,
        allow_http_upstream: true,
        ..Default::default()
    };
    let svc = build_service(repo, &config, None);

    let (status, headers, body) = call(
        &svc,
        Method::POST,
        "svc",
        "/v1",
        None,
        HeaderMap::new(),
        b"hello world".to_vec(),
    )
    .await;
    assert_eq!(status, 413);
    assert_eq!(headers.get("x-oagw-error-source").unwrap(), "gateway");
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["type"], error::GTS_PAYLOAD_TOO_LARGE);
}

#[tokio::test(flavor = "multi_thread")]
async fn upstream_connect_failure_maps_to_503() {
    // Bind a listener then drop it so the port is closed → connect refused.
    let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_port = dead.local_addr().unwrap().port();
    drop(dead);

    let repo = seed_repo();
    repo.insert_upstream(upstream("svc", "127.0.0.1", dead_port));
    let up = repo.list_upstreams(tenant()).pop().unwrap();
    repo.insert_route(route(up.id, "/v1"));
    let svc = build_service(repo.clone(), &base_config(), None);

    let (status, headers, body) = call(
        &svc,
        Method::GET,
        "svc",
        "/v1",
        None,
        HeaderMap::new(),
        vec![],
    )
    .await;
    assert_eq!(status, 503);
    assert_eq!(headers.get("x-oagw-error-source").unwrap(), "gateway");
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["type"], error::GTS_LINK_UNAVAILABLE);
}

#[tokio::test(flavor = "multi_thread")]
async fn target_host_matrix_through_proxy() {
    // Derived common-suffix pool: no Target-Host ⇒ 400 missing.target.host
    // (fails before any network contact).
    let repo = seed_repo();
    let up = Upstream {
        id: Uuid::new_v4(),
        tenant_id: tenant(),
        enabled: true,
        alias: "vendor.com".to_owned(),
        tags: vec![],
        server: ServerConfig {
            endpoints: vec![
                Endpoint {
                    scheme: "http".into(),
                    host: "us.vendor.com".into(),
                    port: 80,
                },
                Endpoint {
                    scheme: "http".into(),
                    host: "eu.vendor.com".into(),
                    port: 80,
                },
            ],
        },
        protocol: Protocol::Http,
        auth: AuthConfig::default(),
        headers: HeadersConfig::default(),
        plugins: PluginsConfig::default(),
        rate_limit: None,
        cors: None,
        created_at: 0,
    };
    repo.insert_upstream(up);
    let up = repo.list_upstreams(tenant()).pop().unwrap();
    repo.insert_route(route(up.id, "/v1"));
    let svc = build_service(repo.clone(), &base_config(), None);

    let (status, _, body) = call(
        &svc,
        Method::GET,
        "vendor.com",
        "/v1",
        None,
        HeaderMap::new(),
        vec![],
    )
    .await;
    assert_eq!(status, 400);
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["type"], error::GTS_MISSING_TARGET_HOST);
    // ADR-0007 extension fields are snake_case.
    assert_eq!(
        body["valid_hosts"],
        serde_json::json!(["us.vendor.com", "eu.vendor.com"])
    );
    assert!(body.get("validHosts").is_none());

    // Unrecognized Target-Host ⇒ 400 unknown.target.host.
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-oagw-target-host",
        HeaderValue::from_static("evil.vendor.com"),
    );
    let (status, _, body) = call(
        &svc,
        Method::GET,
        "vendor.com",
        "/v1",
        None,
        headers,
        vec![],
    )
    .await;
    assert_eq!(status, 400);
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["type"], error::GTS_UNKNOWN_TARGET_HOST);
}

#[tokio::test(flavor = "multi_thread")]
async fn query_allowlist_rejects_unknown_params() {
    let server = MockServer::start();
    let (host, port) = endpoints_for(&server);
    let hit = server.mock(|when, then| {
        when.method(GET)
            .path("/v1/search")
            .query_param("q", "cats")
            .query_param("page", "1");
        then.status(200).body("search-ok");
    });

    let repo = seed_repo();
    repo.insert_upstream(upstream("q", &host, port));
    let up = repo.list_upstreams(tenant()).pop().unwrap();
    let mut r = route(up.id, "/v1/search");
    if let Some(h) = &mut r.match_config.http {
        h.query_allowlist = vec!["q".into(), "page".into()];
    }
    repo.insert_route(r);
    let svc = build_service(repo.clone(), &base_config(), None);

    // Allowlisted params are forwarded and hit the upstream.
    let (status, _, body) = call(
        &svc,
        Method::GET,
        "q",
        "/v1/search",
        Some("q=cats&page=1"),
        HeaderMap::new(),
        vec![],
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(text(&body), "search-ok");
    hit.assert();

    // Unknown param → 400 gateway validation, upstream untouched.
    let (status, headers, body) = call(
        &svc,
        Method::GET,
        "q",
        "/v1/search",
        Some("evil=x"),
        HeaderMap::new(),
        vec![],
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(headers.get("x-oagw-error-source").unwrap(), "gateway");
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["type"], error::GTS_VALIDATION_ERROR);
    hit.assert_calls(1);

    // Empty allowlist → no query parameters admitted.
    let repo2 = seed_repo();
    repo2.insert_upstream(upstream("q2", &host, port));
    let up2 = repo2.list_upstreams(tenant()).pop().unwrap();
    repo2.insert_route(route(up2.id, "/v2"));
    let svc2 = build_service(repo2, &base_config(), None);
    let (status, _, _) = call(
        &svc2,
        Method::GET,
        "q2",
        "/v2",
        Some("x=1"),
        HeaderMap::new(),
        vec![],
    )
    .await;
    assert_eq!(status, 400);
}

#[tokio::test(flavor = "multi_thread")]
async fn apikey_auth_injects_header_and_fails_without_secret() {
    let server = MockServer::start();
    let (host, port) = endpoints_for(&server);
    let hit = server.mock(|when, then| {
        when.method(GET)
            .path("/v1/key")
            .header("x-api-key", "sk-secret");
        then.status(200).body("authed");
    });

    let credstore = Arc::new(MockCredStoreClient::with_secrets(vec![(
        "my-key".to_owned(),
        "sk-secret".to_owned(),
    )])) as Arc<dyn credstore_sdk::CredStoreClientV1>;

    let repo = seed_repo();
    let mut up = upstream("key", &host, port);
    up.auth = AuthConfig {
        plugin_type: Some(AUTH_APIKEY.into()),
        sharing: Sharing::Inherit,
        config: serde_json::json!({ "secret_ref": "cred://my-key" }),
    };
    repo.insert_upstream(up);
    let up = repo.list_upstreams(tenant()).pop().unwrap();
    repo.insert_route(route(up.id, "/v1/key"));
    let svc = build_service(repo.clone(), &base_config(), Some(&credstore));

    let (status, _, body) = call(
        &svc,
        Method::GET,
        "key",
        "/v1/key",
        None,
        HeaderMap::new(),
        vec![],
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(text(&body), "authed");
    hit.assert();

    // Unknown secret ref → 401 authentication failed (gateway origin).
    let repo2 = seed_repo();
    let mut up2 = upstream("key2", &host, port);
    up2.auth = AuthConfig {
        plugin_type: Some(AUTH_APIKEY.into()),
        sharing: Sharing::Inherit,
        config: serde_json::json!({ "secret_ref": "cred://missing" }),
    };
    repo2.insert_upstream(up2);
    let up2 = repo2.list_upstreams(tenant()).pop().unwrap();
    repo2.insert_route(route(up2.id, "/v1/key"));
    let svc2 = build_service(repo2, &base_config(), Some(&credstore));
    let (status, headers, body) = call(
        &svc2,
        Method::GET,
        "key2",
        "/v1/key",
        None,
        HeaderMap::new(),
        vec![],
    )
    .await;
    assert_eq!(status, 401);
    assert_eq!(headers.get("x-oagw-error-source").unwrap(), "gateway");
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["type"], error::GTS_AUTHENTICATION_FAILED);
}

#[tokio::test(flavor = "multi_thread")]
async fn required_headers_guard_rejects_and_allows() {
    let server = MockServer::start();
    let (host, port) = endpoints_for(&server);
    let hit = server.mock(|when, then| {
        when.method(GET).path("/v1/req");
        then.status(200).body("guarded-ok");
    });

    let repo = seed_repo();
    let mut up = upstream("req", &host, port);
    up.plugins = PluginsConfig {
        sharing: Sharing::Inherit,
        items: vec![PluginBinding {
            plugin_ref: GUARD_REQUIRED_HEADERS.into(),
            config: serde_json::json!({ "required_request_headers": "x-tenant" }),
        }],
    };
    repo.insert_upstream(up);
    let up = repo.list_upstreams(tenant()).pop().unwrap();
    repo.insert_route(route(up.id, "/v1/req"));
    let svc = build_service(repo.clone(), &base_config(), None);

    // Missing required header → 400 plugin rejection (gateway).
    let (status, headers, body) = call(
        &svc,
        Method::GET,
        "req",
        "/v1/req",
        None,
        HeaderMap::new(),
        vec![],
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(headers.get("x-oagw-error-source").unwrap(), "gateway");
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["code"], "REQUIRED_HEADER_MISSING");
    hit.assert_calls(0);

    // Present header → passes through.
    let mut headers = HeaderMap::new();
    headers.insert("x-tenant", HeaderValue::from_static("acme"));
    let (status, _, body) = call(&svc, Method::GET, "req", "/v1/req", None, headers, vec![]).await;
    assert_eq!(status, 200);
    assert_eq!(text(&body), "guarded-ok");
    hit.assert();
}

#[tokio::test(flavor = "multi_thread")]
async fn request_id_transform_propagates_and_echoes() {
    let server = MockServer::start();
    let (host, port) = endpoints_for(&server);
    let hit = server.mock(|when, then| {
        when.method(GET)
            .path("/v1/rid")
            .header("x-request-id", "abc-123");
        then.status(200)
            .header("content-type", "text/plain")
            .body("rid-ok");
    });

    let repo = seed_repo();
    let mut up = upstream("rid", &host, port);
    up.plugins = PluginsConfig {
        sharing: Sharing::Inherit,
        items: vec![PluginBinding {
            plugin_ref: TRANSFORM_REQUEST_ID.into(),
            config: serde_json::Value::Null,
        }],
    };
    repo.insert_upstream(up);
    let up = repo.list_upstreams(tenant()).pop().unwrap();
    repo.insert_route(route(up.id, "/v1/rid"));
    let svc = build_service(repo.clone(), &base_config(), None);

    let mut headers = HeaderMap::new();
    headers.insert("x-request-id", HeaderValue::from_static("abc-123"));
    let (status, resp_headers, body) =
        call(&svc, Method::GET, "rid", "/v1/rid", None, headers, vec![]).await;
    assert_eq!(status, 200);
    assert_eq!(text(&body), "rid-ok");
    assert_eq!(resp_headers.get("x-request-id").unwrap(), "abc-123");
    hit.assert();
}

#[tokio::test(flavor = "multi_thread")]
async fn disabled_upstream_returns_503_through_proxy() {
    let server = MockServer::start();
    let (host, port) = endpoints_for(&server);
    let repo = seed_repo();
    let mut up = upstream("down", &host, port);
    up.enabled = false;
    repo.insert_upstream(up);
    let up = repo.list_upstreams(tenant()).pop().unwrap();
    repo.insert_route(route(up.id, "/v1"));
    let svc = build_service(repo.clone(), &base_config(), None);

    let (status, headers, body) = call(
        &svc,
        Method::GET,
        "down",
        "/v1",
        None,
        HeaderMap::new(),
        vec![],
    )
    .await;
    assert_eq!(status, 503);
    assert_eq!(headers.get("x-oagw-error-source").unwrap(), "gateway");
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["type"], error::GTS_LINK_UNAVAILABLE);
}

#[tokio::test(flavor = "multi_thread")]
async fn route_priority_picks_higher_among_equal_longest_paths() {
    let server = MockServer::start();
    let (host, port) = endpoints_for(&server);
    let hit = server.mock(|when, then| {
        when.method(GET).path("/v1/users");
        then.status(200).body("users-ok");
    });

    let repo = seed_repo();
    repo.insert_upstream(upstream("pr", &host, port));
    let up = repo.list_upstreams(tenant()).pop().unwrap();
    // Same path+method prefix: route A (priority 0, no transform) and route B
    // (priority 10, request_id transform).  Higher priority must win.
    let mut a = route(up.id, "/v1/users");
    a.priority = 0;
    let mut b = route(up.id, "/v1/users");
    b.priority = 10;
    b.plugins = PluginsConfig {
        sharing: Sharing::Inherit,
        items: vec![PluginBinding {
            plugin_ref: TRANSFORM_REQUEST_ID.into(),
            config: serde_json::Value::Null,
        }],
    };
    repo.insert_route(a);
    repo.insert_route(b);
    let svc = build_service(repo.clone(), &base_config(), None);

    let mut headers = HeaderMap::new();
    headers.insert("x-request-id", HeaderValue::from_static("picked-b"));
    let (status, resp_headers, body) =
        call(&svc, Method::GET, "pr", "/v1/users", None, headers, vec![]).await;
    assert_eq!(status, 200);
    assert_eq!(text(&body), "users-ok");
    // Route B's transform bound ⇒ its request_id echoed ⇒ B was selected.
    assert_eq!(resp_headers.get("x-request-id").unwrap(), "picked-b");
    hit.assert();
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_target_host_reports_invalid_value() {
    let server = MockServer::start();
    let (host, port) = endpoints_for(&server);
    let repo = seed_repo();
    repo.insert_upstream(upstream("th", &host, port));
    let up = repo.list_upstreams(tenant()).pop().unwrap();
    repo.insert_route(route(up.id, "/v1"));
    let svc = build_service(repo.clone(), &base_config(), None);

    // ADR-0007: ports are not permitted in X-OAGW-Target-Host.
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-oagw-target-host",
        HeaderValue::from_str(&format!("{host}:{port}")).unwrap(),
    );
    let (status, headers, body) = call(&svc, Method::GET, "th", "/v1", None, headers, vec![]).await;
    assert_eq!(status, 400);
    assert_eq!(headers.get("x-oagw-error-source").unwrap(), "gateway");
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["type"], error::GTS_INVALID_TARGET_HOST);
    assert_eq!(
        body["invalid_value"],
        serde_json::json!(format!("{host}:{port}"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn malformed_content_length_rejected_before_upstream() {
    let server = MockServer::start();
    let (host, port) = endpoints_for(&server);
    let hit = server.mock(|when, then| {
        when.method(POST).path("/v1/cl");
        then.status(200).body("ok");
    });

    let repo = seed_repo();
    repo.insert_upstream(upstream("cl", &host, port));
    let up = repo.list_upstreams(tenant()).pop().unwrap();
    repo.insert_route(route(up.id, "/v1/cl"));
    let svc = build_service(repo.clone(), &base_config(), None);

    // Non-integer Content-Length → 400 validation error, upstream untouched.
    let mut headers = HeaderMap::new();
    headers.insert("content-length", HeaderValue::from_static("abc"));
    let (status, resp_headers, body) = call(
        &svc,
        Method::POST,
        "cl",
        "/v1/cl",
        None,
        headers,
        b"payload".to_vec(),
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(resp_headers.get("x-oagw-error-source").unwrap(), "gateway");
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["type"], error::GTS_VALIDATION_ERROR);
    hit.assert_calls(0);

    // Declared length that does not match the delivered body → 400.
    let mut headers = HeaderMap::new();
    headers.insert("content-length", HeaderValue::from_static("5"));
    let (status, _, _) = call(
        &svc,
        Method::POST,
        "cl",
        "/v1/cl",
        None,
        headers,
        b"hi".to_vec(),
    )
    .await;
    assert_eq!(status, 400);
    hit.assert_calls(0);
}

// ---- WebSocket round-trip ------------------------------------------------

/// Minimal raw-TCP WebSocket echo upstream: answer 101, then echo bytes.
async fn spawn_ws_echo_upstream() -> (u16, tokio::task::JoinHandle<()>, Arc<Mutex<Option<String>>>)
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    // Captures the request-line path the upstream actually receives so tests
    // can assert the gateway rewrote the client path to the route-relative one.
    let first_line: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let captured = first_line.clone();
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let captured = captured.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 512];
                // Read until end of headers.
                loop {
                    let n = match sock.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                {
                    let req = String::from_utf8_lossy(&buf).into_owned();
                    let line = req.split("\r\n").next().unwrap_or_default().to_owned();
                    *captured.lock().unwrap() = Some(line);
                }
                let resp = "HTTP/1.1 101 Switching Protocols\r\n\
                            Upgrade: websocket\r\n\
                            Connection: Upgrade\r\n\
                            Sec-WebSocket-Accept: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                            \r\n";
                if sock.write_all(resp.as_bytes()).await.is_err() {
                    return;
                }
                // Echo loop (raw bytes; the gateway bridges frames verbatim).
                let mut echo = [0u8; 1024];
                loop {
                    match sock.read(&mut echo).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => {
                            if sock.write_all(&echo[..n]).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            });
        }
    });
    (port, handle, first_line)
}

/// Gateway-side handler mirroring `handlers::proxy_suffixed` with a fixed
/// security context injected.
async fn ws_proxy_handler(
    Extension(service): Extension<Arc<DataPlaneService>>,
    Path((alias, suffix)): Path<(String, String)>,
    req: Request,
) -> Response {
    service.proxy(&ctx(), req, &alias, &suffix).await
}

async fn inject_security_context(req: Request, next: axum::middleware::Next) -> Response {
    let mut req = req;
    req.extensions_mut().insert(ctx());
    next.run(req).await
}

#[tokio::test(flavor = "multi_thread")]
async fn websocket_upgrade_bridges_bytes_end_to_end() {
    let (ws_port, _upstream_task, upstream_line) = spawn_ws_echo_upstream().await;

    let repo = seed_repo();
    let up = Upstream {
        id: Uuid::new_v4(),
        tenant_id: tenant(),
        enabled: true,
        alias: "echo".to_owned(),
        tags: vec![],
        server: ServerConfig {
            endpoints: vec![Endpoint {
                scheme: "ws".to_owned(),
                host: "127.0.0.1".to_owned(),
                port: ws_port,
            }],
        },
        protocol: Protocol::Http,
        auth: AuthConfig::default(),
        headers: HeadersConfig::default(),
        plugins: PluginsConfig::default(),
        rate_limit: None,
        cors: None,
        created_at: 0,
    };
    repo.insert_upstream(up);
    let up = repo.list_upstreams(tenant()).pop().unwrap();
    repo.insert_route(route(up.id, "/ws"));
    let svc = build_service(repo.clone(), &base_config(), None);

    let app = Router::new()
        .route(
            "/api/oagw/v1/proxy/{alias}/{*suffix}",
            axum::routing::get(ws_proxy_handler),
        )
        .layer(Extension(svc))
        .layer(axum::middleware::from_fn(inject_security_context));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let serve = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // Real WebSocket handshake through the gateway.
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let handshake = format!(
        "GET /api/oagw/v1/proxy/echo/ws HTTP/1.1\r\n\
         Host: {addr}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Version: 13\r\n\
         \r\n"
    );
    stream.write_all(handshake.as_bytes()).await.unwrap();

    let mut resp_head = Vec::new();
    let mut chunk = [0u8; 256];
    loop {
        let n = stream.read(&mut chunk).await.unwrap();
        resp_head.extend_from_slice(&chunk[..n]);
        if resp_head.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let head = String::from_utf8_lossy(&resp_head).into_owned();
    assert!(
        head.starts_with("HTTP/1.1 101"),
        "expected 101 Switching Protocols, got: {head:?}"
    );
    assert!(head.to_ascii_lowercase().contains("upgrade"));
    // Regression: the upstream's 101 response headers — above all
    // `Sec-WebSocket-Accept` — must survive the bridge.  A real WebSocket
    // client verifies the accept token against its key (RFC 6455 §4.2.2) and
    // rejects a 101 without it.
    assert!(
        head.to_ascii_lowercase().contains("sec-websocket-accept"),
        "101 must carry Sec-WebSocket-Accept from the upstream, got: {head:?}"
    );

    // Bytes written by the client reach the echo upstream and come back.
    stream.write_all(b"ping").await.unwrap();
    stream.write_all(b"\x00\x01\x02").await.unwrap();
    let mut echo = [0u8; 7];
    let read = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        stream.read_exact(&mut echo),
    )
    .await;
    match read {
        Ok(Ok(7)) => {}
        Ok(Ok(n)) => panic!("echo read got {n} bytes, expected 7"),
        Ok(Err(e)) => panic!("echo read error: {e}"),
        Err(e) => panic!("echo read timed out: {e}"),
    }
    assert_eq!(&echo[..4], b"ping");
    assert_eq!(&echo[4..], b"\x00\x01\x02");

    // Regression: the upgrade handshake must reach the upstream with the
    // ROUTE-RELATIVE path (`/ws`), not the full client-facing one
    // (`/api/oagw/v1/proxy/echo/ws`).  An upstream that routes on path would
    // otherwise 404 even though the in-gateway route matched.
    let line = upstream_line.lock().unwrap().clone();
    assert_eq!(
        line.as_deref(),
        Some("GET /ws HTTP/1.1"),
        "upstream must see the rewritten route path, got: {line:?}"
    );

    serve.abort();
}
