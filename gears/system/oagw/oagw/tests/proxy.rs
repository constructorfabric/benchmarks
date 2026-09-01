//! Data-plane proxy integration tests (DESIGN slice 4).
//!
//! Exercises `proxy_request` end-to-end against httpmock upstreams:
//! HTTP forwarding (method/path/query, header transforms, passthrough),
//! the `X-OAGW-Target-Host` routing matrix, the DESIGN body-validation and
//! error tables (400/404/413/502/503), SSE passthrough, and a real
//! WebSocket upgrade bridged through an `axum::serve`-hosted router to a
//! raw-TCP echo upstream.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use credstore_sdk::test_util::MockCredStoreClient;
use http::{HeaderMap, HeaderValue, Method, Request, StatusCode};
use http_body_util::BodyExt;
use httpmock::prelude::*;
use oagw::api::rest::routes::register_routes;
use oagw::config::{OagwConfig, SsrfPolicy};
use oagw::domain::models::{
    AuthConfig, BurstConfig, Endpoint, HeaderOps, HeadersConfig, HttpMatch, MatchRule,
    PROTOCOL_HTTP_V1, PassthroughMode, PathSuffixMode, PluginsConfig, RateLimitAlgorithm,
    RateLimitConfig, RateLimitScope, RateLimitStrategy, RateWindow, RequestHeadersConfig, Route,
    Scheme, ServerConfig, SharingMode, SustainedRate, Upstream,
};
use oagw::domain::plugin::{
    API_KEY_AUTH_PLUGIN_ID, BASIC_AUTH_PLUGIN_ID, BEARER_AUTH_PLUGIN_ID, NOOP_AUTH_PLUGIN_ID,
    OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID, OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
    REQUIRED_HEADERS_GUARD_PLUGIN_ID,
};
use oagw::domain::service::ControlPlaneService;
use oagw::infra::plugin::{AuthPluginRegistry, TokenCacheConfig};
use oagw::infra::proxy::proxy_request;
use oagw::infra::ratelimit::RateLimiter;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationSpec;
use toolkit_security::SecurityContext;
use uuid::Uuid;

const TENANT: u128 = 1;

fn tenant() -> Uuid {
    Uuid::from_u128(TENANT)
}

/// A permissive proxy config for tests: plain HTTP relay on, SSRF off, 1 MiB
/// body limit.
fn test_config() -> OagwConfig {
    OagwConfig {
        allow_http_upstream: true,
        ssrf_policy: SsrfPolicy {
            enabled: false,
            allowlist: Vec::new(),
            denylist: Vec::new(),
        },
        body_limit_bytes: 1024 * 1024,
        ..Default::default()
    }
}

fn service(cfg: OagwConfig) -> Arc<ControlPlaneService> {
    Arc::new(ControlPlaneService::new(cfg))
}

/// Build an upstream on `127.0.0.1:{port}` with an explicit alias.
fn upstream(alias: &str, port: u16) -> Upstream {
    Upstream {
        id: Uuid::new_v4(),
        enabled: true,
        alias: alias.to_owned(),
        tags: Vec::new(),
        server: ServerConfig {
            endpoints: vec![Endpoint {
                scheme: Scheme::Https, // relayed over plain HTTP in this MVP
                host: "127.0.0.1".to_owned(),
                port,
            }],
        },
        protocol: PROTOCOL_HTTP_V1.to_owned(),
        auth: None,
        headers: HeadersConfig::default(),
        plugins: PluginsConfig::default(),
        rate_limit: None,
        cors: None,
    }
}

/// Build a route on an upstream with one HTTP match (methods × path).
fn route(upstream_id: Uuid, methods: &[&str], path: &str) -> Route {
    Route {
        id: Uuid::new_v4(),
        enabled: true,
        tags: Vec::new(),
        upstream_id,
        r#match: Some(MatchRule {
            http: Some(HttpMatch {
                methods: methods
                    .iter()
                    .map(std::string::ToString::to_string)
                    .collect(),
                path: path.to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        }),
        plugins: PluginsConfig::default(),
        rate_limit: None,
        cors: None,
    }
}

/// Register the given upstream+route and run a proxy request through the
/// engine directly (router-less; the gateway chain is the caller's tenant).
async fn proxy(
    svc: &Arc<ControlPlaneService>,
    alias: &str,
    path: &str,
    method: Method,
    headers: HeaderMap,
    body: Body,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let resp = proxy_request(
        svc,
        vec![tenant()],
        alias.to_owned(),
        path.to_owned(),
        method,
        headers,
        body,
        None,
        Some(
            &SecurityContext::builder()
                .subject_id(Uuid::new_v4())
                .subject_tenant_id(tenant())
                .build()
                .unwrap(),
        ),
        None,
        None,
    )
    .await;
    let status = resp.status();
    let hdrs = resp.headers().clone();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, hdrs, bytes)
}

/// A deterministic security context (fixed subject) so token-cache identity
/// stays constant across calls within a test.
fn ctx_for(tenant: u128, subject: u64) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::from_u128(u128::from(subject)))
        .subject_tenant_id(Uuid::from_u128(tenant))
        .build()
        .unwrap()
}

/// Run a proxy request with an explicit security context + auth registry.
#[allow(
    clippy::too_many_arguments,
    reason = "helper mirrors the full proxy_request argument list"
)]
async fn proxy_with(
    svc: &Arc<ControlPlaneService>,
    alias: &str,
    path: &str,
    method: Method,
    headers: HeaderMap,
    body: Body,
    ctx: &SecurityContext,
    registry: Option<&AuthPluginRegistry>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let resp = proxy_request(
        svc,
        vec![tenant()],
        alias.to_owned(),
        path.to_owned(),
        method,
        headers,
        body,
        None,
        Some(ctx),
        registry,
        None,
    )
    .await;
    let status = resp.status();
    let hdrs = resp.headers().clone();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, hdrs, bytes)
}

/// An auth registry with the builtin plugins bound to a mock `cred_store`.
fn registry_with(store: MockCredStoreClient) -> AuthPluginRegistry {
    AuthPluginRegistry::new(
        Some(Arc::new(store)),
        TokenCacheConfig {
            ttl: Duration::from_mins(5),
            capacity: 100,
        },
    )
}

/// Run a proxy request with an explicit rate limiter (Slice 6).
#[allow(
    clippy::too_many_arguments,
    reason = "helper mirrors the full proxy_request argument list"
)]
async fn proxy_rate(
    svc: &Arc<ControlPlaneService>,
    alias: &str,
    path: &str,
    method: Method,
    headers: HeaderMap,
    body: Body,
    ctx: &SecurityContext,
    rate: &RateLimiter,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let resp = proxy_request(
        svc,
        vec![tenant()],
        alias.to_owned(),
        path.to_owned(),
        method,
        headers,
        body,
        None,
        Some(ctx),
        None,
        Some(rate),
    )
    .await;
    let status = resp.status();
    let hdrs = resp.headers().clone();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, hdrs, bytes)
}

/// A rate-limit config: `rate` tokens per second with an equal burst capacity.
fn per_second(rate: u64) -> RateLimitConfig {
    RateLimitConfig {
        sharing: SharingMode::Private,
        algorithm: RateLimitAlgorithm::default(),
        sustained: SustainedRate {
            rate,
            window: RateWindow::Second,
        },
        burst: Some(BurstConfig { capacity: rate }),
        scope: RateLimitScope::Tenant,
        strategy: RateLimitStrategy::default(),
        cost: 1,
    }
}

fn problem_type(status: StatusCode, headers: &HeaderMap, body: &[u8]) -> String {
    assert_eq!(
        headers
            .get("x-oagw-error-source")
            .map(|v| v.to_str().unwrap()),
        Some("gateway")
    );
    assert_eq!(
        headers.get("content-type").map(|v| v.to_str().unwrap()),
        Some("application/problem+json")
    );
    let json: serde_json::Value = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(e) => panic!("problem+json body: {e}"),
    };
    assert_eq!(json["status"], status.as_u16());
    json["type"].as_str().unwrap().to_owned()
}

// ---------------------------------------------------------------------------
// Forwarding
// ---------------------------------------------------------------------------

#[tokio::test]
async fn forwards_request_and_preserves_status_headers_body() {
    let mock = MockServer::start();
    let m = mock.mock(|when, then| {
        when.method(GET).path("/v1/chat");
        then.status(200)
            .header("content-type", "application/json")
            .header("x-upstream", "yes")
            .body(r#"{"ok":true,"model":"gpt-4"}"#);
    });

    let svc = service(test_config());
    let u = svc
        .create_upstream(tenant(), upstream("mock-api", mock.port()))
        .unwrap();
    svc.create_route(tenant(), route(u.id, &["GET"], "/v1/chat"))
        .unwrap();

    let (status, hdrs, body) = proxy(
        &svc,
        "mock-api",
        "v1/chat",
        Method::GET,
        HeaderMap::new(),
        Body::empty(),
    )
    .await;

    m.assert();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(hdrs.get("content-type").unwrap(), "application/json");
    assert_eq!(hdrs.get("x-upstream").unwrap(), "yes");
    // ADR 0007: upstream-passthrough responses carry error-source: upstream.
    assert_eq!(hdrs.get("x-oagw-error-source").unwrap(), "upstream");
    assert_eq!(body, br#"{"ok":true,"model":"gpt-4"}"#);
}

#[tokio::test]
async fn forwards_post_body_and_query_string() {
    let mock = MockServer::start();
    let m = mock.mock(|when, then| {
        when.method(POST)
            .path("/v1/chat")
            .query_param("model", "gpt-4")
            .query_param("stream", "true")
            .body_includes("How are you");
        then.status(200).body("reply");
    });

    let svc = service(test_config());
    let u = svc
        .create_upstream(tenant(), upstream("mock-api", mock.port()))
        .unwrap();
    svc.create_route(tenant(), route(u.id, &["POST"], "/v1/chat"))
        .unwrap();

    let (status, _, body) = proxy(
        &svc,
        "mock-api",
        "v1/chat?model=gpt-4&stream=true",
        Method::POST,
        HeaderMap::new(),
        Body::from(r#"{"msg":"How are you"}"#),
    )
    .await;
    m.assert();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"reply");
}

#[tokio::test]
async fn applies_header_transforms_and_strips_routing_header() {
    let mock = MockServer::start();
    let m = mock.mock(|when, then| {
        when.method(POST)
            .path("/v1/chat")
            .header_exists("x-allowed") // passthrough allowlist
            .header_exists("x-set") // set rule
            .header_exists("x-add") // add rule
            .header_missing("x-drop") // remove rule
            .header_missing("x-oagw-target-host") // routing header stripped
            .header_missing("x-not-allowed"); // not in allowlist → dropped
        then.status(204);
    });

    let mut u = upstream("mock-api", mock.port());
    u.headers = HeadersConfig {
        request: RequestHeadersConfig {
            passthrough: PassthroughMode::Allowlist,
            passthrough_allowlist: vec!["x-allowed".to_owned()],
            set: BTreeMap::from([("x-set".to_owned(), "s".to_owned())]),
            add: BTreeMap::from([("x-add".to_owned(), "a".to_owned())]),
            remove: vec!["x-drop".to_owned()],
        },
        response: HeaderOps::default(),
    };
    let svc = service(test_config());
    let u = svc.create_upstream(tenant(), u).unwrap();
    svc.create_route(tenant(), route(u.id, &["POST"], "/v1/chat"))
        .unwrap();

    let mut headers = HeaderMap::new();
    headers.insert("x-allowed", "1".parse().unwrap());
    headers.insert("x-not-allowed", "1".parse().unwrap());
    headers.insert("x-drop", "1".parse().unwrap());
    headers.insert("x-oagw-target-host", "127.0.0.1".parse().unwrap());

    let (status, _, _) = proxy(
        &svc,
        "mock-api",
        "v1/chat",
        Method::POST,
        headers,
        Body::from(r#"{"x":1}"#),
    )
    .await;
    m.assert();
    assert_eq!(status, StatusCode::NO_CONTENT);
}

// ---------------------------------------------------------------------------
// Route matching + DESIGN error table
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unknown_alias_is_404_route_not_found() {
    let svc = service(test_config());
    let (status, hdrs, body) = proxy(
        &svc,
        "nope",
        "v1/x",
        Method::GET,
        HeaderMap::new(),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        problem_type(status, &hdrs, &body),
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
    );
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["alias"], "nope");
}

#[tokio::test]
async fn disallowed_method_is_404_route_not_found() {
    let mock = MockServer::start();
    let svc = service(test_config());
    let u = svc
        .create_upstream(tenant(), upstream("mock-api", mock.port()))
        .unwrap();
    svc.create_route(tenant(), route(u.id, &["GET"], "/v1/chat"))
        .unwrap();

    let (status, hdrs, body) = proxy(
        &svc,
        "mock-api",
        "v1/chat",
        Method::POST,
        HeaderMap::new(),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        problem_type(status, &hdrs, &body),
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
    );
}

#[tokio::test]
async fn longest_path_prefix_wins() {
    let mock = MockServer::start();
    let m = mock.mock(|when, then| {
        when.method(GET).path("/v1/chat/summarize");
        then.status(200).body("summarized");
    });

    let svc = service(test_config());
    let u = svc
        .create_upstream(tenant(), upstream("mock-api", mock.port()))
        .unwrap();
    // Broad prefix first, then a longer one — the longer must win.
    svc.create_route(tenant(), route(u.id, &["GET"], "/v1"))
        .unwrap();
    svc.create_route(tenant(), route(u.id, &["GET"], "/v1/chat/summarize"))
        .unwrap();

    let (status, _, body) = proxy(
        &svc,
        "mock-api",
        "v1/chat/summarize",
        Method::GET,
        HeaderMap::new(),
        Body::empty(),
    )
    .await;
    m.assert();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"summarized");
}

#[tokio::test]
async fn path_suffix_disabled_mode_is_400_validation() {
    let mock = MockServer::start();
    let svc = service(test_config());
    let u = svc
        .create_upstream(tenant(), upstream("mock-api", mock.port()))
        .unwrap();
    let mut r = route(u.id, &["GET"], "/exact");
    r.r#match
        .as_mut()
        .unwrap()
        .http
        .as_mut()
        .unwrap()
        .path_suffix_mode = PathSuffixMode::Disabled;
    svc.create_route(tenant(), r).unwrap();

    // Exact path is fine: forwarded to the upstream (guarded by httpmock 200).
    let m = mock.mock(|when, then| {
        when.method(GET).path("/exact");
        then.status(200).body("ok");
    });
    let (status, _, body) = proxy(
        &svc,
        "mock-api",
        "exact",
        Method::GET,
        HeaderMap::new(),
        Body::empty(),
    )
    .await;
    m.assert();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"ok");

    // A path suffix is rejected before any forwarding.
    let (status, hdrs, body) = proxy(
        &svc,
        "mock-api",
        "exact/extra",
        Method::GET,
        HeaderMap::new(),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        problem_type(status, &hdrs, &body),
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
}

#[tokio::test]
async fn query_allowlist_rejects_unknown_params() {
    let mock = MockServer::start();
    let svc = service(test_config());
    let u = svc
        .create_upstream(tenant(), upstream("mock-api", mock.port()))
        .unwrap();
    let mut r = route(u.id, &["GET"], "/v1/chat");
    r.r#match
        .as_mut()
        .unwrap()
        .http
        .as_mut()
        .unwrap()
        .query_allowlist = vec!["model".to_owned()];
    svc.create_route(tenant(), r).unwrap();

    // Allowed param: guard passes (connect to httpmock succeeds → 200).
    let m = mock.mock(|when, then| {
        when.method(GET)
            .path("/v1/chat")
            .query_param("model", "gpt-4");
        then.status(200).body("ok");
    });
    let (status, _, _) = proxy(
        &svc,
        "mock-api",
        "v1/chat?model=gpt-4",
        Method::GET,
        HeaderMap::new(),
        Body::empty(),
    )
    .await;
    m.assert();
    assert_eq!(status, StatusCode::OK);

    // Disallowed param → 400 before forwarding.
    let (status, hdrs, body) = proxy(
        &svc,
        "mock-api",
        "v1/chat?model=gpt-4&extra=y",
        Method::GET,
        HeaderMap::new(),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        problem_type(status, &hdrs, &body),
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
}

// ---------------------------------------------------------------------------
// Body validation
// ---------------------------------------------------------------------------

/// Register a bare upstream (alias `a`, no live listener) + a POST route on
/// `/x`; used by the body-validation tests where validation fails before any
/// forwarding would occur.
fn register_bare_route(svc: &Arc<ControlPlaneService>) -> Uuid {
    let u = svc.create_upstream(tenant(), upstream("a", 1)).unwrap();
    svc.create_route(tenant(), route(u.id, &["POST"], "/x"))
        .unwrap();
    u.id
}

#[tokio::test]
async fn oversized_body_is_413_payload_too_large() {
    let mut cfg = test_config();
    cfg.body_limit_bytes = 16;
    let svc = service(cfg);
    register_bare_route(&svc);

    // No Content-Length: rejected as soon as the buffering limit is crossed.
    let (status, hdrs, body) = proxy(
        &svc,
        "a",
        "x",
        Method::POST,
        HeaderMap::new(),
        Body::from(vec![b'x'; 20]),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        problem_type(status, &hdrs, &body),
        "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1"
    );

    // Declared Content-Length above the limit: rejected before buffering.
    let mut headers = HeaderMap::new();
    headers.insert("content-length", "500".parse().unwrap());
    let (status, hdrs, body) = proxy(
        &svc,
        "a",
        "x",
        Method::POST,
        headers,
        Body::from(b"tiny".to_vec()),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        problem_type(status, &hdrs, &body),
        "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1"
    );
}

#[tokio::test]
async fn content_length_mismatch_is_400() {
    let svc = service(test_config());
    register_bare_route(&svc);
    let mut headers = HeaderMap::new();
    headers.insert("content-length", "5".parse().unwrap());
    let (status, hdrs, body) = proxy(
        &svc,
        "a",
        "x",
        Method::POST,
        headers,
        Body::from(b"hello!".to_vec()),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        problem_type(status, &hdrs, &body),
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
}

#[tokio::test]
async fn invalid_content_length_is_400() {
    let svc = service(test_config());
    register_bare_route(&svc);
    let mut headers = HeaderMap::new();
    headers.insert("content-length", "abc".parse().unwrap());
    let (status, hdrs, body) = proxy(&svc, "a", "x", Method::POST, headers, Body::empty()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        problem_type(status, &hdrs, &body),
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
}

#[tokio::test]
async fn unsupported_transfer_encoding_is_400() {
    let svc = service(test_config());
    register_bare_route(&svc);
    let mut headers = HeaderMap::new();
    headers.insert("transfer-encoding", "gzip".parse().unwrap());
    let (status, hdrs, body) = proxy(&svc, "a", "x", Method::POST, headers, Body::empty()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        problem_type(status, &hdrs, &body),
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
}

// ---------------------------------------------------------------------------
// X-OAGW-Target-Host matrix (ADR 0001)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn single_endpoint_target_host_selects_endpoint() {
    let mock = MockServer::start();
    let m = mock.mock(|when, then| {
        when.method(GET).path("/v1/chat");
        then.status(200).body("selected");
    });
    let svc = service(test_config());
    let u = svc
        .create_upstream(tenant(), upstream("mock-api", mock.port()))
        .unwrap();
    svc.create_route(tenant(), route(u.id, &["GET"], "/v1/chat"))
        .unwrap();

    let mut headers = HeaderMap::new();
    headers.insert("x-oagw-target-host", "127.0.0.1".parse().unwrap());
    let (status, _, body) = proxy(
        &svc,
        "mock-api",
        "v1/chat",
        Method::GET,
        headers,
        Body::empty(),
    )
    .await;
    m.assert();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"selected");
}

#[tokio::test]
async fn common_suffix_pool_requires_target_host() {
    // us.vendor.com / eu.vendor.com derive the alias "vendor.com"; the routing
    // header is mandatory for the pool alias.
    let mock = MockServer::start();
    let u = Upstream {
        id: Uuid::new_v4(),
        enabled: true,
        alias: String::new(), // auto-derived to "vendor.com"
        tags: Vec::new(),
        server: ServerConfig {
            endpoints: vec![
                Endpoint {
                    scheme: Scheme::Https,
                    host: "us.vendor.com".to_owned(),
                    port: mock.port(),
                },
                Endpoint {
                    scheme: Scheme::Https,
                    host: "eu.vendor.com".to_owned(),
                    port: mock.port(),
                },
            ],
        },
        protocol: PROTOCOL_HTTP_V1.to_owned(),
        auth: None,
        headers: HeadersConfig::default(),
        plugins: PluginsConfig::default(),
        rate_limit: None,
        cors: None,
    };
    let svc = service(test_config());
    let u = svc.create_upstream(tenant(), u).unwrap();
    // Non-standard port appends `:port` to the derived common-suffix alias.
    let pool_alias = format!("vendor.com:{}", mock.port());
    assert_eq!(u.alias, pool_alias);
    svc.create_route(tenant(), route(u.id, &["GET"], "/v1/chat"))
        .unwrap();

    // Missing header → 400 missing_target_host.
    let (status, hdrs, body) = proxy(
        &svc,
        &pool_alias,
        "v1/chat",
        Method::GET,
        HeaderMap::new(),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        problem_type(status, &hdrs, &body),
        "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1"
    );

    // Malformed value (scheme/brackets) → 400 invalid_target_host.
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-oagw-target-host",
        "http://us.vendor.com".parse().unwrap(),
    );
    let (status, hdrs, body) = proxy(
        &svc,
        &pool_alias,
        "v1/chat",
        Method::GET,
        headers,
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        problem_type(status, &hdrs, &body),
        "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1"
    );

    // Well-formed but not an endpoint → 400 unknown_target_host.
    let mut headers = HeaderMap::new();
    headers.insert("x-oagw-target-host", "other.vendor.com".parse().unwrap());
    let (status, hdrs, body) = proxy(
        &svc,
        &pool_alias,
        "v1/chat",
        Method::GET,
        headers,
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        problem_type(status, &hdrs, &body),
        "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1"
    );

    // Valid pool member → forwarded (DNS for the fake domain fails → 502,
    // proving the matrix selected a real endpoint and attempted a connection).
    let mut headers = HeaderMap::new();
    headers.insert("x-oagw-target-host", "us.vendor.com".parse().unwrap());
    let (status, hdrs, body) = proxy(
        &svc,
        &pool_alias,
        "v1/chat",
        Method::GET,
        headers,
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(
        problem_type(status, &hdrs, &body),
        "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1"
    );
}

#[tokio::test]
async fn explicit_multi_endpoint_alias_round_robins() {
    let a = MockServer::start();
    let b = MockServer::start();
    let ma = a.mock(|when, then| {
        when.method(GET).path("/ping");
        then.status(200).body("A");
    });
    let mb = b.mock(|when, then| {
        when.method(GET).path("/ping");
        then.status(200).body("B");
    });

    let svc = service(test_config());
    let u = Upstream {
        id: Uuid::new_v4(),
        enabled: true,
        alias: "mock-pool".to_owned(), // IP endpoints → explicit alias required
        tags: Vec::new(),
        server: ServerConfig {
            endpoints: vec![
                Endpoint {
                    scheme: Scheme::Https,
                    host: "127.0.0.1".to_owned(),
                    port: a.port(),
                },
                Endpoint {
                    scheme: Scheme::Https,
                    host: "127.0.0.1".to_owned(),
                    port: b.port(),
                },
            ],
        },
        protocol: PROTOCOL_HTTP_V1.to_owned(),
        auth: None,
        headers: HeadersConfig::default(),
        plugins: PluginsConfig::default(),
        rate_limit: None,
        cors: None,
    };
    let u = svc.create_upstream(tenant(), u).unwrap();
    svc.create_route(tenant(), route(u.id, &["GET"], "/ping"))
        .unwrap();

    let (status, _, first) = proxy(
        &svc,
        "mock-pool",
        "ping",
        Method::GET,
        HeaderMap::new(),
        Body::empty(),
    )
    .await;
    let (status2, _, second) = proxy(
        &svc,
        "mock-pool",
        "ping",
        Method::GET,
        HeaderMap::new(),
        Body::empty(),
    )
    .await;
    assert_eq!((status, status2), (StatusCode::OK, StatusCode::OK));
    // Each endpoint is hit exactly once across the two requests.
    ma.assert_calls(1);
    mb.assert_calls(1);
    assert_ne!(first, second);
    assert!(first == b"A" || first == b"B");
    assert!(second == b"A" || second == b"B");
}

// ---------------------------------------------------------------------------
// Fail-closed configuration + connect errors
// ---------------------------------------------------------------------------

#[tokio::test]
async fn allow_http_upstream_disabled_is_502() {
    let svc = service(test_config()); // allow_http_upstream=true by default in helper
    let u = svc
        .create_upstream(tenant(), upstream("mock-api", 1))
        .unwrap();
    svc.create_route(tenant(), route(u.id, &["GET"], "/v1/chat"))
        .unwrap();
    let _ = u;

    let mut cfg = test_config();
    cfg.allow_http_upstream = false;
    let svc = service(cfg);
    let u = svc
        .create_upstream(tenant(), upstream("mock-api", 1))
        .unwrap();
    svc.create_route(tenant(), route(u.id, &["GET"], "/v1/chat"))
        .unwrap();

    let (status, hdrs, body) = proxy(
        &svc,
        "mock-api",
        "v1/chat",
        Method::GET,
        HeaderMap::new(),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(
        problem_type(status, &hdrs, &body),
        "gts.cf.core.errors.err.v1~cf.oagw.upstream.unsupported.v1"
    );
}

#[tokio::test]
async fn ssrf_enabled_fails_closed_with_502() {
    let svc = service(OagwConfig::default()); // ssrf_policy.enabled = true
    let u = svc
        .create_upstream(tenant(), upstream("mock-api", 1))
        .unwrap();
    svc.create_route(tenant(), route(u.id, &["GET"], "/v1/chat"))
        .unwrap();

    let (status, hdrs, body) = proxy(
        &svc,
        "mock-api",
        "v1/chat",
        Method::GET,
        HeaderMap::new(),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(
        problem_type(status, &hdrs, &body),
        "gts.cf.core.errors.err.v1~cf.oagw.upstream.unsupported.v1"
    );
}

#[tokio::test]
async fn connection_refused_is_503_link_unavailable() {
    // Grab a free port, then close it so nothing is listening.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let svc = service(test_config());
    let u = svc
        .create_upstream(tenant(), upstream("refused", port))
        .unwrap();
    svc.create_route(tenant(), route(u.id, &["GET"], "/v1/chat"))
        .unwrap();

    let (status, hdrs, body) = proxy(
        &svc,
        "refused",
        "v1/chat",
        Method::GET,
        HeaderMap::new(),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        problem_type(status, &hdrs, &body),
        "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1"
    );
}

// ---------------------------------------------------------------------------
// SSE passthrough
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sse_response_streams_events_through() {
    let mock = MockServer::start();
    let m = mock.mock(|when, then| {
        when.method(GET).path("/v1/events");
        then.status(200)
            .header("content-type", "text/event-stream")
            .header("x-accel-buffering", "no")
            .body("data: one\n\ndata: two\n\ndata: three\n\n");
    });

    let svc = service(test_config());
    let u = svc
        .create_upstream(tenant(), upstream("mock-api", mock.port()))
        .unwrap();
    svc.create_route(tenant(), route(u.id, &["GET"], "/v1/events"))
        .unwrap();

    let (status, hdrs, body) = proxy(
        &svc,
        "mock-api",
        "v1/events",
        Method::GET,
        HeaderMap::new(),
        Body::empty(),
    )
    .await;
    m.assert();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(hdrs.get("content-type").unwrap(), "text/event-stream");
    assert_eq!(hdrs.get("x-accel-buffering").unwrap(), "no");
    assert_eq!(hdrs.get("x-oagw-error-source").unwrap(), "upstream");
    assert_eq!(body, b"data: one\n\ndata: two\n\ndata: three\n\n");
}

// ---------------------------------------------------------------------------
// WebSocket upgrade bridging (end-to-end through a real hyper server)
// ---------------------------------------------------------------------------

/// Minimal `OpenAPI` registry for router registration in the WS test.
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

/// Axum middleware that injects a security context, mirroring the host's auth
/// middleware so the proxy handler can extract `Extension<SecurityContext>`.
async fn inject_security_ctx(
    mut req: Request<Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let ctx = SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(Uuid::from_u128(TENANT))
        .build()
        .unwrap();
    req.extensions_mut().insert(ctx);
    next.run(req).await
}

/// Serve the OAGW router, returning the bound address.
async fn serve(svc: Arc<ControlPlaneService>) -> std::net::SocketAddr {
    let registry = NoopOpenApiRegistry;
    let app = register_routes(Router::new(), &registry, svc, None, None, None)
        .layer(axum::middleware::from_fn(inject_security_ctx));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _result = axum::serve(listener, app.into_make_service()).await;
    });
    addr
}

/// A raw-TCP upstream that answers any request with a 101 Switching Protocols
/// and then echoes bytes (a stand-in for a WebSocket echo server). Returns the
/// bound address plus the lowercased request head that reached the upstream.
async fn spawn_echo_upstream() -> (std::net::SocketAddr, Arc<Mutex<Option<String>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(None));
    let captured_for_task = captured.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let captured_ref = captured_for_task.clone();
            tokio::spawn(async move {
                // Read the request head.
                let mut head = Vec::new();
                let mut chunk = [0u8; 512];
                loop {
                    let n = stream.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    head.extend_from_slice(&chunk[..n]);
                    if head.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let head_lower = String::from_utf8_lossy(&head).to_ascii_lowercase();
                *captured_ref.lock().unwrap() = Some(head_lower);
                stream
                    .write_all(
                        b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n",
                    )
                    .await
                    .unwrap();
                // Echo loop.
                let mut buf = [0u8; 4096];
                loop {
                    match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if stream.write_all(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
    });
    (addr, captured)
}

#[tokio::test]
async fn websocket_upgrade_is_bridged_to_the_upstream() {
    let (echo_addr, echo_head) = spawn_echo_upstream().await;

    let svc = service(test_config());
    let u = svc
        .create_upstream(tenant(), upstream("echo", echo_addr.port()))
        .unwrap();
    svc.create_route(tenant(), route(u.id, &["GET"], "/ws"))
        .unwrap();
    let gw = serve(svc).await;

    // Raw-socket client performing a WebSocket handshake through the gateway.
    let mut stream = tokio::net::TcpStream::connect(gw).await.unwrap();
    let handshake = format!(
        "GET /oagw/v1/proxy/echo/ws HTTP/1.1\r\nHost: {gw}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
    );
    stream.write_all(handshake.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();

    let mut head = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        let n = stream.read(&mut chunk).await.unwrap();
        assert!(n > 0, "gateway closed before answering the handshake");
        head.extend_from_slice(&chunk[..n]);
        if head.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let head = String::from_utf8_lossy(&head);
    assert!(
        head.starts_with("HTTP/1.1 101"),
        "expected 101 Switching Protocols, got: {head:?}"
    );
    assert!(head.contains("x-oagw-error-source: upstream"));

    // Round-trip bytes through the bridged connection.
    stream.write_all(b"ping").await.unwrap();
    stream.flush().await.unwrap();
    let mut buf = [0u8; 4];
    stream.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"ping");

    // A second exchange proves the bridge stays open.
    stream.write_all(b"pong").await.unwrap();
    stream.flush().await.unwrap();
    let mut buf = [0u8; 4];
    stream.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"pong");

    // Rf-007: the upstream must have received the client's RFC 6455 handshake
    // headers end-to-end (they are re-attached, not passthrough-managed).
    let head = echo_head.lock().unwrap().clone().expect("handshake head");
    assert!(
        head.contains("sec-websocket-key:"),
        "upstream must receive Sec-WebSocket-Key, got: {head}"
    );
    assert!(
        head.contains("sec-websocket-version: 13"),
        "upstream must receive Sec-WebSocket-Version, got: {head}"
    );
}

/// Rr-004: a client sending repeated `Sec-WebSocket-*` handshake headers (RFC
/// 6455 allows multi-valued `Sec-WebSocket-Protocol`/`-Extensions`) must have
/// ALL values relayed end-to-end — the re-attach path appends rather than
/// replacing, so no value is silently dropped.
#[tokio::test]
async fn websocket_multi_value_handshake_headers_are_all_relayed() {
    let (echo_addr, echo_head) = spawn_echo_upstream().await;

    let svc = service(test_config());
    let u = svc
        .create_upstream(tenant(), upstream("echo", echo_addr.port()))
        .unwrap();
    svc.create_route(tenant(), route(u.id, &["GET"], "/ws"))
        .unwrap();
    let gw = serve(svc).await;

    let mut stream = tokio::net::TcpStream::connect(gw).await.unwrap();
    let handshake = format!(
        "GET /oagw/v1/proxy/echo/ws HTTP/1.1\r\nHost: {gw}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: chat\r\nSec-WebSocket-Protocol: superchat\r\n\r\n"
    );
    stream.write_all(handshake.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();

    let mut head = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        let n = stream.read(&mut chunk).await.unwrap();
        assert!(n > 0, "gateway closed before answering the handshake");
        head.extend_from_slice(&chunk[..n]);
        if head.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let head = String::from_utf8_lossy(&head);
    assert!(
        head.starts_with("HTTP/1.1 101"),
        "expected 101 Switching Protocols, got: {head:?}"
    );

    // Round-trip once so the upstream head write is guaranteed visible.
    stream.write_all(b"ping").await.unwrap();
    stream.flush().await.unwrap();
    let mut buf = [0u8; 4];
    stream.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"ping");

    let head = echo_head.lock().unwrap().clone().expect("handshake head");
    assert!(
        head.contains("sec-websocket-protocol: chat"),
        "first Sec-WebSocket-Protocol value must be relayed, got: {head}"
    );
    assert!(
        head.contains("sec-websocket-protocol: superchat"),
        "second Sec-WebSocket-Protocol value must be relayed (append, not replace), got: {head}"
    );
}

// ---------------------------------------------------------------------------
// Auth plugins (DESIGN slice 5)
// ---------------------------------------------------------------------------

/// Register an upstream (with optional auth) + one GET route, returning the
/// upstream id.
fn register_upstream(
    svc: &Arc<ControlPlaneService>,
    alias: &str,
    port: u16,
    auth: Option<AuthConfig>,
) -> Uuid {
    let mut u = upstream(alias, port);
    u.auth = auth;
    let u = svc.create_upstream(tenant(), u).unwrap();
    svc.create_route(tenant(), route(u.id, &["GET"], "/v1/chat"))
        .unwrap();
    u.id
}

#[tokio::test]
async fn apikey_header_injects_credential_from_cred_store() {
    let upstream_mock = MockServer::start();
    let m = upstream_mock.mock(|when, then| {
        when.method(GET)
            .path("/v1/chat")
            .header("x-api-key", "sk-1234");
        then.status(200).body("ok");
    });

    let svc = service(test_config());
    let auth = AuthConfig {
        r#type: API_KEY_AUTH_PLUGIN_ID.to_owned(),
        sharing: SharingMode::default(),
        config: serde_json::json!({
            "value_ref": "cred://partner-key",
            "name": "x-api-key",
            "in": "header",
        }),
    };
    register_upstream(&svc, "mock-api", upstream_mock.port(), Some(auth));

    let registry = registry_with(MockCredStoreClient::with_secrets(vec![(
        "cred://partner-key".to_owned(),
        "sk-1234".to_owned(),
    )]));
    let ctx = ctx_for(TENANT, 7);
    let (status, _, _) = proxy_with(
        &svc,
        "mock-api",
        "v1/chat",
        Method::GET,
        HeaderMap::new(),
        Body::empty(),
        &ctx,
        Some(&registry),
    )
    .await;
    m.assert();
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn apikey_query_injects_credential() {
    let upstream_mock = MockServer::start();
    let m = upstream_mock.mock(|when, then| {
        when.method(GET)
            .path("/v1/chat")
            .query_param("api_key", "sk-1234");
        then.status(200).body("ok");
    });

    let svc = service(test_config());
    let auth = AuthConfig {
        r#type: API_KEY_AUTH_PLUGIN_ID.to_owned(),
        sharing: SharingMode::default(),
        config: serde_json::json!({
            "value_ref": "cred://partner-key",
            "name": "api_key",
            "in": "query",
        }),
    };
    register_upstream(&svc, "mock-api", upstream_mock.port(), Some(auth));

    let registry = registry_with(MockCredStoreClient::with_secrets(vec![(
        "cred://partner-key".to_owned(),
        "sk-1234".to_owned(),
    )]));
    let ctx = ctx_for(TENANT, 7);
    let (status, _, _) = proxy_with(
        &svc,
        "mock-api",
        "v1/chat",
        Method::GET,
        HeaderMap::new(),
        Body::empty(),
        &ctx,
        Some(&registry),
    )
    .await;
    m.assert();
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn apikey_missing_secret_is_500_secret_not_found() {
    let upstream_mock = MockServer::start();
    let svc = service(test_config());
    let auth = AuthConfig {
        r#type: API_KEY_AUTH_PLUGIN_ID.to_owned(),
        sharing: SharingMode::default(),
        config: serde_json::json!({
            "value_ref": "cred://ghost",
            "name": "x-api-key",
        }),
    };
    register_upstream(&svc, "mock-api", upstream_mock.port(), Some(auth));

    // Empty store: every `get` resolves to Ok(None).
    let registry = registry_with(MockCredStoreClient::empty());
    let ctx = ctx_for(TENANT, 7);
    let (status, hdrs, body) = proxy_with(
        &svc,
        "mock-api",
        "v1/chat",
        Method::GET,
        HeaderMap::new(),
        Body::empty(),
        &ctx,
        Some(&registry),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        problem_type(status, &hdrs, &body),
        "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1"
    );
}

#[tokio::test]
async fn catalog_only_auth_plugins_are_unknown_plugin_503() {
    for id in [BASIC_AUTH_PLUGIN_ID, BEARER_AUTH_PLUGIN_ID] {
        let upstream_mock = MockServer::start();
        let svc = service(test_config());
        let auth = AuthConfig {
            r#type: id.to_owned(),
            sharing: SharingMode::default(),
            config: serde_json::json!({}),
        };
        register_upstream(&svc, "mock-api", upstream_mock.port(), Some(auth));

        let registry = registry_with(MockCredStoreClient::empty());
        let ctx = ctx_for(TENANT, 7);
        let (status, hdrs, body) = proxy_with(
            &svc,
            "mock-api",
            "v1/chat",
            Method::GET,
            HeaderMap::new(),
            Body::empty(),
            &ctx,
            Some(&registry),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            problem_type(status, &hdrs, &body),
            "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1"
        );
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json["detail"]
                .as_str()
                .unwrap()
                .contains("unknown auth plugin"),
            "detail: {}",
            json["detail"]
        );
    }
}

#[tokio::test]
async fn noop_auth_passes_through_without_credentials() {
    let upstream_mock = MockServer::start();
    let m = upstream_mock.mock(|when, then| {
        when.method(GET)
            .path("/v1/chat")
            .header_missing("authorization");
        then.status(200).body("ok");
    });

    let svc = service(test_config());
    let auth = AuthConfig {
        r#type: NOOP_AUTH_PLUGIN_ID.to_owned(),
        sharing: SharingMode::default(),
        config: serde_json::json!({}),
    };
    register_upstream(&svc, "mock-api", upstream_mock.port(), Some(auth));

    let registry = registry_with(MockCredStoreClient::empty());
    let ctx = ctx_for(TENANT, 7);
    let (status, _, _) = proxy_with(
        &svc,
        "mock-api",
        "v1/chat",
        Method::GET,
        HeaderMap::new(),
        Body::empty(),
        &ctx,
        Some(&registry),
    )
    .await;
    m.assert();
    assert_eq!(status, StatusCode::OK);
}

/// `OAuth2` Form variant: exchanges credentials at the token endpoint once, then
/// serves cached tokens without further `IdP` calls (ADR 0008).
#[tokio::test]
async fn oauth2_client_cred_form_exchanges_once_and_caches() {
    let token_mock = MockServer::start();
    let token = token_mock.mock(|when, then| {
        when.method(POST)
            .path("/token")
            .form_urlencoded_tuple("grant_type", "client_credentials")
            .form_urlencoded_tuple("client_id", "cid")
            .form_urlencoded_tuple("client_secret", "csecret");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"access_token":"abc.def","expires_in":3600,"token_type":"Bearer"}"#);
    });

    let upstream_mock = MockServer::start();
    let api = upstream_mock.mock(|when, then| {
        when.method(GET)
            .path("/v1/chat")
            .header("authorization", "Bearer abc.def");
        then.status(200).body("ok");
    });

    let svc = service(test_config());
    let auth = AuthConfig {
        r#type: OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID.to_owned(),
        sharing: SharingMode::default(),
        config: serde_json::json!({
            "token_endpoint": token_mock.url("/token"),
            "client_id_ref": "cred://client-id",
            "client_secret_ref": "cred://client-secret",
            "scopes": "",
        }),
    };
    register_upstream(&svc, "mock-api", upstream_mock.port(), Some(auth));

    let registry = registry_with(MockCredStoreClient::with_secrets(vec![
        ("cred://client-id".to_owned(), "cid".to_owned()),
        ("cred://client-secret".to_owned(), "csecret".to_owned()),
    ]));
    let ctx = ctx_for(TENANT, 42);

    let (status, _, _) = proxy_with(
        &svc,
        "mock-api",
        "v1/chat",
        Method::GET,
        HeaderMap::new(),
        Body::empty(),
        &ctx,
        Some(&registry),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Second request, same identity: served from the token cache.
    let (status, _, _) = proxy_with(
        &svc,
        "mock-api",
        "v1/chat",
        Method::GET,
        HeaderMap::new(),
        Body::empty(),
        &ctx,
        Some(&registry),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    token.assert_calls(1);
    api.assert_calls(2);
}

/// `OAuth2` Basic variant: client credentials travel in `Authorization: Basic`,
/// not in the form body.
#[tokio::test]
async fn oauth2_client_cred_basic_uses_basic_auth_header() {
    let token_mock = MockServer::start();
    let token = token_mock.mock(|when, then| {
        when.method(POST)
            .path("/token")
            // Credentials travel in `Authorization` (Basic), not the form body.
            .header_exists("authorization")
            .form_urlencoded_tuple("grant_type", "client_credentials")
            .form_urlencoded_tuple_missing("client_secret");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"access_token":"tok-basic","expires_in":3600,"token_type":"Bearer"}"#);
    });

    let upstream_mock = MockServer::start();
    let api = upstream_mock.mock(|when, then| {
        when.method(GET)
            .path("/v1/chat")
            .header("authorization", "Bearer tok-basic");
        then.status(200).body("ok");
    });

    let svc = service(test_config());
    let auth = AuthConfig {
        r#type: OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID.to_owned(),
        sharing: SharingMode::default(),
        config: serde_json::json!({
            "token_endpoint": token_mock.url("/token"),
            "client_id_ref": "cred://client-id",
            "client_secret_ref": "cred://client-secret",
        }),
    };
    register_upstream(&svc, "mock-api", upstream_mock.port(), Some(auth));

    let registry = registry_with(MockCredStoreClient::with_secrets(vec![
        ("cred://client-id".to_owned(), "cid".to_owned()),
        ("cred://client-secret".to_owned(), "csecret".to_owned()),
    ]));
    let ctx = ctx_for(TENANT, 43);
    let (status, _, _) = proxy_with(
        &svc,
        "mock-api",
        "v1/chat",
        Method::GET,
        HeaderMap::new(),
        Body::empty(),
        &ctx,
        Some(&registry),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    token.assert_calls(1);
    api.assert_calls(1);
}

#[tokio::test]
async fn oauth2_missing_client_secret_is_500() {
    let token_mock = MockServer::start();
    let svc = service(test_config());
    let auth = AuthConfig {
        r#type: OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID.to_owned(),
        sharing: SharingMode::default(),
        config: serde_json::json!({
            "token_endpoint": token_mock.url("/token"),
            "client_id_ref": "cred://client-id",
            "client_secret_ref": "cred://client-secret",
        }),
    };
    register_upstream(&svc, "mock-api", 1, Some(auth));

    // Only the client id exists; the client secret reference is missing.
    let registry = registry_with(MockCredStoreClient::with_secrets(vec![(
        "cred://client-id".to_owned(),
        "cid".to_owned(),
    )]));
    let ctx = ctx_for(TENANT, 44);
    let (status, hdrs, body) = proxy_with(
        &svc,
        "mock-api",
        "v1/chat",
        Method::GET,
        HeaderMap::new(),
        Body::empty(),
        &ctx,
        Some(&registry),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        problem_type(status, &hdrs, &body),
        "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1"
    );
}

#[tokio::test]
async fn oauth2_token_exchange_rejected_is_401_auth_failed() {
    let token_mock = MockServer::start();
    // The token endpoint rejects the credentials.
    token_mock.mock(|when, then| {
        when.method(POST).path("/token");
        then.status(401).body("invalid_client");
    });

    let svc = service(test_config());
    let auth = AuthConfig {
        r#type: OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID.to_owned(),
        sharing: SharingMode::default(),
        config: serde_json::json!({
            "token_endpoint": token_mock.url("/token"),
            "client_id_ref": "cred://client-id",
            "client_secret_ref": "cred://client-secret",
        }),
    };
    register_upstream(&svc, "mock-api", 1, Some(auth));

    let registry = registry_with(MockCredStoreClient::with_secrets(vec![
        ("cred://client-id".to_owned(), "bad".to_owned()),
        ("cred://client-secret".to_owned(), "bad".to_owned()),
    ]));
    let ctx = ctx_for(TENANT, 45);
    let (status, hdrs, body) = proxy_with(
        &svc,
        "mock-api",
        "v1/chat",
        Method::GET,
        HeaderMap::new(),
        Body::empty(),
        &ctx,
        Some(&registry),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        problem_type(status, &hdrs, &body),
        "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1"
    );
}

// ---------------------------------------------------------------------------
// Rate limiting + guard plugins (DESIGN slice 6)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rate_limit_rejects_with_429_retry_after_and_ratelimit_headers() {
    let mock = MockServer::start();
    let m = mock.mock(|when, then| {
        when.method(GET).path("/v1/chat");
        then.status(200).body("ok");
    });

    let svc = service(test_config());
    let mut up = upstream("rl", mock.port());
    up.rate_limit = Some(per_second(2));
    let u = svc.create_upstream(tenant(), up).unwrap();
    svc.create_route(tenant(), route(u.id, &["GET"], "/v1/chat"))
        .unwrap();

    let limiter = RateLimiter::new();
    let ctx = ctx_for(TENANT, 90);

    // Burst allowance: the first 2 requests pass.
    for _ in 0..2 {
        let (status, _, _) = proxy_rate(
            &svc,
            "rl",
            "v1/chat",
            Method::GET,
            HeaderMap::new(),
            Body::empty(),
            &ctx,
            &limiter,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    m.assert_calls(2);

    // The 3rd within the same window → 429 + RFC 6585 / rate-limit headers.
    let (status, hdrs, body) = proxy_rate(
        &svc,
        "rl",
        "v1/chat",
        Method::GET,
        HeaderMap::new(),
        Body::empty(),
        &ctx,
        &limiter,
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        problem_type(status, &hdrs, &body),
        "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1"
    );
    let retry: u64 = hdrs
        .get("retry-after")
        .unwrap()
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(retry >= 1, "Retry-After must be positive, got {retry}");
    assert_eq!(hdrs.get("x-ratelimit-limit").unwrap(), "2");
    assert_eq!(hdrs.get("x-ratelimit-remaining").unwrap(), "0");
    let reset: u64 = hdrs
        .get("x-ratelimit-reset")
        .unwrap()
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(reset > 0, "X-RateLimit-Reset must be a future epoch");
    // The problem body carries the retry-guidance extension (DESIGN).
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["retry_after_seconds"], retry);
    assert_eq!(json["alias"], "rl");
    // Nothing reached the upstream for the rejected request.
    m.assert_calls(2);
}

#[tokio::test]
async fn route_rate_limit_wins_as_the_stricter_limit() {
    let mock = MockServer::start();
    let m = mock.mock(|when, then| {
        when.method(GET).path("/v1/chat");
        then.status(200).body("ok");
    });

    let svc = service(test_config());
    let mut up = upstream("rl2", mock.port());
    up.rate_limit = Some(per_second(100)); // generous upstream
    let u = svc.create_upstream(tenant(), up).unwrap();
    let mut r = route(u.id, &["GET"], "/v1/chat");
    r.rate_limit = Some(per_second(2)); // strict route → effective limit 2/s
    svc.create_route(tenant(), r).unwrap();

    let limiter = RateLimiter::new();
    let ctx = ctx_for(TENANT, 91);
    for _ in 0..2 {
        let (status, _, _) = proxy_rate(
            &svc,
            "rl2",
            "v1/chat",
            Method::GET,
            HeaderMap::new(),
            Body::empty(),
            &ctx,
            &limiter,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    let (status, hdrs, _) = proxy_rate(
        &svc,
        "rl2",
        "v1/chat",
        Method::GET,
        HeaderMap::new(),
        Body::empty(),
        &ctx,
        &limiter,
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(hdrs.get("x-ratelimit-limit").unwrap(), "2");
    m.assert_calls(2);
}

#[tokio::test]
async fn rate_limiter_is_fail_open_when_unconfigured() {
    let mock = MockServer::start();
    let m = mock.mock(|when, then| {
        when.method(GET).path("/v1/chat");
        then.status(200).body("ok");
    });

    let svc = service(test_config());
    let u = svc
        .create_upstream(tenant(), upstream("norl", mock.port()))
        .unwrap();
    svc.create_route(tenant(), route(u.id, &["GET"], "/v1/chat"))
        .unwrap();

    // A rate limiter is wired but the upstream/route configure nothing.
    let limiter = RateLimiter::new();
    let ctx = ctx_for(TENANT, 92);
    for _ in 0..5 {
        let (status, _, _) = proxy_rate(
            &svc,
            "norl",
            "v1/chat",
            Method::GET,
            HeaderMap::new(),
            Body::empty(),
            &ctx,
            &limiter,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    m.assert_calls(5);
}

#[tokio::test]
async fn bound_required_headers_guard_fails_open_at_runtime() {
    let mock = MockServer::start();
    let m = mock.mock(|when, then| {
        when.method(GET).path("/v1/chat");
        then.status(200).body("ok");
    });

    let svc = service(test_config());
    let mut up = upstream("guarded", mock.port());
    // Binding the guard by GTS ID runs it in the data plane; the MVP model
    // carries no per-plugin config object, so it is fail-open (ADR 0009) and
    // the request passes through unchanged.
    up.plugins.items = vec![REQUIRED_HEADERS_GUARD_PLUGIN_ID.to_owned()];
    let u = svc.create_upstream(tenant(), up).unwrap();
    svc.create_route(tenant(), route(u.id, &["GET"], "/v1/chat"))
        .unwrap();

    let (status, _, _) = proxy_with(
        &svc,
        "guarded",
        "v1/chat",
        Method::GET,
        HeaderMap::new(),
        Body::empty(),
        &ctx_for(TENANT, 93),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    m.assert();
}

/// Issue a raw HTTP/1.1 GET over a TCP stream and return (status, body).
async fn raw_get(addr: std::net::SocketAddr, path: &str) -> (u16, String) {
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    (status, text.into_owned())
}

#[tokio::test]
async fn rate_limit_enforced_over_the_rest_router() {
    let mock = MockServer::start();
    let m = mock.mock(|when, then| {
        when.method(GET).path("/v1/chat");
        then.status(200).body("ok");
    });

    let svc = service(test_config());
    let mut up = upstream("web", mock.port());
    up.rate_limit = Some(per_second(1));
    let u = svc.create_upstream(tenant(), up).unwrap();
    svc.create_route(tenant(), route(u.id, &["GET"], "/v1/chat"))
        .unwrap();

    let registry = NoopOpenApiRegistry;
    let limiter = Arc::new(RateLimiter::new());
    let app = register_routes(Router::new(), &registry, svc, None, None, Some(limiter))
        .layer(axum::middleware::from_fn(inject_security_ctx));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _result = axum::serve(listener, app.into_make_service()).await;
    });

    // First request consumes the single token; the second is rejected with a
    // 429 the client can see over the wire (handler → engine wiring).
    let (s1, body1) = raw_get(addr, "/oagw/v1/proxy/web/v1/chat").await;
    assert_eq!(s1, 200);
    // Rf-016: even a successful proxied response carries the generated
    // correlation id (the handler's `x-request-id`).
    assert!(
        body1.to_ascii_lowercase().contains("x-request-id:"),
        "200 proxy response must echo a correlation id, got: {body1}"
    );
    let (s2, body) = raw_get(addr, "/oagw/v1/proxy/web/v1/chat").await;
    assert_eq!(s2, 429);
    assert!(
        body.to_ascii_lowercase().contains("retry-after:"),
        "429 response must carry Retry-After, got: {body}"
    );
    m.assert_calls(1);
}

// ---------------------------------------------------------------------------
// CORS (DESIGN slice 7, ADR 0004)
// ---------------------------------------------------------------------------

fn cors_config(
    allowed_origins: Vec<&str>,
    allowed_methods: Vec<&str>,
    allow_credentials: bool,
) -> oagw::domain::models::CorsConfig {
    oagw::domain::models::CorsConfig {
        sharing: SharingMode::default(),
        enabled: true,
        allowed_origins: allowed_origins.into_iter().map(ToOwned::to_owned).collect(),
        allowed_methods: allowed_methods.into_iter().map(ToOwned::to_owned).collect(),
        expose_headers: vec!["X-Request-ID".to_owned()],
        allow_credentials,
    }
}

#[tokio::test]
async fn cors_preflight_returns_permissive_204_without_resolving_upstream() {
    let mock = MockServer::start();
    let m = mock.mock(|when, then| {
        when.method(OPTIONS).path("/v1/chat");
        then.status(204);
    });

    let svc = service(test_config());
    // Note: no upstream registered at all — the preflight must not resolve it.
    let mut headers = HeaderMap::new();
    headers.insert(
        "origin",
        HeaderValue::from_static("https://app.example.com"),
    );
    headers.insert(
        "access-control-request-method",
        HeaderValue::from_static("POST"),
    );
    headers.insert(
        "access-control-request-headers",
        HeaderValue::from_static("Content-Type, Authorization"),
    );
    let body = Body::empty();
    let resp = proxy_request(
        &svc,
        vec![tenant()],
        "ghost".to_owned(),
        "v1/chat".to_owned(),
        Method::OPTIONS,
        headers,
        body,
        None,
        None,
        None,
        None,
    )
    .await;
    let status = resp.status();
    let hdrs = resp.headers().clone();
    // Per ADR 0004 the preflight is answered locally; nothing hits the upstream.
    let _ = m;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        hdrs.get("access-control-allow-origin").unwrap(),
        "https://app.example.com"
    );
    assert_eq!(hdrs.get("access-control-allow-methods").unwrap(), "POST");
    assert_eq!(
        hdrs.get("access-control-allow-headers").unwrap(),
        "Content-Type, Authorization"
    );
    assert_eq!(hdrs.get("access-control-max-age").unwrap(), "86400");
    assert!(
        hdrs.get("vary")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("Origin")
    );
    assert_eq!(hdrs.get("x-oagw-error-source").unwrap(), "gateway");
}

#[tokio::test]
async fn cors_allowed_actual_request_adds_response_headers() {
    let mock = MockServer::start();
    let m = mock.mock(|when, then| {
        when.method(GET).path("/v1/chat");
        then.status(200).body("ok");
    });

    let svc = service(test_config());
    let mut up = upstream("cors", mock.port());
    up.cors = Some(cors_config(
        vec!["https://app.example.com", "https://admin.example.com"],
        vec!["GET", "POST"],
        true,
    ));
    let u = svc.create_upstream(tenant(), up).unwrap();
    svc.create_route(tenant(), route(u.id, &["GET"], "/v1/chat"))
        .unwrap();

    let mut headers = HeaderMap::new();
    headers.insert(
        "origin",
        HeaderValue::from_static("https://app.example.com"),
    );
    let (status, hdrs, _) =
        proxy(&svc, "cors", "v1/chat", Method::GET, headers, Body::empty()).await;
    m.assert();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        hdrs.get("access-control-allow-origin").unwrap(),
        "https://app.example.com"
    );
    assert_eq!(
        hdrs.get("access-control-expose-headers").unwrap(),
        "X-Request-ID"
    );
    assert_eq!(
        hdrs.get("access-control-allow-credentials").unwrap(),
        "true"
    );
    assert!(
        hdrs.get("vary")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("Origin")
    );
}

#[tokio::test]
async fn cors_disallowed_origin_is_403_before_upstream_call() {
    let mock = MockServer::start();
    let m = mock.mock(|when, then| {
        when.method(GET).path("/v1/chat");
        then.status(200).body("ok");
    });

    let svc = service(test_config());
    let mut up = upstream("cors2", mock.port());
    up.cors = Some(cors_config(
        vec!["https://app.example.com"],
        vec!["GET"],
        false,
    ));
    let u = svc.create_upstream(tenant(), up).unwrap();
    svc.create_route(tenant(), route(u.id, &["GET"], "/v1/chat"))
        .unwrap();

    let mut headers = HeaderMap::new();
    headers.insert("origin", HeaderValue::from_static("https://evil.com"));
    let (status, hdrs, body) = proxy(
        &svc,
        "cors2",
        "v1/chat",
        Method::GET,
        headers,
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        problem_type(status, &hdrs, &body),
        "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1"
    );
    m.assert_calls(0);
}

#[tokio::test]
async fn cors_disallowed_method_is_403_before_upstream_call() {
    let mock = MockServer::start();
    let m = mock.mock(|when, then| {
        when.method(DELETE).path("/v1/chat");
        then.status(200).body("ok");
    });

    let svc = service(test_config());
    let mut up = upstream("cors3", mock.port());
    up.cors = Some(cors_config(
        vec!["https://app.example.com"],
        vec!["GET"],
        false,
    ));
    let u = svc.create_upstream(tenant(), up).unwrap();
    // Route matches DELETE so the CORS method check (not route matching) is
    // what rejects it — DESIGN guard rules: route method membership first,
    // then CORS method allowlist for cross-origin requests.
    svc.create_route(tenant(), route(u.id, &["GET", "DELETE"], "/v1/chat"))
        .unwrap();

    let mut headers = HeaderMap::new();
    headers.insert(
        "origin",
        HeaderValue::from_static("https://app.example.com"),
    );
    let (status, hdrs, body) = proxy(
        &svc,
        "cors3",
        "v1/chat",
        Method::DELETE,
        headers,
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        problem_type(status, &hdrs, &body),
        "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1"
    );
    m.assert_calls(0);
}

#[tokio::test]
async fn cors_is_not_applied_to_non_cross_origin_requests() {
    let mock = MockServer::start();
    let m = mock.mock(|when, then| {
        when.method(GET).path("/v1/chat");
        then.status(200).body("ok");
    });

    let svc = service(test_config());
    let mut up = upstream("cors4", mock.port());
    up.cors = Some(cors_config(
        vec!["https://app.example.com"],
        vec!["GET"],
        false,
    ));
    let u = svc.create_upstream(tenant(), up).unwrap();
    svc.create_route(tenant(), route(u.id, &["GET"], "/v1/chat"))
        .unwrap();

    // No Origin header → the request is not cross-origin; CORS is irrelevant.
    let (status, hdrs, _) = proxy(
        &svc,
        "cors4",
        "v1/chat",
        Method::GET,
        HeaderMap::new(),
        Body::empty(),
    )
    .await;
    m.assert();
    assert_eq!(status, StatusCode::OK);
    assert!(hdrs.get("access-control-allow-origin").is_none());
}

// ---------------------------------------------------------------------------
// Rf-020 regression tests (semantic review: tenant chain, enable/disable,
// header set/add/remove CRUD, guards, timeout, WebSocket keys, path
// normalization, rate-limit reconfiguration, oauth2 single-flight)
// ---------------------------------------------------------------------------

/// A raw-TCP upstream that reads every request head (raw), appends it to a
/// shared buffer, and answers 200. Returns `(addr, captured_heads)`.
async fn spawn_capture_upstream() -> (std::net::SocketAddr, Arc<Mutex<String>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(String::new()));
    let captured_for_task = captured.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let captured_ref = captured_for_task.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = stream.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    head.extend_from_slice(&chunk[..n]);
                    if head.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let text = String::from_utf8_lossy(&head).into_owned();
                captured_ref.lock().unwrap().push_str(&text);
                let _res = stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                    )
                    .await;
            });
        }
    });
    (addr, captured)
}

/// Run a proxy request with an explicit tenant chain (`chain[0]` = caller);
/// the gateway resolves aliases descendant → root.
#[allow(
    clippy::too_many_arguments,
    reason = "helper mirrors the full proxy_request argument list"
)]
async fn proxy_with_chain(
    svc: &Arc<ControlPlaneService>,
    chain: &[Uuid],
    alias: &str,
    path: &str,
    method: Method,
    headers: HeaderMap,
    body: Body,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let ctx = SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(chain[0])
        .build()
        .unwrap();
    let resp = proxy_request(
        svc,
        chain.to_vec(),
        alias.to_owned(),
        path.to_owned(),
        method,
        headers,
        body,
        None,
        Some(&ctx),
        None,
        None,
    )
    .await;
    let status = resp.status();
    let hdrs = resp.headers().clone();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, hdrs, bytes)
}

/// Register an alias + route in the **ancestor** tenant (different from
/// `TENANT`), returning a mock that records whether that copy was hit.
fn register_ancestor(
    svc: &Arc<ControlPlaneService>,
    tenant_id: u128,
    alias: &str,
    port: u16,
    enabled: bool,
) -> httpmock::Mock<'static> {
    // Leak the server handle so the returned `Mock<'static>` (which borrows
    // it) can escape the helper; mock servers live for the whole test process.
    let server: &'static MockServer = Box::leak(Box::new(MockServer::start()));
    let m = server.mock(|when, then| {
        when.method(GET).path("/v1/chat");
        then.status(200).body("ok");
    });
    let mut u = upstream(alias, port);
    u.enabled = enabled;
    let u = svc
        .create_upstream(Uuid::from_u128(tenant_id), u)
        .unwrap();
    svc.create_route(
        Uuid::from_u128(tenant_id),
        route(u.id, &["GET"], "/v1/chat"),
    )
    .unwrap();
    m
}

#[tokio::test]
async fn tenant_chain_shadows_alias_to_the_descendant_copy() {
    let descendant_server = MockServer::start();
    let descendant_m = descendant_server.mock(|when, then| {
        when.method(GET).path("/v1/chat");
        then.status(200).body("ok");
    });
    let svc = service(test_config());
    let ancestor_m = register_ancestor(&svc, 2, "m", 1, true);
    let u = svc
        .create_upstream(tenant(), upstream("m", descendant_server.port()))
        .unwrap();
    svc.create_route(tenant(), route(u.id, &["GET"], "/v1/chat"))
        .unwrap();

    let chain = vec![tenant(), Uuid::from_u128(2)];
    let (status, _, body) = proxy_with_chain(
        &svc,
        &chain,
        "m",
        "v1/chat",
        Method::GET,
        HeaderMap::new(),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"ok");
    // The descendant's copy won the closest-wins resolution.
    descendant_m.assert_calls(1);
    ancestor_m.assert_calls(0);
}

#[tokio::test]
async fn tenant_chain_disabled_descendant_is_503_not_ancestor_fallthrough() {
    let svc = service(test_config());
    let ancestor_m = register_ancestor(&svc, 2, "m", 1, true);
    // The descendant owns a *disabled* copy: it may not fall through to the
    // ancestor's enabled one (PRD cpt-cf-oagw-fr-enable-disable).
    let mut d = upstream("m", 1);
    d.enabled = false;
    let d = svc.create_upstream(tenant(), d).unwrap();
    svc.create_route(tenant(), route(d.id, &["GET"], "/v1/chat"))
        .unwrap();

    let chain = vec![tenant(), Uuid::from_u128(2)];
    let (status, hdrs, body) = proxy_with_chain(
        &svc,
        &chain,
        "m",
        "v1/chat",
        Method::GET,
        HeaderMap::new(),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        problem_type(status, &hdrs, &body),
        "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1"
    );
    ancestor_m.assert_calls(0);
}

#[tokio::test]
async fn tenant_chain_disabled_ancestor_is_503() {
    let svc = service(test_config());
    register_ancestor(&svc, 2, "m", 1, false);

    let chain = vec![tenant(), Uuid::from_u128(2)];
    let (status, hdrs, body) = proxy_with_chain(
        &svc,
        &chain,
        "m",
        "v1/chat",
        Method::GET,
        HeaderMap::new(),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        problem_type(status, &hdrs, &body),
        "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1"
    );
}

#[tokio::test]
async fn disabled_route_is_404_while_enabled_sibling_still_forwards() {
    let mock = MockServer::start();
    let m = mock.mock(|when, then| {
        when.method(GET).path("/v1/other");
        then.status(200).body("ok");
    });
    let svc = service(test_config());
    let u = svc
        .create_upstream(tenant(), upstream("r", mock.port()))
        .unwrap();
    let mut off = route(u.id, &["GET"], "/v1/chat");
    off.enabled = false;
    svc.create_route(tenant(), off).unwrap();
    svc.create_route(tenant(), route(u.id, &["GET"], "/v1/other"))
        .unwrap();

    // The disabled route must not match.
    let (status, hdrs, body) = proxy(
        &svc,
        "r",
        "v1/chat",
        Method::GET,
        HeaderMap::new(),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        problem_type(status, &hdrs, &body),
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
    );

    // Its enabled sibling still forwards.
    let (status, _, _) = proxy(
        &svc,
        "r",
        "v1/other",
        Method::GET,
        HeaderMap::new(),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    m.assert();
}

#[tokio::test]
async fn response_direction_set_add_remove_apply_on_the_forwarded_response() {
    let mock = MockServer::start();
    mock.mock(|when, then| {
        when.method(GET).path("/v1/chat");
        then.status(200)
            .header("x-resp-remove", "gone")
            .header("x-resp-keep", "kept")
            .body("ok");
    });

    let svc = service(test_config());
    let mut u = upstream("ro", mock.port());
    u.headers.response = HeaderOps {
        set: BTreeMap::from([("x-resp-set".to_owned(), "s".to_owned())]),
        add: BTreeMap::from([("x-resp-add".to_owned(), "a".to_owned())]),
        remove: vec!["x-resp-remove".to_owned()],
    };
    let u = svc.create_upstream(tenant(), u).unwrap();
    svc.create_route(tenant(), route(u.id, &["GET"], "/v1/chat"))
        .unwrap();

    let (status, hdrs, _) = proxy(
        &svc,
        "ro",
        "v1/chat",
        Method::GET,
        HeaderMap::new(),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // set replaces/normalizes, add appends, remove strips.
    assert_eq!(hdrs.get("x-resp-set").unwrap(), "s");
    assert_eq!(hdrs.get("x-resp-add").unwrap(), "a");
    assert_eq!(hdrs.get("x-resp-keep").unwrap(), "kept");
    assert!(hdrs.get("x-resp-remove").is_none());
}

#[tokio::test]
async fn auth_injected_authorization_replaces_inbound_authorization() {
    let upstream_mock = MockServer::start();
    let am = upstream_mock.mock(|when, then| {
        when.method(GET)
            .path("/v1/chat")
            .header("authorization", "Bearer tok-hl");
        then.status(200).body("ok");
    });

    let svc = service(test_config());
    // Passthrough All: an inbound (leaked) `Authorization` would normally be
    // forwarded — the injected credential must replace it, not ride alongside.
    let mut u = upstream("auth", upstream_mock.port());
    u.headers.request.passthrough = PassthroughMode::All;
    let u = svc.create_upstream(tenant(), u).unwrap();
    svc.create_route(tenant(), route(u.id, &["GET"], "/v1/chat"))
        .unwrap();
    let auth = AuthConfig {
        r#type: API_KEY_AUTH_PLUGIN_ID.to_owned(),
        sharing: SharingMode::default(),
        config: serde_json::json!({
            "value_ref": "cred://apikey",
            "name": "authorization",
            "in": "header",
        }),
    };
    svc.update_upstream(tenant(), u.id, {
        let mut u = u.clone();
        u.auth = Some(auth);
        u
    })
    .unwrap();

    let registry = registry_with(MockCredStoreClient::with_secrets(vec![(
        "cred://apikey".to_owned(),
        "Bearer tok-hl".to_owned(),
    )]));
    let ctx = ctx_for(TENANT, 47);
    let mut headers = HeaderMap::new();
    headers.insert("authorization", HeaderValue::from_static("Basic bGVha2Vk"));
    let (status, _, _) = proxy_with(
        &svc,
        "auth",
        "v1/chat",
        Method::GET,
        headers,
        Body::empty(),
        &ctx,
        Some(&registry),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // The upstream saw exactly one Authorization value: the injected bearer.
    am.assert_calls(1);
}

#[tokio::test]
async fn unresolvable_guard_plugin_fails_open_and_request_proceeds() {
    let mock = MockServer::start();
    let m = mock.mock(|when, then| {
        when.method(GET).path("/v1/chat");
        then.status(200).body("ok");
    });

    let svc = service(test_config());
    let mut up = upstream("guarded", mock.port());
    // A plugin id that no registry knows about: the guard must fail open
    // (ADR 0009), never block the request.
    up.plugins.items = vec![Uuid::new_v4().to_string()];
    let u = svc.create_upstream(tenant(), up).unwrap();
    svc.create_route(tenant(), route(u.id, &["GET"], "/v1/chat"))
        .unwrap();

    let (status, _, _) = proxy(
        &svc,
        "guarded",
        "v1/chat",
        Method::GET,
        HeaderMap::new(),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    m.assert();
}

#[tokio::test]
async fn upstream_timeout_is_504_gateway_timeout() {
    // A TCP upstream that accepts a connection and then never answers.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ok = listener.accept().await;
        // Hold the connection open forever (never write a response).
        std::future::pending::<()>().await;
    });

    let mut cfg = test_config();
    cfg.proxy_timeout_secs = 1;
    let svc = service(cfg);
    let u = svc
        .create_upstream(tenant(), upstream("slow", addr.port()))
        .unwrap();
    svc.create_route(tenant(), route(u.id, &["GET"], "/v1/chat"))
        .unwrap();

    let (status, hdrs, body) = proxy(
        &svc,
        "slow",
        "v1/chat",
        Method::GET,
        HeaderMap::new(),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(
        problem_type(status, &hdrs, &body),
        "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1"
    );
}

#[tokio::test]
async fn encoded_and_dot_segment_paths_are_normalized_when_forwarded() {
    // Dot-segment traversal: /v1/chat/../summarize must reach the upstream as
    // /v1/summarize (no literal `..`, no 502).
    let (dot_addr, dot_captured) = spawn_capture_upstream().await;
    let svc = service(test_config());
    let u = svc
        .create_upstream(tenant(), upstream("px", dot_addr.port()))
        .unwrap();
    svc.create_route(tenant(), route(u.id, &["GET"], "/v1"))
        .unwrap();

    let (status, _, body) = proxy(
        &svc,
        "px",
        "v1/chat/../summarize",
        Method::GET,
        HeaderMap::new(),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"ok");
    let dot_head = dot_captured.lock().unwrap().clone();
    assert!(
        dot_head.contains("GET /v1/summarize HTTP/1.1"),
        "dot segments must be collapsed, got: {dot_head:?}"
    );
    assert!(!dot_head.contains(".."), "no literal `..` may be forwarded");

    // Encoded path (axum delivers the decoded suffix): spaces and non-ASCII
    // must be re-encoded once on the wire.
    let (enc_addr, enc_captured) = spawn_capture_upstream().await;
    let svc = service(test_config());
    let u = svc
        .create_upstream(tenant(), upstream("enc", enc_addr.port()))
        .unwrap();
    svc.create_route(tenant(), route(u.id, &["GET"], "/v1"))
        .unwrap();
    let (status, _, _) = proxy(
        &svc,
        "enc",
        "v1/chat/h\u{e9}llo world",
        Method::GET,
        HeaderMap::new(),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let enc_head = enc_captured.lock().unwrap().clone();
    assert!(
        enc_head.contains("GET /v1/chat/h%C3%A9llo%20world HTTP/1.1"),
        "decoded path must be re-encoded once, got: {enc_head:?}"
    );
}

#[tokio::test]
async fn rate_limit_reconfiguration_resets_the_bucket_immediately() {
    let mock = MockServer::start();
    let m = mock.mock(|when, then| {
        when.method(GET).path("/v1/chat");
        then.status(200).body("ok");
    });

    let svc = service(test_config());
    let mut up = upstream("re", mock.port());
    up.rate_limit = Some(per_second(1));
    let u = svc.create_upstream(tenant(), up).unwrap();
    svc.create_route(tenant(), route(u.id, &["GET"], "/v1/chat"))
        .unwrap();
    let limiter = RateLimiter::new();
    let ctx = ctx_for(TENANT, 91);

    // Consume the single initial token.
    let (status, _, _) = proxy_rate(
        &svc,
        "re",
        "v1/chat",
        Method::GET,
        HeaderMap::new(),
        Body::empty(),
        &ctx,
        &limiter,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Reconfigure with a different plan: the drift check resets the bucket so
    // the new burst (2) is available right away, not after the old window.
    let mut updated = u.clone();
    updated.rate_limit = Some(per_second(2));
    svc.update_upstream(tenant(), u.id, updated).unwrap();
    for _ in 0..2 {
        let (status, _, _) = proxy_rate(
            &svc,
            "re",
            "v1/chat",
            Method::GET,
            HeaderMap::new(),
            Body::empty(),
            &ctx,
            &limiter,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    // 3 upstream calls total (1 pre-reconfig + 2 post-reconfig); the next
    // request is limited.
    let (status, _, _) = proxy_rate(
        &svc,
        "re",
        "v1/chat",
        Method::GET,
        HeaderMap::new(),
        Body::empty(),
        &ctx,
        &limiter,
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    m.assert_calls(3);
}

/// A raw-TCP token endpoint that counts received requests (one per in-flight
/// exchange) and answers 200 after a short delay, so concurrent first-requests
/// overlap instead of completing from an already-cached entry.
async fn spawn_delayed_token_server(
    delay: Duration,
) -> (std::net::SocketAddr, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let count_for_task = count.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let count_ref = count_for_task.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = stream.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    head.extend_from_slice(&chunk[..n]);
                    if head.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                if head.is_empty() {
                    return;
                }
                count_ref.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(delay).await;
                let body =
                    br#"{"access_token":"stampede","expires_in":3600,"token_type":"Bearer"}"#;
                let head_resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _res = stream.write_all(head_resp.as_bytes()).await;
                let _res = stream.write_all(body).await;
            });
        }
    });
    (addr, count)
}

#[tokio::test]
async fn oauth2_concurrent_first_requests_issue_exactly_one_token_exchange() {
    let (token_addr, token_hits) =
        spawn_delayed_token_server(Duration::from_millis(300)).await;

    let upstream_mock = MockServer::start();
    let api = upstream_mock.mock(|when, then| {
        when.method(GET)
            .path("/v1/chat")
            .header("authorization", "Bearer stampede");
        then.status(200).body("ok");
    });

    let svc = service(test_config());
    let auth = AuthConfig {
        r#type: OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID.to_owned(),
        sharing: SharingMode::default(),
        config: serde_json::json!({
            "token_endpoint": format!("http://127.0.0.1:{}/token", token_addr.port()),
            "client_id_ref": "cred://client-id",
            "client_secret_ref": "cred://client-secret",
            "scopes": "",
        }),
    };
    register_upstream(&svc, "mock-api", upstream_mock.port(), Some(auth));

    let registry = registry_with(MockCredStoreClient::with_secrets(vec![
        ("cred://client-id".to_owned(), "cid".to_owned()),
        ("cred://client-secret".to_owned(), "csecret".to_owned()),
    ]));
    let ctx = ctx_for(TENANT, 48);

    // N concurrent first-requests, same identity → same cache key. The slow
    // token endpoint keeps them all in flight so the single-flight path is
    // exercised: exactly one IdP exchange for the stampede.
    let mut futs = Vec::new();
    for _ in 0..8 {
        futs.push(proxy_with(
            &svc,
            "mock-api",
            "v1/chat",
            Method::GET,
            HeaderMap::new(),
            Body::empty(),
            &ctx,
            Some(&registry),
        ));
    }
    let results = futures_util::future::join_all(futs).await;
    for (status, _, _) in results {
        assert_eq!(status, StatusCode::OK);
    }
    assert_eq!(token_hits.load(Ordering::SeqCst), 1);
    api.assert_calls(8);
}
