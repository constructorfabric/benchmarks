//! Integration tests for the plain-HTTP proxy data plane
//! (`cpt-cf-oagw-feature-http-proxy`).
//!
//! Drives a real `axum::Router` (assembled the same way `OagwGear::register_rest`
//! assembles it) with `tower::ServiceExt::oneshot`, using `httpmock` as the
//! upstream so each test can assert both what the upstream received and what
//! the client got. Every §6 acceptance criterion is exercised here.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use axum::Extension;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use httpmock::MockServer;
use oagw::api::rest::routes::register_routes;
use oagw::config::OagwConfig;
use oagw::domain::model::{
    CorsConfig, Endpoint, HeadersConfig, HttpMatch, HttpMethod, MatchConfig, PathSuffixMode,
    Protocol, RequestHeaders, ResponseHeaders, Route, RouteMethod, Scheme, ServerConfig, Sharing,
    Upstream,
};
use oagw::state::ControlPlaneState;
use toolkit::api::openapi_registry::OpenApiRegistryImpl;
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

/// Builds a router over a fresh control-plane store and a fresh
/// `reqwest::Client`, with `config` and a `SecurityContext` asserting
/// `tenant_id` layered on, the same extension shapes
/// `OagwGear::register_rest` attaches.
fn router_for(state: Arc<ControlPlaneState>, config: OagwConfig, tenant_id: Uuid) -> Router {
    let openapi = OpenApiRegistryImpl::new();
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(config.proxy_timeout_secs.max(1)))
        .build()
        .expect("client must build");
    let ctx = SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(tenant_id)
        .build()
        .expect("security context must build");

    register_routes(Router::new(), &openapi)
        .layer(Extension(state))
        .layer(Extension(Arc::new(config)))
        .layer(Extension(Arc::new(client)))
        .layer(Extension(ctx))
}

fn default_config() -> OagwConfig {
    OagwConfig {
        proxy_timeout_secs: 2,
        allow_http_upstream: true,
        ..OagwConfig::default()
    }
}

fn http_endpoint(host: &str, port: u16) -> Endpoint {
    Endpoint {
        scheme: Scheme::Http,
        host: host.to_owned(),
        port: Some(port),
    }
}

fn mock_endpoint(server: &MockServer) -> Endpoint {
    http_endpoint(&server.host(), server.port())
}

fn upstream_with(alias: &str, endpoints: Vec<Endpoint>) -> Upstream {
    Upstream {
        id: Uuid::new_v4(),
        enabled: true,
        alias: alias.to_owned(),
        tags: Vec::new(),
        server: ServerConfig { endpoints },
        protocol: Protocol::Http,
        auth: None,
        headers: None,
        plugins: None,
        rate_limit: None,
        cors: None,
    }
}

fn route_with(
    upstream_id: Uuid,
    path: &str,
    methods: &[RouteMethod],
    query_allowlist: Vec<String>,
    path_suffix_mode: PathSuffixMode,
) -> Route {
    Route {
        id: Uuid::new_v4(),
        upstream_id,
        tags: Vec::new(),
        match_config: MatchConfig {
            http: Some(HttpMatch {
                methods: methods.to_vec(),
                path: path.to_owned(),
                query_allowlist,
                path_suffix_mode,
            }),
            grpc: None,
        },
        plugins: None,
        rate_limit: None,
        enabled: true,
        priority: 0,
    }
}

fn simple_route(upstream_id: Uuid, path: &str, methods: &[RouteMethod]) -> Route {
    route_with(
        upstream_id,
        path,
        methods,
        Vec::new(),
        PathSuffixMode::Append,
    )
}

/// Registers `upstream` and `route` under `tenant_id` in `state`.
fn install(state: &ControlPlaneState, tenant_id: Uuid, upstream: Upstream, route: Route) {
    state
        .tenant(tenant_id)
        .upstreams
        .insert(upstream.id, upstream);
    state.tenant(tenant_id).routes.insert(route.id, route);
}

async fn send(router: Router, request: Request<Body>) -> axum::response::Response {
    router
        .oneshot(request)
        .await
        .expect("router call must succeed")
}

async fn body_bytes(response: axum::response::Response) -> Vec<u8> {
    response
        .into_body()
        .collect()
        .await
        .expect("read body")
        .to_bytes()
        .to_vec()
}

async fn body_json(response: axum::response::Response) -> serde_json::Value {
    let bytes = body_bytes(response).await;
    serde_json::from_slice(&bytes).expect("body must be JSON")
}

fn error_source(response: &axum::response::Response) -> Option<String> {
    response
        .headers()
        .get("x-oagw-error-source")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

// ---------------------------------------------------------------------------
// Successful proxying.
// ---------------------------------------------------------------------------

// @cpt-begin:cpt-cf-oagw-dod-proxy-endpoint-registration:p1:inst-proxy-it-success-get-01
#[tokio::test]
async fn a_successful_get_returns_upstream_status_body_and_upstream_source() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/items");
        then.status(200).body(r#"{"ok":true}"#);
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let up = upstream_with("api.example.com", vec![mock_endpoint(&server)]);
    let route = simple_route(up.id, "/v1/items", &[RouteMethod::Get]);
    install(&state, tenant_id, up, route);

    let router = router_for(state, default_config(), tenant_id);
    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/api.example.com/v1/items")
        .body(Body::empty())
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(error_source(&response), Some("upstream".to_owned()));
    let bytes = body_bytes(response).await;
    assert_eq!(bytes, br#"{"ok":true}"#);
    mock.assert();
}
// @cpt-end:cpt-cf-oagw-dod-proxy-endpoint-registration:p1:inst-proxy-it-success-get-01

// @cpt-begin:cpt-cf-oagw-dod-proxy-endpoint-registration:p1:inst-proxy-it-success-post-01
#[tokio::test]
async fn a_successful_post_forwards_method_path_and_body() {
    let server = MockServer::start();
    let payload = "x".repeat(32);
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/v1/items")
            .body(payload.clone());
        then.status(201).body(r#"{"created":true}"#);
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let up = upstream_with("api.example.com", vec![mock_endpoint(&server)]);
    let route = simple_route(up.id, "/v1/items", &[RouteMethod::Get, RouteMethod::Post]);
    install(&state, tenant_id, up, route);

    let router = router_for(state, default_config(), tenant_id);
    let request = Request::builder()
        .method("POST")
        .uri("/oagw/v1/proxy/api.example.com/v1/items")
        .header("content-type", "application/json")
        .body(Body::from(payload))
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::CREATED);
    let bytes = body_bytes(response).await;
    assert_eq!(bytes, br#"{"created":true}"#);
    mock.assert();
}
// @cpt-end:cpt-cf-oagw-dod-proxy-endpoint-registration:p1:inst-proxy-it-success-post-01

// ---------------------------------------------------------------------------
// Upstream failure passthrough (`cpt-cf-oagw-flow-proxy-upstream-failure`).
// ---------------------------------------------------------------------------

// @cpt-begin:cpt-cf-oagw-dod-error-mapping:p1:inst-proxy-it-upstream-500-01
#[tokio::test]
async fn an_upstream_500_is_passed_through_unmodified() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/items");
        then.status(500).body(r#"{"err":"boom"}"#);
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let up = upstream_with("api.example.com", vec![mock_endpoint(&server)]);
    let route = simple_route(up.id, "/v1/items", &[RouteMethod::Get]);
    install(&state, tenant_id, up, route);

    let router = router_for(state, default_config(), tenant_id);
    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/api.example.com/v1/items")
        .body(Body::empty())
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(error_source(&response), Some("upstream".to_owned()));
    let bytes = body_bytes(response).await;
    assert_eq!(bytes, br#"{"err":"boom"}"#);
}
// @cpt-end:cpt-cf-oagw-dod-error-mapping:p1:inst-proxy-it-upstream-500-01

#[tokio::test]
async fn an_upstream_404_is_distinguished_from_a_gateway_route_not_found() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/items");
        then.status(404).body("not here");
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let up = upstream_with("api.example.com", vec![mock_endpoint(&server)]);
    let route = simple_route(up.id, "/v1/items", &[RouteMethod::Get]);
    install(&state, tenant_id, up, route);

    let router = router_for(state, default_config(), tenant_id);
    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/api.example.com/v1/items")
        .body(Body::empty())
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(error_source(&response), Some("upstream".to_owned()));
}

// ---------------------------------------------------------------------------
// Gateway errors (`cpt-cf-oagw-flow-proxy-guard-rejection`).
// ---------------------------------------------------------------------------

// @cpt-begin:cpt-cf-oagw-dod-error-mapping:p1:inst-proxy-it-unknown-alias-01
#[tokio::test]
async fn an_unknown_alias_returns_404_route_not_found_as_a_gateway_error() {
    let state = Arc::new(ControlPlaneState::new());
    let router = router_for(state, default_config(), Uuid::new_v4());

    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/does-not-exist/v1/items")
        .body(Body::empty())
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(error_source(&response), Some("gateway".to_owned()));
    assert_eq!(
        response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/problem+json")
    );
    let body = body_json(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
    );
    assert!(body["title"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(body["instance"].as_str().is_some_and(|s| !s.is_empty()));
}
// @cpt-end:cpt-cf-oagw-dod-error-mapping:p1:inst-proxy-it-unknown-alias-01

// @cpt-begin:cpt-cf-oagw-dod-proxy-endpoint-registration:p1:inst-proxy-it-wrong-method-01
#[tokio::test]
async fn a_method_absent_from_the_route_returns_404_and_the_upstream_receives_nothing() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.any_request();
        then.status(200);
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let up = upstream_with("api.example.com", vec![mock_endpoint(&server)]);
    let route = simple_route(up.id, "/v1/items", &[RouteMethod::Get, RouteMethod::Post]);
    install(&state, tenant_id, up, route);

    let router = router_for(state, default_config(), tenant_id);
    let request = Request::builder()
        .method("DELETE")
        .uri("/oagw/v1/proxy/api.example.com/v1/items")
        .body(Body::empty())
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = body_json(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
    );
    assert_eq!(mock.calls(), 0);
}
// @cpt-end:cpt-cf-oagw-dod-proxy-endpoint-registration:p1:inst-proxy-it-wrong-method-01

// @cpt-begin:cpt-cf-oagw-dod-disabled-upstream-rejection:p1:inst-proxy-it-disabled-upstream-01
#[tokio::test]
async fn a_disabled_upstream_returns_503_link_unavailable_and_opens_no_socket() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.any_request();
        then.status(200);
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let mut up = upstream_with("api.example.com", vec![mock_endpoint(&server)]);
    up.enabled = false;
    let route = simple_route(up.id, "/v1/items", &[RouteMethod::Get]);
    install(&state, tenant_id, up, route);

    let router = router_for(state, default_config(), tenant_id);
    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/api.example.com/v1/items")
        .body(Body::empty())
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(error_source(&response), Some("gateway".to_owned()));
    let body = body_json(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1"
    );
    assert_eq!(mock.calls(), 0);
}
// @cpt-end:cpt-cf-oagw-dod-disabled-upstream-rejection:p1:inst-proxy-it-disabled-upstream-01

// ---------------------------------------------------------------------------
// Guards.
// ---------------------------------------------------------------------------

// @cpt-begin:cpt-cf-oagw-dod-query-allowlist-guard:p1:inst-proxy-it-query-allowlist-01
#[tokio::test]
async fn a_disallowed_query_parameter_returns_400_and_the_upstream_receives_nothing() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.any_request();
        then.status(200);
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let up = upstream_with("api.example.com", vec![mock_endpoint(&server)]);
    let route = route_with(
        up.id,
        "/v1/items",
        &[RouteMethod::Get],
        vec!["limit".to_owned()],
        PathSuffixMode::Append,
    );
    install(&state, tenant_id, up, route);

    let router = router_for(state, default_config(), tenant_id);
    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/api.example.com/v1/items?debug=1")
        .body(Body::empty())
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_json(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
    assert_eq!(mock.calls(), 0);
}
// @cpt-end:cpt-cf-oagw-dod-query-allowlist-guard:p1:inst-proxy-it-query-allowlist-01

// @cpt-begin:cpt-cf-oagw-dod-path-suffix-guard:p1:inst-proxy-it-path-suffix-01
#[tokio::test]
async fn a_disabled_path_suffix_mode_rejects_an_extra_segment() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.any_request();
        then.status(200);
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let up = upstream_with("api.example.com", vec![mock_endpoint(&server)]);
    let route = route_with(
        up.id,
        "/v1/items",
        &[RouteMethod::Get],
        Vec::new(),
        PathSuffixMode::Disabled,
    );
    install(&state, tenant_id, up, route);

    let router = router_for(state, default_config(), tenant_id);
    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/api.example.com/v1/items/extra")
        .body(Body::empty())
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(mock.calls(), 0);
}
// @cpt-end:cpt-cf-oagw-dod-path-suffix-guard:p1:inst-proxy-it-path-suffix-01

// ---------------------------------------------------------------------------
// Body validation.
// ---------------------------------------------------------------------------

// @cpt-begin:cpt-cf-oagw-dod-body-validation:p1:inst-proxy-it-body-limit-01
#[tokio::test]
async fn a_declared_content_length_over_the_limit_is_rejected_before_buffering() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.any_request();
        then.status(200);
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let up = upstream_with("api.example.com", vec![mock_endpoint(&server)]);
    let route = simple_route(up.id, "/v1/items", &[RouteMethod::Post]);
    install(&state, tenant_id, up, route);

    let router = router_for(state, default_config(), tenant_id);
    let request = Request::builder()
        .method("POST")
        .uri("/oagw/v1/proxy/api.example.com/v1/items")
        .header("content-length", "104857601")
        .body(Body::from("tiny"))
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body = body_json(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1"
    );
    assert_eq!(mock.calls(), 0);
}
// @cpt-end:cpt-cf-oagw-dod-body-validation:p1:inst-proxy-it-body-limit-01

#[tokio::test]
async fn content_length_and_transfer_encoding_together_return_400() {
    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let server = MockServer::start();
    let up = upstream_with("api.example.com", vec![mock_endpoint(&server)]);
    let route = simple_route(up.id, "/v1/items", &[RouteMethod::Post]);
    install(&state, tenant_id, up, route);

    let router = router_for(state, default_config(), tenant_id);
    let request = Request::builder()
        .method("POST")
        .uri("/oagw/v1/proxy/api.example.com/v1/items")
        .header("content-length", "10")
        .header("transfer-encoding", "chunked")
        .body(Body::from("0123456789"))
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

/// The accept half of "A `POST` sending `Transfer-Encoding: gzip` returns
/// `400`, while `Transfer-Encoding: chunked` is accepted and forwarded":
/// a chunked request with no `Content-Length` reaches the upstream with the
/// exact same body bytes.
#[tokio::test]
async fn a_chunked_post_body_is_accepted_and_forwarded_with_matching_bytes() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/v1/items")
            .body("chunked-payload-bytes");
        then.status(201);
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let up = upstream_with("chunked.example.com", vec![mock_endpoint(&server)]);
    let route = simple_route(up.id, "/v1/items", &[RouteMethod::Post]);
    install(&state, tenant_id, up, route);

    let router = router_for(state, default_config(), tenant_id);
    let request = Request::builder()
        .method("POST")
        .uri("/oagw/v1/proxy/chunked.example.com/v1/items")
        .header("transfer-encoding", "chunked")
        .body(Body::from("chunked-payload-bytes"))
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::CREATED);
    mock.assert();
}

/// Integration-level counterpart of
/// `body::tests::a_streamed_body_crossing_the_limit_is_rejected_as_413`:
/// a chunked (no declared `Content-Length`) body whose actual byte count
/// crosses the 100MB limit is rejected before the upstream is ever called.
#[tokio::test]
async fn a_chunked_post_body_crossing_the_limit_returns_413_with_no_upstream_call() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.any_request();
        then.status(200);
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let up = upstream_with(
        "chunked-over-limit.example.com",
        vec![mock_endpoint(&server)],
    );
    let route = simple_route(up.id, "/v1/items", &[RouteMethod::Post]);
    install(&state, tenant_id, up, route);

    let router = router_for(state, default_config(), tenant_id);
    let oversized = vec![0_u8; oagw::domain::proxy::body::MAX_BODY_BYTES + 1];
    let request = Request::builder()
        .method("POST")
        .uri("/oagw/v1/proxy/chunked-over-limit.example.com/v1/items")
        .header("transfer-encoding", "chunked")
        .body(Body::from(oversized))
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body = body_json(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1"
    );
    assert_eq!(mock.calls(), 0);
}

#[tokio::test]
async fn an_unsupported_transfer_encoding_returns_400() {
    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let server = MockServer::start();
    let up = upstream_with("api.example.com", vec![mock_endpoint(&server)]);
    let route = simple_route(up.id, "/v1/items", &[RouteMethod::Post]);
    install(&state, tenant_id, up, route);

    let router = router_for(state, default_config(), tenant_id);
    let request = Request::builder()
        .method("POST")
        .uri("/oagw/v1/proxy/api.example.com/v1/items")
        .header("transfer-encoding", "gzip")
        .body(Body::from("data"))
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// Header transformation.
// ---------------------------------------------------------------------------

// @cpt-begin:cpt-cf-oagw-dod-hop-by-hop-stripping:p1:inst-proxy-it-hop-by-hop-01
// @cpt-begin:cpt-cf-oagw-dod-header-plan-application:p1:inst-proxy-it-host-rewrite-01
#[tokio::test]
async fn hop_by_hop_headers_are_stripped_and_host_is_rewritten() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/v1/items")
            .header("host", server.host())
            .header_missing("connection")
            .header_missing("te")
            .header_missing("trailer")
            .header_missing("upgrade")
            .header_missing("keep-alive")
            .header_missing("proxy-authenticate")
            .header_missing("proxy-authorization")
            .header_missing("transfer-encoding");
        then.status(200);
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let up = upstream_with("api.example.com", vec![mock_endpoint(&server)]);
    let route = simple_route(up.id, "/v1/items", &[RouteMethod::Get]);
    install(&state, tenant_id, up, route);

    let router = router_for(state, default_config(), tenant_id);
    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/api.example.com/v1/items")
        .header("host", "gateway.internal")
        .header("connection", "keep-alive")
        .header("te", "trailers")
        .header("trailer", "X-T")
        .header("upgrade", "h2c")
        .header("keep-alive", "timeout=5")
        .header("proxy-authenticate", "Basic")
        .header("proxy-authorization", "Basic abc")
        .header("transfer-encoding", "chunked")
        .body(Body::empty())
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::OK);
    mock.assert();
}
// @cpt-end:cpt-cf-oagw-dod-header-plan-application:p1:inst-proxy-it-host-rewrite-01
// @cpt-end:cpt-cf-oagw-dod-hop-by-hop-stripping:p1:inst-proxy-it-hop-by-hop-01

// @cpt-begin:cpt-cf-oagw-dod-header-plan-application:p1:inst-proxy-it-request-plan-01
#[tokio::test]
async fn a_request_header_plan_applies_remove_set_and_add() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/v1/items")
            .header_missing("x-drop")
            .header("x-set", "a")
            .header("x-add", "b");
        then.status(200);
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let mut up = upstream_with("api.example.com", vec![mock_endpoint(&server)]);
    up.headers = Some(HeadersConfig {
        request: Some(RequestHeaders {
            set: Some(std::collections::BTreeMap::from([(
                "X-Set".to_owned(),
                "a".to_owned(),
            )])),
            add: Some(std::collections::BTreeMap::from([(
                "X-Add".to_owned(),
                "b".to_owned(),
            )])),
            remove: Some(vec!["X-Drop".to_owned()]),
            passthrough: oagw::domain::model::Passthrough::All,
            passthrough_allowlist: None,
        }),
        response: None,
    });
    let route = simple_route(up.id, "/v1/items", &[RouteMethod::Get]);
    install(&state, tenant_id, up, route);

    let router = router_for(state, default_config(), tenant_id);
    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/api.example.com/v1/items")
        .header("x-drop", "1")
        .body(Body::empty())
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::OK);
    mock.assert();
}
// @cpt-end:cpt-cf-oagw-dod-header-plan-application:p1:inst-proxy-it-request-plan-01

// @cpt-begin:cpt-cf-oagw-dod-header-plan-application:p1:inst-proxy-it-response-plan-01
#[tokio::test]
async fn a_response_header_plan_applies_remove_and_set_while_the_body_stays_unchanged() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/items");
        then.status(200).header("server", "nginx").body("unchanged");
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let mut up = upstream_with("api.example.com", vec![mock_endpoint(&server)]);
    up.headers = Some(HeadersConfig {
        request: None,
        response: Some(ResponseHeaders {
            set: Some(std::collections::BTreeMap::from([(
                "X-Gw".to_owned(),
                "1".to_owned(),
            )])),
            add: None,
            remove: Some(vec!["Server".to_owned()]),
        }),
    });
    let route = simple_route(up.id, "/v1/items", &[RouteMethod::Get]);
    install(&state, tenant_id, up, route);

    let router = router_for(state, default_config(), tenant_id);
    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/api.example.com/v1/items")
        .body(Body::empty())
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response.headers().contains_key("server"));
    assert_eq!(
        response.headers().get("x-gw").and_then(|v| v.to_str().ok()),
        Some("1")
    );
    let bytes = body_bytes(response).await;
    assert_eq!(bytes, b"unchanged");
}
// @cpt-end:cpt-cf-oagw-dod-header-plan-application:p1:inst-proxy-it-response-plan-01

// CODE2-F-002 regression: hop-by-hop headers on the upstream RESPONSE must
// never reach the client either, symmetric with the request direction.
#[tokio::test]
async fn hop_by_hop_response_headers_never_reach_the_client() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/items");
        then.status(200)
            .header("transfer-encoding", "chunked")
            .header("connection", "keep-alive")
            .body("body");
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let up = upstream_with("hopresp.example.com", vec![mock_endpoint(&server)]);
    let route = simple_route(up.id, "/v1/items", &[RouteMethod::Get]);
    install(&state, tenant_id, up, route);

    let router = router_for(state, default_config(), tenant_id);
    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/hopresp.example.com/v1/items")
        .body(Body::empty())
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response.headers().contains_key("transfer-encoding"));
    assert!(!response.headers().contains_key("connection"));
    let bytes = body_bytes(response).await;
    assert_eq!(bytes, b"body");
}

// ---------------------------------------------------------------------------
// Target-host selection (`cpt-cf-oagw-adr-request-routing`).
// ---------------------------------------------------------------------------

// @cpt-begin:cpt-cf-oagw-dod-target-host-selection:p1:inst-proxy-it-target-host-01
#[tokio::test]
async fn target_host_header_selects_one_endpoint_and_is_stripped_from_the_upstream_request() {
    let server_a = MockServer::start();
    let server_b = MockServer::start();
    let mock_a = server_a.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/v1/status")
            .header_missing("x-oagw-target-host");
        then.status(200).body("a");
    });
    let mock_b = server_b.mock(|when, then| {
        when.any_request();
        then.status(200).body("b");
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let up = upstream_with(
        "my-service",
        vec![mock_endpoint(&server_a), mock_endpoint(&server_b)],
    );
    let route = simple_route(up.id, "/v1/status", &[RouteMethod::Get]);
    install(&state, tenant_id, up, route);

    let router = router_for(state, default_config(), tenant_id);
    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/my-service/v1/status")
        .header("x-oagw-target-host", server_a.host())
        .body(Body::empty())
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = body_bytes(response).await;
    assert_eq!(bytes, b"a");
    mock_a.assert();
    assert_eq!(mock_b.calls(), 0);
}
// @cpt-end:cpt-cf-oagw-dod-target-host-selection:p1:inst-proxy-it-target-host-01

#[tokio::test]
async fn a_common_suffix_alias_with_no_target_host_header_returns_400_missing() {
    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let up = upstream_with(
        "vendor.com",
        vec![
            http_endpoint("us.vendor.com", 443),
            http_endpoint("eu.vendor.com", 443),
        ],
    );
    let route = simple_route(up.id, "/v1/api/resource", &[RouteMethod::Post]);
    install(&state, tenant_id, up, route);

    let router = router_for(state, default_config(), tenant_id);
    let request = Request::builder()
        .method("POST")
        .uri("/oagw/v1/proxy/vendor.com/v1/api/resource")
        .body(Body::empty())
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_json(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1"
    );
}

#[tokio::test]
async fn a_target_host_header_carrying_a_port_returns_400_invalid() {
    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let up = upstream_with(
        "vendor.com",
        vec![
            http_endpoint("us.vendor.com", 443),
            http_endpoint("eu.vendor.com", 443),
        ],
    );
    let route = simple_route(up.id, "/v1/api/resource", &[RouteMethod::Post]);
    install(&state, tenant_id, up, route);

    let router = router_for(state, default_config(), tenant_id);
    let request = Request::builder()
        .method("POST")
        .uri("/oagw/v1/proxy/vendor.com/v1/api/resource")
        .header("x-oagw-target-host", "us.vendor.com:8443")
        .body(Body::empty())
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_json(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1"
    );
}

#[tokio::test]
async fn an_unmatched_target_host_header_returns_400_unknown() {
    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let up = upstream_with(
        "vendor.com",
        vec![
            http_endpoint("us.vendor.com", 443),
            http_endpoint("eu.vendor.com", 443),
        ],
    );
    let route = simple_route(up.id, "/v1/api/resource", &[RouteMethod::Post]);
    install(&state, tenant_id, up, route);

    let router = router_for(state, default_config(), tenant_id);
    let request = Request::builder()
        .method("POST")
        .uri("/oagw/v1/proxy/vendor.com/v1/api/resource")
        .header("x-oagw-target-host", "apac.vendor.com")
        .body(Body::empty())
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_json(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1"
    );
}

// @cpt-begin:cpt-cf-oagw-dod-target-host-selection:p1:inst-proxy-it-round-robin-01
#[tokio::test]
async fn ten_requests_to_a_two_endpoint_explicit_pool_split_five_and_five() {
    let server_a = MockServer::start();
    let server_b = MockServer::start();
    let mock_a = server_a.mock(|when, then| {
        when.any_request();
        then.status(200);
    });
    let mock_b = server_b.mock(|when, then| {
        when.any_request();
        then.status(200);
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let up = upstream_with(
        "my-pool",
        vec![mock_endpoint(&server_a), mock_endpoint(&server_b)],
    );
    let route = simple_route(up.id, "/v1/status", &[RouteMethod::Get]);
    install(&state, tenant_id, up, route);

    for _ in 0..10 {
        let router = router_for(Arc::clone(&state), default_config(), tenant_id);
        let request = Request::builder()
            .method("GET")
            .uri("/oagw/v1/proxy/my-pool/v1/status")
            .body(Body::empty())
            .unwrap();
        let response = send(router, request).await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    assert_eq!(mock_a.calls(), 5);
    assert_eq!(mock_b.calls(), 5);
}
// @cpt-end:cpt-cf-oagw-dod-target-host-selection:p1:inst-proxy-it-round-robin-01

// ---------------------------------------------------------------------------
// Timeouts and transport failures (`cpt-cf-oagw-flow-proxy-upstream-failure`).
// ---------------------------------------------------------------------------

// @cpt-begin:cpt-cf-oagw-dod-proxy-timeout:p1:inst-proxy-it-request-timeout-01
#[tokio::test]
async fn an_upstream_that_never_answers_in_time_returns_504_request_timeout() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/slow");
        then.status(200).delay(Duration::from_secs(5));
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let up = upstream_with("api.example.com", vec![mock_endpoint(&server)]);
    let route = simple_route(up.id, "/v1/slow", &[RouteMethod::Get]);
    install(&state, tenant_id, up, route);

    let mut config = default_config();
    config.proxy_timeout_secs = 1;
    let router = router_for(state, config, tenant_id);
    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/api.example.com/v1/slow")
        .body(Body::empty())
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(error_source(&response), Some("gateway".to_owned()));
    let body = body_json(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1"
    );
}
// @cpt-end:cpt-cf-oagw-dod-proxy-timeout:p1:inst-proxy-it-request-timeout-01

/// "the client request is never re-issued": this crate has no retry
/// mechanism at all (no `tokio-retry` usage anywhere in the proxy path), so
/// the only externally observable symptom a hidden retry-with-backoff would
/// produce is added latency; the elapsed-time ceiling below is the closest
/// assertable proxy for "never re-issued" available from outside the crate
/// (a single immediate connection refusal completes in low
/// single-digit milliseconds; any retry loop would multiply that).
#[tokio::test]
async fn a_refused_connection_returns_502_downstream_error() {
    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    // Port 1 is privileged and virtually never bound in test environments,
    // so a connection attempt is refused immediately.
    let up = upstream_with("closed.example.com", vec![http_endpoint("127.0.0.1", 1)]);
    let route = simple_route(up.id, "/v1/items", &[RouteMethod::Get]);
    install(&state, tenant_id, up, route);

    let router = router_for(state, default_config(), tenant_id);
    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/closed.example.com/v1/items")
        .body(Body::empty())
        .unwrap();
    let started = std::time::Instant::now();
    let response = send(router, request).await;
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "a single refused connection must not be retried with added latency"
    );

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = body_json(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1"
    );
}

// ---------------------------------------------------------------------------
// Plaintext-connection policy (`cpt-cf-oagw-dod-plaintext-connection-policy`).
// ---------------------------------------------------------------------------

// @cpt-begin:cpt-cf-oagw-dod-plaintext-connection-policy:p1:inst-proxy-it-http-disabled-01
#[tokio::test]
async fn with_allow_http_upstream_disabled_an_http_endpoint_returns_503() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.any_request();
        then.status(200);
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let up = upstream_with("api.example.com", vec![mock_endpoint(&server)]);
    let route = simple_route(up.id, "/v1/items", &[RouteMethod::Get]);
    install(&state, tenant_id, up, route);

    let mut config = default_config();
    config.allow_http_upstream = false;
    let router = router_for(state, config, tenant_id);
    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/api.example.com/v1/items")
        .body(Body::empty())
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = body_json(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1"
    );
    assert_eq!(mock.calls(), 0);
}
// @cpt-end:cpt-cf-oagw-dod-plaintext-connection-policy:p1:inst-proxy-it-http-disabled-01

// @cpt-begin:cpt-cf-oagw-dod-plaintext-connection-policy:p1:inst-proxy-it-http-enabled-01
/// The mirror image of `with_allow_http_upstream_disabled_an_http_endpoint_returns_503`:
/// with the flag enabled, the same `http` endpoint is actually dialed and
/// its response returned unchanged, rather than merely "not rejected".
#[tokio::test]
async fn with_allow_http_upstream_enabled_the_http_endpoint_is_forwarded_unchanged() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/items");
        then.status(200).body("plaintext-ok");
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let up = upstream_with("plain.example.com", vec![mock_endpoint(&server)]);
    let route = simple_route(up.id, "/v1/items", &[RouteMethod::Get]);
    install(&state, tenant_id, up, route);

    let mut config = default_config();
    config.allow_http_upstream = true;
    let router = router_for(state, config, tenant_id);
    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/plain.example.com/v1/items")
        .body(Body::empty())
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(error_source(&response), Some("upstream".to_owned()));
    assert_eq!(body_bytes(response).await, b"plaintext-ok");
    mock.assert();
}
// @cpt-end:cpt-cf-oagw-dod-plaintext-connection-policy:p1:inst-proxy-it-http-enabled-01

// @cpt-begin:cpt-cf-oagw-dod-plaintext-connection-policy:p1:inst-proxy-it-ws-disabled-01
#[tokio::test]
async fn with_allow_http_upstream_disabled_a_ws_endpoint_returns_the_same_503() {
    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let up = upstream_with(
        "ws.example.com",
        vec![Endpoint {
            scheme: Scheme::Ws,
            host: "ws.example.com".to_owned(),
            port: Some(80),
        }],
    );
    let route = simple_route(up.id, "/v1/stream", &[RouteMethod::Get]);
    install(&state, tenant_id, up, route);

    let mut config = default_config();
    config.allow_http_upstream = false;
    let router = router_for(state, config, tenant_id);
    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/ws.example.com/v1/stream")
        .body(Body::empty())
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = body_json(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1"
    );
}
// @cpt-end:cpt-cf-oagw-dod-plaintext-connection-policy:p1:inst-proxy-it-ws-disabled-01

// ---------------------------------------------------------------------------
// CORS (`cpt-cf-oagw-adr-cors`).
// ---------------------------------------------------------------------------

// @cpt-begin:cpt-cf-oagw-dod-cors-preflight:p1:inst-proxy-it-preflight-01
#[tokio::test]
async fn a_cors_preflight_returns_204_with_the_documented_headers() {
    let state = Arc::new(ControlPlaneState::new());
    let router = router_for(state, default_config(), Uuid::new_v4());

    let request = Request::builder()
        .method("OPTIONS")
        .uri("/oagw/v1/proxy/api.example.com/v1/items")
        .header("origin", "https://app.example.com")
        .header("access-control-request-method", "POST")
        .header(
            "access-control-request-headers",
            "Content-Type, Authorization",
        )
        .body(Body::empty())
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let headers = response.headers();
    assert_eq!(
        headers
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("https://app.example.com")
    );
    assert_eq!(
        headers
            .get("access-control-allow-methods")
            .and_then(|v| v.to_str().ok()),
        Some("POST")
    );
    assert_eq!(
        headers
            .get("access-control-allow-headers")
            .and_then(|v| v.to_str().ok()),
        Some("Content-Type, Authorization")
    );
    assert_eq!(
        headers
            .get("access-control-max-age")
            .and_then(|v| v.to_str().ok()),
        Some("86400")
    );
    let bytes = body_bytes(response).await;
    assert!(bytes.is_empty());
}
// @cpt-end:cpt-cf-oagw-dod-cors-preflight:p1:inst-proxy-it-preflight-01

#[tokio::test]
async fn a_preflight_succeeds_even_for_an_alias_no_upstream_defines() {
    let state = Arc::new(ControlPlaneState::new());
    let router = router_for(state, default_config(), Uuid::new_v4());

    let request = Request::builder()
        .method("OPTIONS")
        .uri("/oagw/v1/proxy/does-not-exist/v1/items")
        .header("origin", "https://app.example.com")
        .header("access-control-request-method", "GET")
        .body(Body::empty())
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

// @cpt-begin:cpt-cf-oagw-dod-cors-request-enforcement:p1:inst-proxy-it-cors-origin-01
#[tokio::test]
async fn a_disallowed_cors_origin_returns_403_and_the_upstream_receives_nothing() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.any_request();
        then.status(200);
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let mut up = upstream_with("api.example.com", vec![mock_endpoint(&server)]);
    up.cors = Some(CorsConfig {
        sharing: Sharing::Private,
        enabled: true,
        allowed_origins: vec!["https://app.example.com".to_owned()],
        allowed_methods: vec![HttpMethod::Get, HttpMethod::Post],
        expose_headers: Vec::new(),
        allow_credentials: false,
    });
    let route = simple_route(up.id, "/v1/items", &[RouteMethod::Post]);
    install(&state, tenant_id, up, route);

    let router = router_for(state, default_config(), tenant_id);
    let request = Request::builder()
        .method("POST")
        .uri("/oagw/v1/proxy/api.example.com/v1/items")
        .header("origin", "https://evil.com")
        .body(Body::empty())
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = body_json(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1"
    );
    assert_eq!(mock.calls(), 0);
}
// @cpt-end:cpt-cf-oagw-dod-cors-request-enforcement:p1:inst-proxy-it-cors-origin-01

#[tokio::test]
async fn a_disallowed_cors_method_returns_403() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.any_request();
        then.status(200);
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let mut up = upstream_with("api.example.com", vec![mock_endpoint(&server)]);
    up.cors = Some(CorsConfig {
        sharing: Sharing::Private,
        enabled: true,
        allowed_origins: vec!["https://app.example.com".to_owned()],
        allowed_methods: vec![HttpMethod::Get, HttpMethod::Post],
        expose_headers: Vec::new(),
        allow_credentials: false,
    });
    let route = simple_route(
        up.id,
        "/v1/items",
        &[RouteMethod::Get, RouteMethod::Post, RouteMethod::Delete],
    );
    install(&state, tenant_id, up, route);

    let router = router_for(state, default_config(), tenant_id);
    let request = Request::builder()
        .method("DELETE")
        .uri("/oagw/v1/proxy/api.example.com/v1/items")
        .header("origin", "https://app.example.com")
        .body(Body::empty())
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = body_json(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1"
    );
    assert_eq!(mock.calls(), 0);
}

#[tokio::test]
async fn a_permitted_cross_origin_get_carries_the_documented_cors_response_headers() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/items");
        then.status(200);
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let mut up = upstream_with("api.example.com", vec![mock_endpoint(&server)]);
    up.cors = Some(CorsConfig {
        sharing: Sharing::Private,
        enabled: true,
        allowed_origins: vec!["https://app.example.com".to_owned()],
        allowed_methods: vec![HttpMethod::Get],
        expose_headers: vec!["X-Request-ID".to_owned()],
        allow_credentials: false,
    });
    let route = simple_route(up.id, "/v1/items", &[RouteMethod::Get]);
    install(&state, tenant_id, up, route);

    let router = router_for(state, default_config(), tenant_id);
    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/api.example.com/v1/items")
        .header("origin", "https://app.example.com")
        .body(Body::empty())
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers();
    assert_eq!(
        headers
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("https://app.example.com")
    );
    assert_eq!(
        headers.get("vary").and_then(|v| v.to_str().ok()),
        Some("Origin")
    );
    assert_eq!(
        headers
            .get("access-control-expose-headers")
            .and_then(|v| v.to_str().ok()),
        Some("X-Request-ID")
    );
}

#[tokio::test]
async fn cors_disabled_upstream_forwards_without_origin_enforcement_or_cors_headers() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/items");
        then.status(200);
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let up = upstream_with("api.example.com", vec![mock_endpoint(&server)]);
    let route = simple_route(up.id, "/v1/items", &[RouteMethod::Get]);
    install(&state, tenant_id, up, route);

    let router = router_for(state, default_config(), tenant_id);
    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/api.example.com/v1/items")
        .header("origin", "https://anyone.example.com")
        .body(Body::empty())
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        !response
            .headers()
            .contains_key("access-control-allow-origin")
    );
}

// ---------------------------------------------------------------------------
// Router registration itself.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn both_proxy_path_forms_route_to_the_handler() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::GET);
        then.status(200).body("bare");
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let up = upstream_with("bare.example.com", vec![mock_endpoint(&server)]);
    let route = simple_route(up.id, "", &[RouteMethod::Get]);
    install(&state, tenant_id, up, route);

    let router = router_for(Arc::clone(&state), default_config(), tenant_id);
    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/bare.example.com")
        .body(Body::empty())
        .unwrap();
    let response = send(router, request).await;
    assert_eq!(response.status(), StatusCode::OK);
}
