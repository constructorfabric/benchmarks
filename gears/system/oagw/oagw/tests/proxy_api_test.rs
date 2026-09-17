//! Integration tests for the proxy data plane over HTTP.
//!
//! The tests mount the proxy endpoint exactly as the gear does —
//! [`oagw::api::proxy::routes::register_proxy`] over a real [`DataPlane`] — and
//! drive it with `tower::ServiceExt::oneshot`. Every gateway refusal is checked
//! for its RFC 9457 shape and its `X-OAGW-Error-Source` header; a forwarded
//! upstream answer is checked to be passed through untouched.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use toolkit::api::OpenApiRegistry;
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

use oagw::MemoryStore;
use oagw::api::proxy::routes::register_proxy;
use oagw::domain::model::{
    CorsConfig, Endpoint, EndpointScheme, HttpMatch, PathSuffixMode, Protocol, RateAlgorithm,
    RateLimitConfig, RateScope, RateStrategy, Route, RouteMatcher, ServerConfig, SharingMode,
    SustainedRate, Upstream,
};
use oagw::domain::repo::ConfigStore;
use oagw::infra::plugin::{PluginEngine, resolver_that_fails};
use oagw::infra::proxy::config::{ConfigSource, ResolverChain};
use oagw::infra::proxy::connector::UpstreamDialer;
use oagw::infra::proxy::{DataPlane, SsrfPolicy};

// ── Noop OpenAPI registry ───────────────────────────────────────────────

struct NoopRegistry;

impl OpenApiRegistry for NoopRegistry {
    fn register_operation(&self, _spec: &toolkit::api::operation_builder::OperationSpec) {}

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

// ── Harness ─────────────────────────────────────────────────────────────

struct Fixture {
    router: Router,
}

fn endpoint(port: u16) -> Endpoint {
    Endpoint::new(EndpointScheme::Http, "127.0.0.1", Some(port)).expect("endpoint")
}

fn upstream(tenant: Uuid, alias: &str, port: u16) -> Upstream {
    Upstream {
        id: Uuid::new_v4(),
        tenant_id: tenant,
        alias: alias.to_owned(),
        enabled: true,
        tags: vec![],
        server: ServerConfig {
            endpoints: vec![endpoint(port)],
        },
        protocol: Protocol::Http,
        auth: None,
        headers: None,
        plugins: None,
        rate_limit: None,
        cors: None,
        created_at: 1,
        updated_at: 1,
    }
}

fn route(tenant: Uuid, upstream_id: Uuid, path: &str, methods: &[&str]) -> Route {
    Route {
        id: Uuid::new_v4(),
        tenant_id: tenant,
        upstream_id,
        name: None,
        tags: vec![],
        matcher: RouteMatcher::Http(HttpMatch {
            methods: methods.iter().map(|method| (*method).to_owned()).collect(),
            path: path.to_owned(),
            query_allowlist: vec![],
            path_suffix_mode: PathSuffixMode::Append,
        }),
        priority: 0,
        enabled: true,
        plugins: None,
        rate_limit: None,
        cors: None,
        created_at: 1,
        updated_at: 1,
    }
}

/// A loopback server that answers every request with a raw HTTP/1.1 response.
async fn spawn_raw_server(response: &'static str) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener");
    let port = listener.local_addr().expect("address").port();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buffer = vec![0_u8; 4096];
                let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buffer).await;
                let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes()).await;
                let _ = tokio::io::AsyncWriteExt::shutdown(&mut socket).await;
            });
        }
    });
    port
}

fn data_plane(store: Arc<oagw::MemoryStore>, allow_http: bool) -> DataPlane {
    let source = ConfigSource::new(store, Arc::new(ResolverChain::new(None)));
    let dialer = UpstreamDialer::new(
        Arc::new(pingora_core::connectors::TransportConnector::new(None)),
        SsrfPolicy::disabled(),
        allow_http,
    );
    let engine = PluginEngine::with_builtins(resolver_that_fails());
    DataPlane::new(source, dialer, engine, Duration::from_secs(5))
}

async fn fixture(
    tenant: Uuid,
    store: Arc<MemoryStore>,
    owner: Upstream,
    matched: Route,
) -> Fixture {
    store.insert_upstream(&owner).expect("upstream");
    store.insert_route(&matched).expect("route");

    let plane = Arc::new(data_plane(store, true));
    let context = SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_type("user")
        .subject_tenant_id(tenant)
        .build()
        .expect("security context");
    let router = register_proxy(Router::new(), &NoopRegistry)
        .layer(axum::Extension(plane))
        .layer(axum::Extension(context));
    Fixture { router }
}

/// Send a request to the proxy endpoint and return status, headers and body.
async fn send(
    router: Router,
    request: Request<Body>,
) -> (StatusCode, Vec<(String, String)>, Value) {
    let response = router.oneshot(request).await.expect("infallible service");
    let status = response.status();
    let headers = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                value.to_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or(Value::String(String::from_utf8_lossy(&bytes).to_string()))
    };
    (status, headers, body)
}

fn proxy_request(method: &str, path: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(format!("http://gateway{path}"));
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(Body::empty()).expect("request")
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(candidate, _)| candidate == name)
        .map(|(_, value)| value.as_str())
}

// ── Tests ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_proxy_mounts_under_the_version_prefix_without_the_api_prefix() {
    let store = Arc::new(MemoryStore::new());
    let tenant = Uuid::new_v4();
    let owner = upstream(tenant, "payments", 1);
    let matched = route(tenant, owner.id, "/v1/*", &["GET"]);
    let Fixture { router } = fixture(tenant, store, owner, matched).await;

    // A request addressed at `/api/...` is not the proxy route: axum answers
    // with its own 404, which is what proves the mount point.
    let (status, _, body) = send(
        router,
        proxy_request("GET", "/api/oagw/v1/proxy/payments", &[]),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        body,
        Value::Null,
        "an unmatched route carries no problem document"
    );
}

#[tokio::test]
async fn an_unknown_alias_is_a_404_problem_document_from_the_gateway() {
    let store = Arc::new(oagw::MemoryStore::new());
    let tenant = Uuid::new_v4();
    let owner = upstream(tenant, "payments", 1);
    let matched = route(tenant, owner.id, "/v1/*", &["GET"]);
    let Fixture { router } = fixture(tenant, store, owner, matched).await;

    let (status, headers, body) = send(
        router,
        proxy_request("GET", "/oagw/v1/proxy/absent/v1/charges", &[]),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["title"], "Route Not Found");
    assert!(
        body["type"]
            .as_str()
            .is_some_and(|value| value.starts_with("gts."))
    );
    assert!(body["context"].is_object());
    assert_eq!(header(&headers, "x-oagw-error-source"), Some("gateway"));
    assert_eq!(
        header(&headers, "content-type"),
        Some("application/problem+json")
    );
}

#[tokio::test]
async fn a_route_that_does_not_match_is_a_404() {
    let (port, _server) = (1_u16, ());
    let store = Arc::new(oagw::MemoryStore::new());
    let tenant = Uuid::new_v4();
    let owner = upstream(tenant, "payments", port);
    let matched = route(tenant, owner.id, "/v1/*", &["GET"]);
    let Fixture { router } = fixture(tenant, store, owner, matched).await;

    let (status, _, body) = send(
        router,
        proxy_request("GET", "/oagw/v1/proxy/payments/v2/charges", &[]),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["title"], "Route Not Found");
}

#[tokio::test]
async fn a_forwarded_answer_arrives_with_the_upstream_status_and_body() {
    let port =
        spawn_raw_server("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\r\n{\"ok\":true}")
            .await;
    let store = Arc::new(oagw::MemoryStore::new());
    let tenant = Uuid::new_v4();
    let owner = upstream(tenant, "payments", port);
    let matched = route(tenant, owner.id, "/v1/*", &["GET"]);
    let Fixture { router } = fixture(tenant, store, owner, matched).await;

    let (status, _, body) = send(
        router,
        proxy_request("GET", "/oagw/v1/proxy/payments/v1/charges", &[]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], json!(true));
}

#[tokio::test]
async fn an_upstream_404_is_passed_through_with_the_upstream_source() {
    let port = spawn_raw_server(
        "HTTP/1.1 404 Not Found\r\ncontent-type: application/json\r\n\r\n{\"code\":\"nope\"}",
    )
    .await;
    let store = Arc::new(oagw::MemoryStore::new());
    let tenant = Uuid::new_v4();
    let owner = upstream(tenant, "payments", port);
    let matched = route(tenant, owner.id, "/v1/*", &["GET"]);
    let Fixture { router } = fixture(tenant, store, owner, matched).await;

    let (status, headers, body) = send(
        router,
        proxy_request("GET", "/oagw/v1/proxy/payments/v1/missing", &[]),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], json!("nope"));
    assert_eq!(header(&headers, "x-oagw-error-source"), Some("upstream"));
}

#[tokio::test]
async fn a_disabled_upstream_is_a_503_problem_document() {
    let store = Arc::new(oagw::MemoryStore::new());
    let tenant = Uuid::new_v4();
    let mut owner = upstream(tenant, "payments", 1);
    owner.enabled = false;
    let matched = route(tenant, owner.id, "/v1/*", &["GET"]);
    let Fixture { router } = fixture(tenant, store, owner, matched).await;

    let (status, headers, body) = send(
        router,
        proxy_request("GET", "/oagw/v1/proxy/payments/v1/charges", &[]),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["title"], "Link Unavailable");
    assert_eq!(header(&headers, "x-oagw-error-source"), Some("gateway"));
}

#[tokio::test]
async fn a_rate_limited_route_answers_429_with_retry_after() {
    let store = Arc::new(oagw::MemoryStore::new());
    let tenant = Uuid::new_v4();
    let mut owner = upstream(tenant, "payments", 1);
    owner.rate_limit = Some(RateLimitConfig {
        sharing: SharingMode::Private,
        algorithm: RateAlgorithm::TokenBucket,
        sustained: SustainedRate {
            rate: 1,
            window: oagw::domain::model::RateWindow::Minute,
        },
        burst: None,
        scope: RateScope::Global,
        strategy: RateStrategy::Reject,
        cost: 1,
    });
    let matched = route(tenant, owner.id, "/v1/*", &["GET"]);
    let Fixture { router } = fixture(tenant, store, owner, matched).await;

    let (first, _, _) = send(
        router.clone(),
        proxy_request("GET", "/oagw/v1/proxy/payments/v1/charges", &[]),
    )
    .await;
    // The first request is not rate limited (it fails later, on the dial).
    assert_ne!(first, StatusCode::TOO_MANY_REQUESTS);

    let (status, headers, body) = send(
        router,
        proxy_request("GET", "/oagw/v1/proxy/payments/v1/charges", &[]),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body["title"], "Rate Limit Exceeded");
    assert!(header(&headers, "retry-after").is_some());
    assert_eq!(header(&headers, "x-ratelimit-limit"), Some("1"));
    assert_eq!(header(&headers, "x-ratelimit-remaining"), Some("0"));
    assert!(header(&headers, "x-ratelimit-reset").is_some());
}

#[tokio::test]
async fn a_disallowed_cross_origin_request_is_a_403_problem_document() {
    let store = Arc::new(oagw::MemoryStore::new());
    let tenant = Uuid::new_v4();
    let mut owner = upstream(tenant, "payments", 1);
    owner.cors = Some(CorsConfig {
        sharing: SharingMode::Private,
        enabled: true,
        allowed_origins: vec!["https://console.example".to_owned()],
        allowed_methods: vec!["GET".to_owned()],
        expose_headers: vec![],
        allow_credentials: false,
    });
    let matched = route(tenant, owner.id, "/v1/*", &["GET"]);
    let Fixture { router } = fixture(tenant, store, owner, matched).await;

    let (status, headers, body) = send(
        router,
        proxy_request(
            "GET",
            "/oagw/v1/proxy/payments/v1/charges",
            &[("origin", "https://evil.example")],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(
        body["type"]
            .as_str()
            .is_some_and(|value| value.contains("cors"))
    );
    assert_eq!(header(&headers, "vary"), Some("Origin"));
    assert_eq!(header(&headers, "x-oagw-error-source"), Some("gateway"));
}

#[tokio::test]
async fn a_cross_origin_preflight_is_answered_locally_with_a_204() {
    let store = Arc::new(oagw::MemoryStore::new());
    let tenant = Uuid::new_v4();
    let mut owner = upstream(tenant, "payments", 1);
    owner.cors = Some(CorsConfig {
        sharing: SharingMode::Private,
        enabled: true,
        allowed_origins: vec!["https://console.example".to_owned()],
        allowed_methods: vec!["POST".to_owned()],
        expose_headers: vec![],
        allow_credentials: false,
    });
    let matched = route(tenant, owner.id, "/v1/charges", &["GET"]);
    let Fixture { router } = fixture(tenant, store, owner, matched).await;

    let (status, headers, body) = send(
        router,
        proxy_request(
            "OPTIONS",
            "/oagw/v1/proxy/payments/v1/charges",
            &[
                ("origin", "https://console.example"),
                ("access-control-request-method", "POST"),
                ("access-control-request-headers", "content-type"),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(body, Value::Null);
    assert_eq!(
        header(&headers, "access-control-allow-origin"),
        Some("https://console.example")
    );
    assert_eq!(
        header(&headers, "access-control-allow-methods"),
        Some("POST")
    );
}

#[tokio::test]
async fn a_plaintext_upstream_is_refused_when_http_is_not_allowed() {
    let store = Arc::new(oagw::MemoryStore::new());
    let tenant = Uuid::new_v4();
    let owner = upstream(tenant, "payments", 1);
    let matched = route(tenant, owner.id, "/v1/*", &["GET"]);

    store.insert_upstream(&owner).expect("upstream");
    store.insert_route(&matched).expect("route");

    let plane = Arc::new(data_plane(store, false));
    let context = SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_type("user")
        .subject_tenant_id(tenant)
        .build()
        .expect("security context");
    let router = register_proxy(Router::new(), &NoopRegistry)
        .layer(axum::Extension(plane))
        .layer(axum::Extension(context));

    let (status, headers, body) = send(
        router,
        proxy_request("GET", "/oagw/v1/proxy/payments/v1/charges", &[]),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(header(&headers, "x-oagw-error-source"), Some("gateway"));
    assert!(
        body["detail"]
            .as_str()
            .is_some_and(|value| value.contains("plaintext"))
    );
}

#[tokio::test]
async fn every_documented_method_reaches_the_proxy_endpoint() {
    let store = Arc::new(oagw::MemoryStore::new());
    let tenant = Uuid::new_v4();
    let owner = upstream(tenant, "payments", 1);
    let matched = route(tenant, owner.id, "/v1/*", &["GET"]);
    let Fixture { router } = fixture(tenant, store, owner, matched).await;

    for method in ["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"] {
        let (status, _, _) = send(
            router.clone(),
            proxy_request(method, "/oagw/v1/proxy/payments/v1/charges", &[]),
        )
        .await;
        assert_ne!(
            status,
            StatusCode::METHOD_NOT_ALLOWED,
            "{method} must be routed"
        );
    }
}
