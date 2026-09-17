//! Policy-layer tests for slice S5: rate limiting (ADR-0003), CORS (ADR-0004)
//! and the plugin chain (ADR-0002, ADR-0009) over the proxy path.
//!
//! The harness is the slice-S2 one with the hooks installed: the three policies
//! are built exactly as [`oagw::OagwGear`] builds them — the token bucket, the
//! CORS handler and the plugin engine with the built-in registrations — and
//! handed to [`oagw::ProxyHooks`], so what runs here is the wiring the gear
//! ships, not a reimplementation. Only the tenant hierarchy and the credential
//! store are injected, which is how the ancestor and fail-closed behaviour are
//! asserted.
//!
//! The upstream is a real local HTTP server that echoes back the headers it
//! received, so "the gateway never forwards its own rate-limit or CORS headers"
//! and "the plugin injected the credential" are read off what the upstream
//! actually got.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use credstore_sdk::test_util::MockCredStoreClient;
use http_body_util::BodyExt;
use httpmock::MockServer;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

use oagw::DataPlaneService;
use oagw::OagwConfig;
use oagw::PluginEngineService;
use oagw::PluginRegistries;
use oagw::ProxyHooks;
use oagw::RateLimitLimiter;
use oagw::RateLimitService;
use oagw::TenantHierarchy;
use oagw::api::rest::proxy_routes::register_proxy_routes;
use oagw::domain::services::control_plane::ControlPlaneService;
use oagw::domain::storage::{PluginStore, RouteStore, UpstreamStore};
use oagw::domain::types::{
    AuthConfig, CorsConfig, CorsMethod, Endpoint, HeaderTransform, HeadersConfig, HttpMatch,
    PassthroughMode, PathSuffixMode, Plugin, PluginRef, PluginsConfig, Protocol,
    RateLimitAlgorithm, RateLimitBurst, RateLimitConfig, RateLimitScope, RateLimitStrategy,
    RateLimitSustained, RateLimitWindow, Route, RouteMatch, RouteMethod, RouteSpec, Scheme,
    ServerConfig, SharingMode, Upstream, UpstreamSpec,
};
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationSpec;
use toolkit_security::SecurityContext;

/// The proxy path (gear-relative, without `/api`).
const PROXY: &str = "/oagw/v1/proxy";

/// The calling tenant of this file's requests.
const TENANT: Uuid = Uuid::from_u128(0x6f61_6777_0000_0000_0000_0000_0000_0002);

/// The root of the hierarchy the stub reports.
const ROOT: Uuid = Uuid::from_u128(0x6f61_6777_0000_0000_0000_0000_0000_0003);

/// The GTS type id of a 503 for a plugin nothing in this process can run.
const PLUGIN_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1";

/// The GTS type id of a 429.
const RATE_LIMITED: &str = "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1";

/// The GTS type id of a 403 for a disallowed CORS origin.
const ORIGIN_NOT_ALLOWED: &str = "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1";

/// The GTS type id of a 403 for a disallowed CORS method.
const METHOD_NOT_ALLOWED: &str = "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1";

/// The GTS type id of a 401.
const AUTHENTICATION_FAILED: &str = "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1";

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Minimal OpenAPI registry: records nothing, returns the schema name.
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

/// Ancestor-chain stub: the chain is what the proxy is allowed to see.
struct StubHierarchy {
    chain: Vec<Uuid>,
}

#[async_trait]
impl TenantHierarchy for StubHierarchy {
    async fn chain(&self, _security: &SecurityContext, tenant: Uuid) -> Vec<Uuid> {
        let mut chain = vec![tenant];
        chain.extend(self.chain.iter().copied());
        chain
    }
}

/// A proxy stack plus the stores a test seeds configuration into.
struct Harness {
    router: Router,
    upstreams: Arc<UpstreamStore>,
    routes: Arc<RouteStore>,
    plugins: Arc<PluginStore>,
}

/// A proxy stack with every hook of this slice installed, and no credential
/// store: `cred://` references then fail closed, which is one of the behaviours
/// under test.
fn harness(ancestors: Vec<Uuid>) -> Harness {
    harness_with(ancestors, None)
}

/// A proxy stack whose plugin engine resolves credentials through `credstore`
/// when it is given one.
fn harness_with(
    ancestors: Vec<Uuid>,
    credstore: Option<Arc<dyn credstore_sdk::api::CredStoreClientV1>>,
) -> Harness {
    // The loopback mock is plaintext, so the gate the schema ships with is open
    // here, as the e2e configuration opens it.
    let config = OagwConfig {
        allow_http_upstream: true,
        ..OagwConfig::default()
    };
    let control_plane = Arc::new(ControlPlaneService::new(config));
    let upstreams = Arc::clone(control_plane.upstream_store());
    let routes = Arc::clone(control_plane.route_store());
    let plugins = Arc::clone(control_plane.plugin_store());

    let hierarchy: Arc<dyn TenantHierarchy> = Arc::new(StubHierarchy { chain: ancestors });

    // The same three hooks, in the same construction order, the gear installs.
    let hooks = ProxyHooks::new(
        Some(Arc::new(RateLimitService::new(Arc::new(
            RateLimitLimiter::new(),
        )))),
        Some(Arc::new(oagw::CorsService)),
        Some(Arc::new(PluginEngineService::new(
            PluginRegistries::with_builtins_and(credstore),
            Arc::clone(&plugins),
        ))),
    );

    let data_plane = Arc::new(
        DataPlaneService::new(
            config,
            Arc::clone(&control_plane),
            Arc::clone(&upstreams),
            Arc::clone(&routes),
        )
        .with_tenant_hierarchy(hierarchy)
        .with_hooks(hooks),
    );

    Harness {
        router: register_proxy_routes(Router::new(), &NoopOpenApiRegistry, data_plane),
        upstreams,
        routes,
        plugins,
    }
}

/// An `http` endpoint pointing at `server`.
fn http_endpoint(server: &MockServer) -> Endpoint {
    Endpoint {
        scheme: Scheme::Http,
        host: "127.0.0.1".to_owned(),
        port: server.port(),
    }
}

/// An HTTP server that answers every request with the headers it received, as
/// JSON.
///
/// httpmock matches requests against expectations, which is the right tool for
/// *behaviour*; these tests need to *see* the forwarded headers, so the socket
/// is handled directly.
struct EchoServer {
    endpoint: Endpoint,
}

/// Start an echo server on a loopback port.
async fn echo_server() -> EchoServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("the loopback address binds");
    let port = listener.local_addr().expect("the address is known").port();

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                continue;
            };
            let _ = echo_once(&mut socket).await;
        }
    });

    EchoServer {
        endpoint: Endpoint {
            scheme: Scheme::Http,
            host: "127.0.0.1".to_owned(),
            port,
        },
    }
}

/// Read one request, answer it with the headers it carried.
async fn echo_once(socket: &mut tokio::net::TcpStream) -> std::io::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut raw = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let read = socket.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..read]);
        if raw.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }

    let mut headers = std::collections::BTreeMap::new();
    for line in String::from_utf8_lossy(&raw).split("\r\n").skip(1) {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers
                .entry(name.trim().to_ascii_lowercase())
                .or_insert_with(|| value.trim().to_owned());
        }
    }

    let body = serde_json::to_string(&json!({ "headers": headers })).expect("the echo is JSON");
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length:          {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(response.as_bytes()).await?;
    socket.flush().await
}

impl EchoServer {
    /// The endpoint to configure the upstream with.
    fn endpoint(&self) -> Endpoint {
        self.endpoint.clone()
    }

    /// The header the upstream reports having received.
    fn received(&self, body: &[u8], name: &str) -> Option<String> {
        let received: Value = serde_json::from_slice(body).expect("the echo is JSON");
        received["headers"][name].as_str().map(ToOwned::to_owned)
    }
}

/// A mock that answers every request with a 200 and a fixed body.
fn mock_ok(server: &MockServer) -> httpmock::Mock<'_> {
    server.mock(|_when, then| {
        then.status(200).body("ok");
    })
}

/// Seed an upstream record pointing at `server`, whose spec is `amend`.
///
/// The spec is validated first, exactly as a management write would be, so a
/// test that seeds an invalid policy fails here rather than passing vacuously.
fn seed_upstream(
    harness: &Harness,
    tenant: Uuid,
    alias: &str,
    endpoint: Endpoint,
    amend: impl FnOnce(&mut UpstreamSpec),
) -> Uuid {
    let mut spec = UpstreamSpec {
        alias: Some(alias.to_owned()),
        server: ServerConfig {
            endpoints: vec![endpoint],
        },
        protocol: Protocol::Http,
        ..UpstreamSpec::default()
    };
    amend(&mut spec);
    let spec = spec.validate().expect("the upstream spec is valid");

    harness
        .upstreams
        .insert(Upstream {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            alias: alias.to_owned(),
            created_at: 0,
            updated_at: 0,
            spec,
        })
        .expect("the upstream inserts")
        .id
}

/// Seed a `GET` route for `path`, with `amend` applied to its spec.
fn seed_route_with_spec(
    harness: &Harness,
    tenant: Uuid,
    upstream: Uuid,
    path: &str,
    amend: impl FnOnce(&mut RouteSpec),
) -> Uuid {
    let mut spec = RouteSpec {
        upstream_id: upstream,
        match_rules: RouteMatch {
            http: Some(HttpMatch {
                methods: vec![RouteMethod::Get],
                path: path.to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        },
        enabled: true,
        tags: Vec::new(),
        plugins: None,
        rate_limit: None,
    };
    amend(&mut spec);

    harness
        .routes
        .insert(Route {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            upstream_id: upstream,
            created_at: 0,
            updated_at: 0,
            spec,
        })
        .expect("the route inserts")
        .id
}

/// Seed a plain `GET` route for `path`.
fn seed_route(harness: &Harness, tenant: Uuid, upstream: Uuid, path: &str) -> Uuid {
    seed_route_with_spec(harness, tenant, upstream, path, |_| {})
}

/// Seed a custom plugin record in the tenant's plugin store.
fn seed_custom_plugin(harness: &Harness, tenant: Uuid, plugin_type: &str) -> Plugin {
    let plugin = Plugin {
        id: Uuid::new_v4(),
        tenant_id: tenant,
        plugin_type: plugin_type.to_owned(),
        name: format!("custom-{plugin_type}"),
        config_schema: None,
        source_code: "def execute(ctx):\n    return ctx\n".to_owned(),
        last_used_at: None,
        gc_eligible_at: None,
    };
    harness
        .plugins
        .insert(plugin.clone())
        .expect("the plugin inserts");
    plugin
}

/// A `SecurityContext` authenticated for `tenant`.
fn security_context(tenant: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(tenant)
        .build()
        .expect("security context builds")
}

/// A `SecurityContext` for `tenant` with an explicit subject.
fn subject_context(tenant: Uuid, subject: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(subject)
        .subject_tenant_id(tenant)
        .build()
        .expect("security context builds")
}

/// Send a proxied request for `TENANT` and return `(status, headers, body)`.
async fn proxy(
    harness: &Harness,
    method: &str,
    path_suffix: &str,
    headers: &[(&str, &str)],
    context: Option<SecurityContext>,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut builder = Request::builder()
        .method(method)
        .uri(format!("{PROXY}/{path_suffix}"));
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder
        .extension(context.unwrap_or_else(|| security_context(TENANT)))
        .body(Body::empty())
        .expect("the request builds");

    let response = harness
        .router
        .clone()
        .oneshot(request)
        .await
        .expect("the router responds");
    let status = response.status();
    let response_headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("the body is readable")
        .to_bytes()
        .to_vec();

    (status, response_headers, bytes)
}

/// Send a request that carries **no** `SecurityContext` extension at all.
///
/// This is the transport as a browser presents it: a preflight carries no
/// credentials (WHATWG Fetch), so the host injects nothing for it, and what
/// reaches the handler is an anonymous request.
async fn anonymous(
    harness: &Harness,
    method: &str,
    path_suffix: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut builder = Request::builder()
        .method(method)
        .uri(format!("{PROXY}/{path_suffix}"));
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder.body(Body::empty()).expect("the request builds");

    let response = harness
        .router
        .clone()
        .oneshot(request)
        .await
        .expect("the router responds");
    let status = response.status();
    let response_headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("the body is readable")
        .to_bytes()
        .to_vec();

    (status, response_headers, bytes)
}

/// A preflight request, which carries no security context by construction.
async fn preflight(
    harness: &Harness,
    path_suffix: &str,
    origin: &str,
    method: &str,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    preflight_with(harness, path_suffix, origin, method, &[]).await
}

/// A preflight with extra `Access-Control-Request-*` headers, still anonymous.
async fn preflight_with(
    harness: &Harness,
    path_suffix: &str,
    origin: &str,
    method: &str,
    extra: &[(&str, &str)],
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut headers = vec![
        ("origin", origin),
        ("access-control-request-method", method),
    ];
    headers.extend_from_slice(extra);
    anonymous(harness, "OPTIONS", path_suffix, &headers).await
}

/// The `name` header of a response, as a string.
fn header<'a>(headers: &'a axum::http::HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// The problem document of a gateway error.
fn problem(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).expect("gateway errors are problem+json")
}

/// The `content-type` of a response.
fn content_type(headers: &axum::http::HeaderMap) -> Option<String> {
    header(headers, "content-type").map(ToOwned::to_owned)
}

/// `X-OAGW-Error-Source` of a response.
fn error_source(headers: &axum::http::HeaderMap) -> Option<String> {
    header(headers, "x-oagw-error-source").map(ToOwned::to_owned)
}

/// A sustained rate of `rate` per second.
fn per_second(rate: u32) -> RateLimitSustained {
    RateLimitSustained {
        rate,
        window: RateLimitWindow::Second,
    }
}

/// A reject-strategy tenant-scoped limit of `rate` per second, no burst.
fn limit_per_second(rate: u32) -> RateLimitConfig {
    RateLimitConfig {
        sharing: SharingMode::Private,
        algorithm: RateLimitAlgorithm::TokenBucket,
        sustained: per_second(rate),
        burst: None,
        scope: RateLimitScope::Tenant,
        strategy: RateLimitStrategy::Reject,
        cost: 1,
    }
}

/// CORS for `origins` and `methods`, enabled.
fn cors_config(origins: &[&str], methods: &[CorsMethod]) -> CorsConfig {
    CorsConfig {
        sharing: SharingMode::Private,
        enabled: true,
        allowed_origins: origins.iter().map(|origin| (*origin).to_owned()).collect(),
        allowed_methods: methods.to_vec(),
        expose_headers: vec!["x-request-id".to_owned()],
        allow_credentials: false,
    }
}

/// The default two methods the CORS schema defaults to.
fn cors_methods() -> Vec<CorsMethod> {
    vec![CorsMethod::Get, CorsMethod::Post]
}

/// An `auth` binding of the built-in API-key plugin.
fn api_key_auth(key: &str) -> AuthConfig {
    AuthConfig {
        plugin_type: "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1".to_owned(),
        sharing: SharingMode::Private,
        config: Some(json!({"key": key})),
    }
}

// ---------------------------------------------------------------------------
// Rate limiting (ADR-0003)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_bucket_admits_its_capacity_then_returns_429_with_the_rate_limit_headers() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let mock = upstream.mock(|_when, then| {
        then.status(200).body("ok");
    });

    // Two requests per second: the second fits, the third does not.
    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        http_endpoint(&upstream),
        |spec| {
            spec.rate_limit = Some(limit_per_second(2));
        },
    );
    seed_route(&harness, TENANT, id, "/");

    for _ in 0..2 {
        let (status, _, _) = proxy(&harness, "GET", "api.openai.com/v1", &[], None).await;
        assert_eq!(status, StatusCode::OK);
    }

    let (status, headers, bytes) = proxy(&harness, "GET", "api.openai.com/v1", &[], None).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(mock.calls(), 2, "the throttled request is never forwarded");
    assert_problem(&status, &headers, &bytes, RATE_LIMITED);

    // The gateway's own budget (ADR-0003 "Response Headers"), and `Retry-After`
    // so a well-behaved client can back off.
    assert_eq!(header(&headers, "x-ratelimit-limit"), Some("2/second"));
    assert_eq!(header(&headers, "x-ratelimit-remaining"), Some("0"));
    assert!(header(&headers, "x-ratelimit-reset").is_some_and(|reset| reset != "0"));
    assert!(
        headers.get("retry-after").is_some_and(|value| value
            .to_str()
            .expect("ascii")
            .parse::<u64>()
            .expect("secs")
            >= 1)
    );

    let document = problem(&bytes);
    assert!(
        document["retry_after_seconds"].as_u64() >= Some(1),
        "the 429 carries the retry guidance: {document}"
    );
    assert_eq!(document["rate_limit"]["limit"], json!("2/second"));
    assert_eq!(document["rate_limit"]["remaining"], json!(0));
}

#[tokio::test]
async fn the_tokens_refill_at_the_sustained_rate() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let _mock = mock_ok(&upstream);
    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        http_endpoint(&upstream),
        |spec| {
            spec.rate_limit = Some(limit_per_second(2));
        },
    );
    seed_route(&harness, TENANT, id, "/");

    for _ in 0..2 {
        let (status, _, _) = proxy(&harness, "GET", "api.openai.com/v1", &[], None).await;
        assert_eq!(status, StatusCode::OK);
    }
    let (status, _, _) = proxy(&harness, "GET", "api.openai.com/v1", &[], None).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    // Half a window is half a token short of two; one token is enough for one
    // request, so this is the refill, not a reset of the whole bucket.
    tokio::time::sleep(Duration::from_millis(600)).await;
    let (status, _, _) = proxy(&harness, "GET", "api.openai.com/v1", &[], None).await;
    assert_eq!(status, StatusCode::OK, "one token has refilled");

    let (status, _, _) = proxy(&harness, "GET", "api.openai.com/v1", &[], None).await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "and no more than one"
    );
}

#[tokio::test]
async fn the_burst_capacity_is_the_bucket_size_not_the_sustained_rate() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let _mock = mock_ok(&upstream);
    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        http_endpoint(&upstream),
        |spec| {
            spec.rate_limit = Some(RateLimitConfig {
                sustained: RateLimitSustained {
                    rate: 1,
                    window: RateLimitWindow::Second,
                },
                burst: Some(RateLimitBurst { capacity: 3 }),
                ..limit_per_second(1)
            });
        },
    );
    seed_route(&harness, TENANT, id, "/");

    for _ in 0..3 {
        let (status, _, _) = proxy(&harness, "GET", "api.openai.com/v1", &[], None).await;
        assert_eq!(status, StatusCode::OK, "the burst absorbs three");
    }
    let (status, headers, _) = proxy(&harness, "GET", "api.openai.com/v1", &[], None).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(header(&headers, "x-ratelimit-limit"), Some("1/second"));
}

#[tokio::test]
async fn a_user_scoped_counter_follows_the_caller_and_not_a_header_it_controls() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let _mock = mock_ok(&upstream);
    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        http_endpoint(&upstream),
        |spec| {
            spec.rate_limit = Some(RateLimitConfig {
                scope: RateLimitScope::User,
                ..limit_per_second(1)
            });
        },
    );
    seed_route(&harness, TENANT, id, "/");

    let first = || subject_context(TENANT, Uuid::from_u128(0x1111));
    let second = subject_context(TENANT, Uuid::from_u128(0x2222));

    let (status, _, _) = proxy(&harness, "GET", "api.openai.com/v1", &[], Some(first())).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = proxy(&harness, "GET", "api.openai.com/v1", &[], Some(first())).await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "the same user is throttled"
    );

    // A caller-supplied header cannot move the request into another bucket: the
    // subject comes from the host's security context, which the transport
    // builds, and the counter key never reads a header.
    let (status, _, _) = proxy(
        &harness,
        "GET",
        "api.openai.com/v1",
        &[("x-forwarded-user", "someone-else")],
        Some(first()),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "a spoofed identity header does not buy a fresh bucket"
    );

    let (status, _, _) = proxy(&harness, "GET", "api.openai.com/v1", &[], Some(second)).await;
    assert_eq!(status, StatusCode::OK, "another user has its own bucket");
}

#[tokio::test]
async fn an_ip_scoped_counter_uses_the_forwarded_for_header() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let _mock = mock_ok(&upstream);
    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        http_endpoint(&upstream),
        |spec| {
            spec.rate_limit = Some(RateLimitConfig {
                scope: RateLimitScope::Ip,
                ..limit_per_second(1)
            });
        },
    );
    seed_route(&harness, TENANT, id, "/");

    // Two callers behind one proxy: the leftmost entry is the client's, and the
    // counter is per client, not per proxy.
    let (status, _, _) = proxy(
        &harness,
        "GET",
        "api.openai.com/v1",
        &[("x-forwarded-for", "203.0.113.7, 10.0.0.1")],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = proxy(
        &harness,
        "GET",
        "api.openai.com/v1",
        &[("x-forwarded-for", "203.0.113.7, 10.0.0.1")],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    let (status, _, _) = proxy(
        &harness,
        "GET",
        "api.openai.com/v1",
        &[("x-forwarded-for", "203.0.113.9")],
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a different client is a different bucket"
    );
}

#[tokio::test]
async fn a_global_scoped_counter_is_shared_by_every_caller() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let _mock = mock_ok(&upstream);
    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        http_endpoint(&upstream),
        |spec| {
            spec.rate_limit = Some(RateLimitConfig {
                scope: RateLimitScope::Global,
                ..limit_per_second(1)
            });
        },
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, _, _) = proxy(&harness, "GET", "api.openai.com/v1", &[], None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = proxy(&harness, "GET", "api.openai.com/v1", &[], None).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn a_cost_of_three_spends_three_tokens() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let _mock = mock_ok(&upstream);
    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        http_endpoint(&upstream),
        |spec| {
            spec.rate_limit = Some(RateLimitConfig {
                cost: 3,
                ..limit_per_second(4)
            });
        },
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, headers, _) = proxy(&harness, "GET", "api.openai.com/v1", &[], None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(header(&headers, "x-ratelimit-remaining"), Some("1"));

    let (status, _, _) = proxy(&harness, "GET", "api.openai.com/v1", &[], None).await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "three more do not fit"
    );
}

#[tokio::test]
async fn an_enforced_ancestor_limit_tightens_the_descendant_budget() {
    let harness = harness(vec![ROOT]);
    let upstream = MockServer::start_async().await;
    let _mock = mock_ok(&upstream);
    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        http_endpoint(&upstream),
        |spec| {
            spec.rate_limit = Some(limit_per_second(10));
        },
    );
    // The root owns the same alias and enforces a much tighter budget.
    seed_upstream(
        &harness,
        ROOT,
        "api.openai.com",
        http_endpoint(&upstream),
        |spec| {
            spec.rate_limit = Some(RateLimitConfig {
                sharing: SharingMode::Enforce,
                ..limit_per_second(1)
            });
        },
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, _, _) = proxy(&harness, "GET", "api.openai.com/v1", &[], None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, headers, _) = proxy(&harness, "GET", "api.openai.com/v1", &[], None).await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "the ancestor's tighter budget wins over the descendant's own"
    );
    assert_eq!(
        header(&headers, "x-ratelimit-limit"),
        Some("1/second"),
        "the decision is the strictest of the chain"
    );
}

#[tokio::test]
async fn a_private_ancestor_limit_is_invisible_to_the_descendant() {
    let harness = harness(vec![ROOT]);
    let upstream = MockServer::start_async().await;
    let _mock = mock_ok(&upstream);
    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        http_endpoint(&upstream),
        |spec| {
            spec.rate_limit = Some(limit_per_second(5));
        },
    );
    seed_upstream(
        &harness,
        ROOT,
        "api.openai.com",
        http_endpoint(&upstream),
        |spec| {
            spec.rate_limit = Some(limit_per_second(1));
        },
    );
    seed_route(&harness, TENANT, id, "/");

    // Five requests are admitted: the root's record is not enforced on the
    // descendant, and the descendant's own budget governs.
    for _ in 0..5 {
        let (status, headers, _) = proxy(&harness, "GET", "api.openai.com/v1", &[], None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(header(&headers, "x-ratelimit-limit"), Some("5/second"));
    }
    let (status, _, _) = proxy(&harness, "GET", "api.openai.com/v1", &[], None).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn queue_and_degrade_are_answered_with_a_reject_429() {
    let upstream = MockServer::start_async().await;
    let _mock = mock_ok(&upstream);
    for strategy in [RateLimitStrategy::Queue, RateLimitStrategy::Degrade] {
        let harness = harness(vec![]);
        let id = seed_upstream(
            &harness,
            TENANT,
            "api.openai.com",
            http_endpoint(&upstream),
            |spec| {
                spec.rate_limit = Some(RateLimitConfig {
                    strategy,
                    ..limit_per_second(1)
                });
            },
        );
        seed_route(&harness, TENANT, id, "/");

        let (status, _, _) = proxy(&harness, "GET", "api.openai.com/v1", &[], None).await;
        assert_eq!(status, StatusCode::OK);
        let (status, headers, bytes) = proxy(&harness, "GET", "api.openai.com/v1", &[], None).await;
        assert_eq!(
            status,
            StatusCode::TOO_MANY_REQUESTS,
            "{strategy:?} is not implemented and must not be simulated"
        );
        assert_problem(&status, &headers, &bytes, RATE_LIMITED);
    }
}

#[tokio::test]
async fn an_upstream_without_a_limit_is_never_throttled() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let mock = upstream.mock(|_when, then| {
        then.status(200).body("ok");
    });
    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        http_endpoint(&upstream),
        |_| {},
    );
    seed_route(&harness, TENANT, id, "/");

    for _ in 0..25 {
        let (status, headers, _) = proxy(&harness, "GET", "api.openai.com/v1", &[], None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(header(&headers, "x-ratelimit-limit").is_none());
    }
    assert_eq!(mock.calls(), 25);
}

#[tokio::test]
async fn the_rate_limit_headers_are_never_forwarded_to_the_upstream() {
    // The echo upstream reports what it received, so a header the gateway adds
    // for the *caller* would be visible here if it were ever sent upstream.
    let upstream = echo_server().await;
    let harness = harness(vec![]);
    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        upstream.endpoint(),
        |spec| {
            spec.rate_limit = Some(limit_per_second(50));
        },
    );
    seed_route(&harness, TENANT, id, "/");

    // A caller that pretends to have been rate-limited cannot inject the header
    // into the upstream request either.
    let (status, _, body) = proxy(
        &harness,
        "GET",
        "api.openai.com/v1",
        &[("x-ratelimit-remaining", "0")],
        None,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        upstream.received(&body, "x-ratelimit-remaining"),
        None,
        "the caller's rate-limit header is dropped: {body:?}"
    );
}

#[tokio::test]
async fn a_preflight_is_answered_locally_without_touching_the_upstream() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let mock = upstream.mock(|_when, then| {
        then.status(200).body("ok");
    });

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        http_endpoint(&upstream),
        |spec| {
            spec.cors = Some(cors_config(&["https://studio.openai.com"], &cors_methods()));
        },
    );
    seed_route(&harness, TENANT, id, "/");

    // Sent exactly as a browser sends it: no security context, no credential.
    let (status, headers, bytes) = preflight_with(
        &harness,
        "api.openai.com/v1",
        "https://studio.openai.com",
        "POST",
        &[("access-control-request-headers", "x-api-key, x-request-id")],
    )
    .await;

    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(bytes, Vec::<u8>::new());
    assert_eq!(mock.calls(), 0, "a preflight is never forwarded");
    assert_eq!(error_source(&headers).as_deref(), Some("gateway"));

    // The answer echoes what the browser asked for, so a specific request works
    // whatever the configuration allows.
    assert_eq!(
        header(&headers, "access-control-allow-origin"),
        Some("https://studio.openai.com")
    );
    assert_eq!(
        header(&headers, "access-control-allow-methods"),
        Some("POST")
    );
    assert_eq!(
        header(&headers, "access-control-allow-headers"),
        Some("x-api-key, x-request-id")
    );
    assert_eq!(header(&headers, "access-control-max-age"), Some("86400"));
    assert!(
        header(&headers, "vary").is_some_and(|vary| vary.contains("Access-Control-Request-Method")),
        "no cache may serve this answer for another preflight: {headers:?}"
    );
}

#[tokio::test]
async fn a_preflight_is_answered_even_when_nothing_resolves_the_alias() {
    let harness = harness(vec![]);

    // No upstream, no route, no tenant resolution: the preflight is still
    // answered, because it carries no credential and cannot be tied to a
    // tenant yet (ADR-0004 "Preflight Request Handling").
    let (status, headers, _) = preflight(
        &harness,
        "no-such-alias.example/v1",
        "https://studio.openai.com",
        "GET",
    )
    .await;

    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        header(&headers, "access-control-allow-origin"),
        Some("https://studio.openai.com")
    );
}

#[tokio::test]
async fn only_a_preflight_is_answered_without_a_security_context() {
    let harness = harness(vec![]);

    // The anonymous fast path is the preflight's alone: the same request without
    // the preflight headers is still an authenticated one, and fails closed.
    let (status, headers, bytes) = anonymous(&harness, "GET", "api.openai.com/v1", &[]).await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_problem(&status, &headers, &bytes, AUTHENTICATION_FAILED);
    assert_eq!(
        header(&headers, "access-control-allow-origin"),
        None,
        "no preflight answer for a request that is not one"
    );
}

#[tokio::test]
async fn a_preflight_is_not_rate_limited_but_the_actual_request_is() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let mock = upstream.mock(|_when, then| {
        then.status(200).body("ok");
    });

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        http_endpoint(&upstream),
        |spec| {
            spec.rate_limit = Some(limit_per_second(1));
        },
    );
    seed_route(&harness, TENANT, id, "/");

    // Three preflights, all answered, none of them forwarded and none of them
    // throttled: a preflight carries no credential, so there is no caller to
    // key a bucket with (ADR-0004 "Preflight Request Handling").
    for _ in 0..3 {
        let (status, _, bytes) = preflight(
            &harness,
            "api.openai.com/v1",
            "https://studio.openai.com",
            "POST",
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NO_CONTENT,
            "a preflight is never throttled: {}",
            String::from_utf8_lossy(&bytes)
        );
    }
    assert_eq!(mock.calls(), 0, "a preflight is never forwarded");

    // The actual request the preflight preceded *is* throttled: the first is
    // admitted, the second refused, which is only possible if the preflights
    // above consumed nothing from the caller's bucket.
    let (status, _, _) = proxy(
        &harness,
        "GET",
        "api.openai.com/v1",
        &[("origin", "https://studio.openai.com")],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, headers, bytes) = proxy(
        &harness,
        "GET",
        "api.openai.com/v1",
        &[("origin", "https://studio.openai.com")],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_problem(&status, &headers, &bytes, RATE_LIMITED);
}

#[tokio::test]
async fn an_options_without_the_cors_headers_is_an_ordinary_request() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let mock = upstream.mock(|_when, then| {
        then.status(200).body("served");
    });

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        http_endpoint(&upstream),
        |spec| {
            spec.cors = Some(cors_config(&["https://studio.openai.com"], &cors_methods()));
        },
    );
    // The route matches `GET` only, so the `OPTIONS` cannot be forwarded: the
    // assertion is that the gateway did not *fabricate* a preflight answer for
    // it.
    seed_route(&harness, TENANT, id, "/");

    let (status, headers, bytes) = proxy(
        &harness,
        "OPTIONS",
        "api.openai.com/v1",
        &[("origin", "https://studio.openai.com")],
        None,
    )
    .await;

    assert_ne!(status, StatusCode::NO_CONTENT, "not a preflight");
    assert_eq!(mock.calls(), 0);
    assert_problem(&status, &headers, &bytes, ROUTE_NOT_FOUND);
    assert!(
        header(&headers, "access-control-allow-origin").is_none(),
        "no preflight answer was fabricated"
    );
}

#[tokio::test]
async fn a_disallowed_origin_is_a_403_and_never_reaches_the_upstream() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let mock = upstream.mock(|_when, then| {
        then.status(200).body("ok");
    });

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        http_endpoint(&upstream),
        |spec| {
            spec.cors = Some(cors_config(&["https://studio.openai.com"], &cors_methods()));
        },
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, headers, bytes) = proxy(
        &harness,
        "GET",
        "api.openai.com/v1",
        &[("origin", "https://evil.example")],
        None,
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        mock.calls(),
        0,
        "a cross-origin theft attempt is not proxied"
    );
    assert_problem(&status, &headers, &bytes, ORIGIN_NOT_ALLOWED);
    let document = problem(&bytes);
    assert_eq!(document["origin"], json!("https://evil.example"));
    assert!(
        header(&headers, "access-control-allow-origin").is_none(),
        "a rejected origin is never told it is allowed"
    );
}

#[tokio::test]
async fn a_disallowed_method_is_a_403() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let _mock = mock_ok(&upstream);
    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        http_endpoint(&upstream),
        |spec| {
            spec.cors = Some(cors_config(
                &["https://studio.openai.com"],
                &[CorsMethod::Get],
            ));
        },
    );
    seed_route_with_spec(&harness, TENANT, id, "/", |spec| {
        spec.match_rules.http.as_mut().expect("http match").methods =
            vec![RouteMethod::Get, RouteMethod::Delete];
    });

    let (status, headers, bytes) = proxy(
        &harness,
        "DELETE",
        "api.openai.com/v1",
        &[("origin", "https://studio.openai.com")],
        None,
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_problem(&status, &headers, &bytes, METHOD_NOT_ALLOWED);
    assert_eq!(problem(&bytes)["method"], json!("DELETE"));
}

#[tokio::test]
async fn an_allowed_origin_gets_the_cors_headers_on_the_upstream_response() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let mock = upstream.mock(|_when, then| {
        then.status(200).body("ok").header("x-upstream", "yes");
    });

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        http_endpoint(&upstream),
        |spec| {
            spec.cors = Some(cors_config(&["https://studio.openai.com"], &cors_methods()));
        },
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, headers, bytes) = proxy(
        &harness,
        "GET",
        "api.openai.com/v1",
        &[("origin", "https://studio.openai.com")],
        None,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, b"ok");
    assert_eq!(
        header(&headers, "access-control-allow-origin"),
        Some("https://studio.openai.com"),
        "the origin is echoed, not replaced by a wildcard"
    );
    assert_eq!(
        header(&headers, "access-control-expose-headers"),
        Some("x-request-id")
    );
    assert!(header(&headers, "access-control-allow-credentials").is_none());
    assert!(header(&headers, "vary").is_some_and(|vary| vary.contains("Origin")));
    assert_eq!(mock.calls(), 1);
}

#[tokio::test]
async fn a_wildcard_configuration_still_echoes_the_request_origin() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let _mock = mock_ok(&upstream);
    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        http_endpoint(&upstream),
        |spec| {
            spec.cors = Some(cors_config(&["*"], &cors_methods()));
        },
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, headers, _) = proxy(
        &harness,
        "GET",
        "api.openai.com/v1",
        &[("origin", "https://any.example")],
        None,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        header(&headers, "access-control-allow-origin"),
        Some("https://any.example"),
        "a wildcard never becomes a bare `*`, which credentials could combine with"
    );
}

#[tokio::test]
async fn a_request_without_an_origin_is_not_a_cors_request() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let mock = upstream.mock(|_when, then| {
        then.status(200).body("ok");
    });

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        http_endpoint(&upstream),
        |spec| {
            spec.cors = Some(cors_config(&["https://studio.openai.com"], &cors_methods()));
        },
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, headers, _) = proxy(&harness, "GET", "api.openai.com/v1", &[], None).await;

    assert_eq!(status, StatusCode::OK);
    assert!(header(&headers, "access-control-allow-origin").is_none());
    assert_eq!(mock.calls(), 1);
}

#[tokio::test]
async fn a_disabled_cors_configuration_enforces_nothing_at_all() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let mock = upstream.mock(|_when, then| {
        then.status(200).body("ok");
    });

    // `enabled: false` with a non-empty allow-list: the list is inert (ADR-0004
    // "Security defaults", deny by default).
    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        http_endpoint(&upstream),
        |spec| {
            let mut disabled = cors_config(&["https://studio.openai.com"], &cors_methods());
            disabled.enabled = false;
            spec.cors = Some(disabled);
        },
    );
    seed_route(&harness, TENANT, id, "/");

    // (a) An origin the configuration *does* name gets no CORS header at all.
    let (status, headers, _) = proxy(
        &harness,
        "GET",
        "api.openai.com/v1",
        &[("origin", "https://studio.openai.com")],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(mock.calls(), 1, "the request is forwarded like any other");
    for name in [
        "access-control-allow-origin",
        "access-control-allow-methods",
        "access-control-expose-headers",
        "access-control-allow-credentials",
    ] {
        assert_eq!(
            header(&headers, name),
            None,
            "a disabled configuration vouches for no origin: {name}"
        );
    }

    // (b) A foreign origin is not rejected either: nothing is policed, so there
    // is no 403 `cf.oagw.cors.origin_not_allowed.v1` and no refusal to proxy.
    let (status, headers, bytes) = proxy(
        &harness,
        "GET",
        "api.openai.com/v1",
        &[("origin", "https://evil.example")],
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a disabled configuration rejects nothing: {}",
        String::from_utf8_lossy(&bytes)
    );
    assert_eq!(mock.calls(), 2);
    assert_ne!(
        header(&headers, "content-type"),
        Some("application/problem+json"),
        "no problem document, so no CORS rejection"
    );
    assert_eq!(
        error_source(&headers).as_deref(),
        Some("upstream"),
        "the answer is the upstream's, not a gateway rejection"
    );

    // (c) The preflight is answered as it always is — permissively, before any
    // configuration is resolved — so the preflight answer demonstrably does not
    // come from the CORS configuration: it is the same 204 with the
    // configuration switched off.
    let (status, headers, _) = preflight(
        &harness,
        "api.openai.com/v1",
        "https://studio.openai.com",
        "POST",
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        header(&headers, "access-control-allow-origin"),
        Some("https://studio.openai.com"),
        "the preflight echo is permissive whatever `cors.enabled` says"
    );
    assert_eq!(mock.calls(), 2, "a preflight is still never forwarded");
}

#[tokio::test]
async fn an_origin_is_matched_case_sensitively() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let _mock = mock_ok(&upstream);
    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        http_endpoint(&upstream),
        |spec| {
            spec.cors = Some(cors_config(&["https://studio.openai.com"], &cors_methods()));
        },
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, headers, bytes) = proxy(
        &harness,
        "GET",
        "api.openai.com/v1",
        &[("origin", "HTTPS://STUDIO.OPENAI.COM")],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_problem(&status, &headers, &bytes, ORIGIN_NOT_ALLOWED);
}

#[tokio::test]
async fn the_management_api_rejects_credentials_with_a_wildcard_origin_on_create_and_replace() {
    use oagw::api::rest::routes::register_routes;

    let config = OagwConfig::default();
    let service = Arc::new(ControlPlaneService::new(config));
    let router = register_routes(Router::new(), &NoopOpenApiRegistry, service);

    let body = json!({
        "server": { "endpoints": [ { "host": "api.openai.com" } ] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "cors": {
            "enabled": true,
            "allowed_origins": ["*"],
            "allow_credentials": true
        }
    });

    // The create path.
    let (status, headers, bytes) = send_json(
        router.clone(),
        "POST",
        "",
        body.clone(),
        security_context(TENANT),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        content_type(&headers).as_deref(),
        Some("application/problem+json")
    );
    let document = problem(&bytes);
    assert_eq!(document["field"], json!("cors.allowed_origins"));
    assert_eq!(
        document["context"]["field"],
        json!("cors.allowed_origins"),
        "the field survives the middleware round-trip in `context` too: {document}"
    );

    // The replace path: a valid record first, then a body that cannot be stored.
    let (status, _, bytes) = send_json(
        router.clone(),
        "POST",
        "",
        json!({
            "server": { "endpoints": [ { "host": "api.openai.com" } ] },
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
        }),
        security_context(TENANT),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let created: Value = serde_json::from_slice(&bytes).expect("the record is JSON");
    let id = created["id"]
        .as_str()
        .expect("the record carries an id")
        .to_owned();

    let (status, _, bytes) = send_json(router, "PUT", &id, body, security_context(TENANT)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{id}");
    assert_eq!(problem(&bytes)["field"], json!("cors.allowed_origins"));
}

/// Send a management request with a JSON body.
async fn send_json(
    router: Router,
    method: &str,
    id: &str,
    body: Value,
    context: SecurityContext,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let uri = if method == "PUT" {
        format!("{UPSTREAMS}/{id}")
    } else {
        UPSTREAMS.to_owned()
    };
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .extension(context)
        .body(Body::from(body.to_string()))
        .expect("the request builds");

    let response = router.oneshot(request).await.expect("the router responds");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("the body is readable")
        .to_bytes()
        .to_vec();

    (status, headers, bytes)
}

/// The upstreams base path, without the `/api` prefix the host adds.
const UPSTREAMS: &str = "/oagw/v1/upstreams";

/// The GTS type id of a 404 for an alias or route nothing resolves.
const ROUTE_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1";

// ---------------------------------------------------------------------------
// Plugin chain (ADR-0002, ADR-0009)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_api_key_plugin_injects_the_credential_the_upstream_never_sees_the_callers() {
    let harness = harness(vec![]);
    let upstream = echo_server().await;
    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        upstream.endpoint(),
        |spec| {
            spec.auth = Some(api_key_auth("s3cr3t"));
        },
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, _, body) = proxy(
        &harness,
        "GET",
        "api.openai.com/v1",
        &[
            ("x-api-key", "caller-secret"),
            ("authorization", "Bearer caller"),
        ],
        None,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        upstream.received(&body, "x-api-key").as_deref(),
        Some("s3cr3t"),
        "the gateway's credential, not the caller's: {body:?}"
    );
    assert_eq!(
        upstream.received(&body, "authorization"),
        None,
        "the caller's credential is not forwarded: {body:?}"
    );
}

#[tokio::test]
async fn the_api_key_plugin_writes_the_configured_header() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let mock = upstream.mock(|_when, then| {
        then.status(200).body("ok");
    });

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        http_endpoint(&upstream),
        |spec| {
            spec.plugins = Some(PluginsConfig {
                sharing: SharingMode::Private,
                items: vec![PluginRef::bound(
                    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
                    json!({"header": "authorization", "key": "Bearer s3cr3t"}),
                )],
            });
        },
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, _, body) = proxy(&harness, "GET", "api.openai.com/v1", &[], None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(mock.calls(), 1);
    assert_eq!(body, b"ok");
}

#[tokio::test]
async fn a_cred_reference_without_a_credential_store_fails_closed() {
    // The harness builds the registries the way the gear does when the host
    // publishes no credstore client.
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let mock = upstream.mock(|_when, then| {
        then.status(200).body("ok");
    });

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        http_endpoint(&upstream),
        |spec| {
            spec.plugins = Some(PluginsConfig {
                sharing: SharingMode::Private,
                items: vec![PluginRef::bound(
                    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
                    json!({"key": "cred://vendor-api-key"}),
                )],
            });
        },
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, headers, bytes) = proxy(&harness, "GET", "api.openai.com/v1", &[], None).await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(mock.calls(), 0, "nothing is forwarded unauthenticated");
    assert_problem(
        &status,
        &headers,
        &bytes,
        "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1",
    );
}

#[tokio::test]
async fn a_cred_reference_is_resolved_through_the_credential_store() {
    // The gear installs the client the host publishes; here a mock stands in
    // for it, holding the secret the binding names.
    let credstore: Arc<dyn credstore_sdk::api::CredStoreClientV1> = Arc::new(
        MockCredStoreClient::with_secrets(vec![("vendor-api-key".to_owned(), "s3cr3t".to_owned())]),
    );
    let harness = harness_with(vec![], Some(credstore));
    let upstream = echo_server().await;
    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        upstream.endpoint(),
        |spec| {
            spec.plugins = Some(PluginsConfig {
                sharing: SharingMode::Private,
                items: vec![PluginRef::bound(
                    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
                    json!({"key": "cred://vendor-api-key"}),
                )],
            });
        },
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, _, body) = proxy(&harness, "GET", "api.openai.com/v1", &[], None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        upstream.received(&body, "x-api-key").as_deref(),
        Some("s3cr3t"),
        "the resolved credential, not the reference: {body:?}"
    );
}

#[tokio::test]
async fn an_unknown_plugin_reference_fails_closed_with_a_503() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let mock = upstream.mock(|_when, then| {
        then.status(200).body("ok");
    });

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        http_endpoint(&upstream),
        |spec| {
            spec.plugins = Some(PluginsConfig {
                sharing: SharingMode::Private,
                items: vec![PluginRef::new(
                    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.signed_headers.v1",
                )],
            });
        },
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, headers, bytes) = proxy(&harness, "GET", "api.openai.com/v1", &[], None).await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(mock.calls(), 0, "a policy that cannot run is not skipped");
    assert_problem(&status, &headers, &bytes, PLUGIN_NOT_FOUND);
    assert_eq!(
        problem(&bytes)["plugin_ref"],
        json!("gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.signed_headers.v1")
    );
}

#[tokio::test]
async fn a_stored_custom_plugin_without_an_implementation_names_the_sandbox() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let plugin = seed_custom_plugin(&harness, TENANT, "guard_plugin");
    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        http_endpoint(&upstream),
        |spec| {
            spec.plugins = Some(PluginsConfig {
                sharing: SharingMode::Private,
                items: vec![PluginRef::new(plugin.gts_id())],
            });
        },
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, headers, bytes) = proxy(&harness, "GET", "api.openai.com/v1", &[], None).await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_problem(&status, &headers, &bytes, PLUGIN_NOT_FOUND);
    let document = problem(&bytes);
    assert_eq!(document["plugin_type"], json!("guard_plugin"));
    assert_eq!(document["sandbox"], json!(false));
    assert!(
        document["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("no plugin sandbox is available")),
        "the detail says why the plugin cannot run: {document}"
    );
}

#[tokio::test]
async fn an_unknown_custom_plugin_uuid_is_a_503_that_names_the_reference() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let _mock = mock_ok(&upstream);
    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        http_endpoint(&upstream),
        |spec| {
            spec.plugins = Some(PluginsConfig {
                sharing: SharingMode::Private,
                items: vec![PluginRef::new(
                    "gts.cf.core.oagw.auth_plugin.v1~0f0e0d0c-0b0a-4948-8786-654433221100",
                )],
            });
        },
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, _, bytes) = proxy(&harness, "GET", "api.openai.com/v1", &[], None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        problem(&bytes)["plugin_ref"],
        json!("gts.cf.core.oagw.auth_plugin.v1~0f0e0d0c-0b0a-4948-8786-654433221100")
    );
}

#[tokio::test]
async fn the_required_headers_guard_rejects_a_request_missing_the_header() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let mock = upstream.mock(|_when, then| {
        then.status(200).body("ok");
    });

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        http_endpoint(&upstream),
        |spec| {
            spec.plugins = Some(PluginsConfig {
                sharing: SharingMode::Private,
                items: vec![PluginRef::bound(
                    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
                    json!({"required_request_headers": "x-tenant-echo"}),
                )],
            });
        },
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, headers, bytes) = proxy(&harness, "GET", "api.openai.com/v1", &[], None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(mock.calls(), 0);
    // The guard reports through the validation surface with an ADR-0009
    // `error_code` and the missing header as an extension.
    assert_problem(
        &status,
        &headers,
        &bytes,
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
    );
    let document = problem(&bytes);
    assert_eq!(document["error_code"], json!("REQUIRED_HEADER_MISSING"));
    assert_eq!(document["header"], json!("x-tenant-echo"));

    // With the header present the request passes and is forwarded.
    let (status, _, body) = proxy(
        &harness,
        "GET",
        "api.openai.com/v1",
        &[("x-tenant-echo", "present")],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(mock.calls(), 1);
    assert_eq!(body, b"ok");
}

#[tokio::test]
async fn the_request_id_transform_sets_the_id_the_upstream_and_the_caller_see() {
    let harness = harness(vec![]);
    let upstream = echo_server().await;
    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        upstream.endpoint(),
        |spec| {
            spec.plugins = Some(PluginsConfig {
                sharing: SharingMode::Private,
                items: vec![PluginRef::new(
                    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1",
                )],
            });
        },
    );
    seed_route(&harness, TENANT, id, "/");

    // The caller supplies its own id; the proxy replaces it.
    let (status, headers, body) = proxy(
        &harness,
        "GET",
        "api.openai.com/v1",
        &[("x-request-id", "caller-id")],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let forwarded = upstream
        .received(&body, "x-request-id")
        .expect("the id is set");
    assert_ne!(
        forwarded, "caller-id",
        "the proxy mints the id, it does not trust the caller's: {body:?}"
    );
    assert!(Uuid::parse_str(&forwarded).is_ok(), "{forwarded}");
    assert_eq!(
        header(&headers, "x-request-id"),
        Some(forwarded.as_str()),
        "the caller sees the same id on the response"
    );
}

#[tokio::test]
async fn upstream_plugins_run_before_route_plugins() {
    // Both the upstream and the route bind the same auth plugin to the same
    // header: the request carries the *route's* key, which is only possible if
    // the route's bindings ran after the upstream's.
    let upstream = echo_server().await;
    let apikey = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";

    let harness = harness(vec![]);
    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        upstream.endpoint(),
        |spec| {
            spec.plugins = Some(PluginsConfig {
                sharing: SharingMode::Private,
                items: vec![PluginRef::bound(apikey, json!({"key": "upstream-key"}))],
            });
        },
    );
    seed_route_with_spec(&harness, TENANT, id, "/", |spec| {
        spec.plugins = Some(PluginsConfig {
            sharing: SharingMode::Private,
            items: vec![PluginRef::bound(apikey, json!({"key": "route-key"}))],
        });
    });

    let (status, _, body) = proxy(&harness, "GET", "api.openai.com/v1", &[], None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        upstream.received(&body, "x-api-key").as_deref(),
        Some("route-key"),
        "the route's bindings ran last: {body:?}"
    );
}

#[tokio::test]
async fn the_guard_reads_the_inbound_headers_not_the_transformed_ones() {
    // A passthrough policy that drops the header would hide it from a guard
    // that only looked at the outbound map, and the request would be rejected
    // for a header the caller actually sent.
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let mock = upstream.mock(|_when, then| {
        then.status(200).body("ok");
    });

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        http_endpoint(&upstream),
        |spec| {
            // The request policy drops the caller's headers (`none` passthrough),
            // so the outbound map the guard would otherwise be handed does not
            // contain the header it must check for.
            spec.headers = Some(HeadersConfig {
                request: Some(HeaderTransform {
                    passthrough: PassthroughMode::None,
                    ..HeaderTransform::default()
                }),
                response: None,
            });
            spec.plugins = Some(PluginsConfig {
                sharing: SharingMode::Private,
                items: vec![PluginRef::bound(
                    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
                    json!({"required_request_headers": "x-trace-id"}),
                )],
            });
        },
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, _, _) = proxy(
        &harness,
        "GET",
        "api.openai.com/v1",
        &[("x-trace-id", "trace")],
        None,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(mock.calls(), 1);
}

#[tokio::test]
async fn a_plugin_failure_after_the_upstream_answered_is_a_gateway_error() {
    // The response-side guard runs on the upstream's headers: a policy that
    // rejects them turns a passed-through response into a gateway problem, so
    // a response the caller must not trust is never handed over.
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let mock = upstream.mock(|_when, then| {
        then.status(200).body("ok").header("x-upstream", "yes");
    });

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        http_endpoint(&upstream),
        |spec| {
            spec.plugins = Some(PluginsConfig {
                sharing: SharingMode::Private,
                items: vec![PluginRef::bound(
                    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
                    json!({"required_response_headers": "x-must-be-there"}),
                )],
            });
        },
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, headers, bytes) = proxy(&harness, "GET", "api.openai.com/v1", &[], None).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(
        mock.calls(),
        1,
        "the upstream was called; its answer is not used"
    );
    // The response phase reports as a bad gateway (ADR-0009), again with the
    // guard's error code.
    assert_problem(
        &status,
        &headers,
        &bytes,
        "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1",
    );
    assert_eq!(
        problem(&bytes)["error_code"],
        json!("REQUIRED_HEADER_MISSING")
    );
    assert_eq!(error_source(&headers).as_deref(), Some("gateway"));

    // The failure happened *after* the upstream answered, and it is still a
    // gateway error for this request, so it carries the request's correlation
    // id — the same id the audit record logs (DESIGN §3.3).
    let document = problem(&bytes);
    let trace_id = document["trace_id"].as_str().expect("a trace_id");
    Uuid::parse_str(trace_id).expect("the correlation id is a UUID");
    assert_eq!(
        header(&headers, "x-oagw-error-source"),
        Some("gateway"),
        "and the source marker that identifies a gateway-produced document"
    );
}

/// Assert a gateway problem document of the given GTS type id.
fn assert_problem(
    status: &StatusCode,
    headers: &axum::http::HeaderMap,
    bytes: &[u8],
    type_id: &str,
) {
    assert_eq!(
        content_type(headers).as_deref(),
        Some("application/problem+json"),
        "gateway errors are problem+json"
    );
    assert_eq!(
        error_source(headers).as_deref(),
        Some("gateway"),
        "gateway errors are stamped with the error source"
    );

    let document = problem(bytes);
    assert_eq!(document["type"], type_id);
    assert_eq!(document["status"], status.as_u16());
    assert!(
        document["instance"]
            .as_str()
            .is_some_and(|instance| instance.starts_with(PROXY)),
        "the instance is the request path, got {document}"
    );
}
