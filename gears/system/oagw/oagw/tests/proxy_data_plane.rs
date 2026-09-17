//! Data-plane tests: the proxy path (`Router` + `oneshot`) against a **real
//! local HTTP server**.
//!
//! The proxy resolves an alias, matches a route, selects an endpoint and
//! forwards the request to an httpmock server, so forwarding, header
//! transformation and passthrough behaviour are exercised over a socket rather
//! than against a stubbed client. Only the tenant hierarchy is injected: the
//! ancestor chain is exactly what the test allows the proxy to see, which is
//! also how tenant isolation and shadowing are asserted.
//!
//! Configuration is seeded straight into the stores the control plane shares
//! with the data plane (the management API is not under test here, and slice S2
//! exposes no route endpoints at all).

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use httpmock::MockServer;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;
use uuid::Uuid;

use oagw::DataPlaneService;
use oagw::OagwConfig;
use oagw::TenantHierarchy;
use oagw::api::rest::proxy_routes::register_proxy_routes;
use oagw::domain::services::control_plane::ControlPlaneService;
use oagw::domain::storage::{RouteStore, UpstreamStore};
use oagw::domain::types::{
    Endpoint, HeadersConfig, HttpMatch, PathSuffixMode, Protocol, Route, RouteMatch, RouteMethod,
    RouteSpec, Scheme, ServerConfig, Upstream, UpstreamSpec,
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

/// A tenant the hierarchy does *not* relate `TENANT` to.
const UNRELATED: Uuid = Uuid::from_u128(0x6f61_6777_0000_0000_0000_0000_0000_0004);

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
}

/// A proxy stack whose ancestor chain (above `TENANT`) is `ancestors`.
fn harness(ancestors: Vec<Uuid>) -> Harness {
    harness_with(2, true, ancestors)
}

/// A proxy stack with an explicit upstream deadline in seconds.
fn harness_with(
    proxy_timeout_secs: u64,
    allow_http_upstream: bool,
    ancestors: Vec<Uuid>,
) -> Harness {
    let config = OagwConfig {
        proxy_timeout_secs,
        allow_http_upstream,
        ..OagwConfig::default()
    };
    let control_plane = Arc::new(ControlPlaneService::new(config));
    let upstreams = Arc::clone(control_plane.upstream_store());
    let routes = Arc::clone(control_plane.route_store());

    let hierarchy: Arc<dyn TenantHierarchy> = Arc::new(StubHierarchy { chain: ancestors });

    let data_plane = Arc::new(
        DataPlaneService::new(
            config,
            Arc::clone(&control_plane),
            Arc::clone(&upstreams),
            Arc::clone(&routes),
        )
        .with_tenant_hierarchy(hierarchy),
    );

    Harness {
        router: register_proxy_routes(Router::new(), &NoopOpenApiRegistry, data_plane),
        upstreams,
        routes,
    }
}

/// A plaintext endpoint pointing at `server`.
fn http_endpoint(server: &MockServer) -> Endpoint {
    Endpoint {
        scheme: Scheme::Http,
        host: "127.0.0.1".to_owned(),
        port: server.port(),
    }
}

/// An `https` endpoint on `host` at the standard port.
fn tls_endpoint(host: &str) -> Endpoint {
    Endpoint {
        scheme: Scheme::Https,
        host: host.to_owned(),
        port: 443,
    }
}

/// Seed an upstream record directly into the shared store.
///
/// The record is *not* passed through `UpstreamSpec::validate_for_create`: the
/// management API is not under test here, and the round-robin pool below needs
/// two loopback endpoints on distinct ports, which the control-plane pool rule
/// (one scheme and one port per upstream) would reject.
fn seed_upstream(harness: &Harness, tenant: Uuid, alias: &str, endpoints: Vec<Endpoint>) -> Uuid {
    seed_upstream_with(harness, tenant, alias, endpoints, true, None)
}

/// Seed an upstream record with an explicit enabled flag and header rules.
fn seed_upstream_with(
    harness: &Harness,
    tenant: Uuid,
    alias: &str,
    endpoints: Vec<Endpoint>,
    enabled: bool,
    headers: Option<HeadersConfig>,
) -> Uuid {
    let mut spec = UpstreamSpec {
        enabled,
        alias: Some(alias.to_owned()),
        server: ServerConfig { endpoints },
        protocol: Protocol::Http,
        headers,
        ..UpstreamSpec::default()
    };
    // Normalizing (host case, alias form) is worth keeping: resolution relies on
    // the stored alias being lowercase.
    spec = spec.validate().expect("the upstream spec normalizes");

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

/// Seed a load-balancing pool of loopback endpoints.
///
/// `UpstreamSpec::validate` requires every endpoint of an upstream to share one
/// scheme and port, and these two servers necessarily listen on different ports;
/// the pool rule is a management-API concern, while what is under test here is
/// how the data plane balances across the endpoints it was given.
fn seed_pool(harness: &Harness, tenant: Uuid, alias: &str, endpoints: Vec<Endpoint>) -> Uuid {
    let spec = UpstreamSpec {
        alias: Some(alias.to_owned()),
        server: ServerConfig { endpoints },
        protocol: Protocol::Http,
        ..UpstreamSpec::default()
    };

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

/// Seed an enabled `GET` route for `path`.
fn seed_route(harness: &Harness, tenant: Uuid, upstream: Uuid, path: &str) -> Uuid {
    seed_route_with(
        harness,
        tenant,
        upstream,
        HttpMatch {
            methods: vec![RouteMethod::Get],
            path: path.to_owned(),
            query_allowlist: Vec::new(),
            path_suffix_mode: PathSuffixMode::Append,
        },
    )
}

/// Seed a route with a fully specified HTTP match.
fn seed_route_with(harness: &Harness, tenant: Uuid, upstream: Uuid, http: HttpMatch) -> Uuid {
    harness
        .routes
        .insert(Route {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            upstream_id: upstream,
            created_at: 0,
            updated_at: 0,
            spec: RouteSpec {
                upstream_id: upstream,
                match_rules: RouteMatch {
                    http: Some(http),
                    grpc: None,
                },
                enabled: true,
                tags: Vec::new(),
                plugins: None,
                rate_limit: None,
            },
        })
        .expect("the route inserts")
        .id
}

/// Seed a disabled route: it must be invisible to the matcher.
fn seed_disabled_route(harness: &Harness, tenant: Uuid, upstream: Uuid, path: &str) -> Uuid {
    let id = seed_route(harness, tenant, upstream, path);
    let route = harness.routes.get(tenant, id).expect("the route exists");
    let mut spec = route.spec.clone();
    spec.enabled = false;
    harness
        .routes
        .replace(tenant, id, spec, 0)
        .expect("the route updates");
    id
}

/// Seed a `POST` route with an `append` suffix.
fn post_route(harness: &Harness, tenant: Uuid, upstream: Uuid) {
    seed_route_with(
        harness,
        tenant,
        upstream,
        HttpMatch {
            methods: vec![RouteMethod::Post],
            path: "/v1".to_owned(),
            query_allowlist: Vec::new(),
            path_suffix_mode: PathSuffixMode::Append,
        },
    );
}

/// A `SecurityContext` authenticated for `tenant`.
fn security_context(tenant: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(tenant)
        .build()
        .expect("security context builds")
}

/// Send a proxied request and return `(status, headers, body bytes)`.
async fn send(
    harness: &Harness,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
    tenant: Option<Uuid>,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    if let Some(tenant) = tenant {
        builder = builder.extension(security_context(tenant));
    }
    let request = builder
        .body(Body::from(body.unwrap_or_default().to_owned()))
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
        .to_bytes();

    (status, response_headers, bytes.to_vec())
}

/// An authenticated proxy request for `TENANT`.
async fn proxy(
    harness: &Harness,
    method: &str,
    path_suffix: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    send(
        harness,
        method,
        &format!("{PROXY}/{path_suffix}"),
        headers,
        body,
        Some(TENANT),
    )
    .await
}

/// Send a proxied request whose body is a stream, not a buffered string.
///
/// The length guards are enforced while the body is *streamed*, so this is the
/// only way to exercise a body that turns out to be too large or truncated.
async fn send_streaming(
    harness: &Harness,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    body: Body,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder
        .extension(security_context(TENANT))
        .body(body)
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
        .to_bytes();

    (status, response_headers, bytes.to_vec())
}

/// A chunked body of `chunks` one-mebibyte blocks, streamed lazily.
///
/// Nothing is buffered: the stream yields the same buffer again and again, so a
/// body far larger than the limit costs no more memory than one chunk.
fn oversized_chunked_body(chunks: usize) -> Body {
    let chunk = vec![b'a'; 1024 * 1024];
    Body::from_stream(futures_util::stream::unfold(chunks, move |remaining| {
        let chunk = chunk.clone();
        async move {
            if remaining == 0 {
                None
            } else {
                Some((
                    Ok::<_, std::io::Error>(axum::body::Bytes::from(chunk)),
                    remaining - 1,
                ))
            }
        }
    }))
}

/// `X-OAGW-Error-Source` of a response.
fn error_source(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get("x-oagw-error-source")
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
}

/// The `content-type` of a response.
fn content_type(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
}

/// The problem document of a gateway error.
fn problem(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).expect("gateway errors are problem+json")
}

/// Assert a gateway problem document of the given GTS type id.
fn assert_problem(
    status: StatusCode,
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

/// A loopback port that is closed: bound and immediately released again.
fn closed_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("a loopback port binds")
        .local_addr()
        .expect("the local address is known")
        .port()
}

// ---------------------------------------------------------------------------
// Echo upstream: a real HTTP server that reports exactly what it received
// ---------------------------------------------------------------------------

/// A minimal HTTP server that answers every request with the request line and
/// headers it received, as JSON.
///
/// httpmock matches a request against expectations, which is the right tool for
/// *behaviour*; these tests need to *see* the forwarded headers, so the socket
/// is handled directly.
struct EchoServer {
    endpoint: Endpoint,
}

/// Start an echo server on a loopback port.
async fn echo_server() -> EchoServer {
    echo_server_on("127.0.0.1", "127.0.0.1:0").await
}

/// Start an echo server on `addr`, announcing itself as `host`.
///
/// `"[::1]:0"` gives an IPv6 loopback server, which is how the bracketed
/// authority of an IPv6 endpoint is exercised over a real socket.
async fn echo_server_on(host: &str, addr: &str) -> EchoServer {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("the address binds");
    let port = listener.local_addr().expect("the address is known").port();

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                continue;
            };

            let mut raw = Vec::new();
            let mut chunk = [0_u8; 1024];
            loop {
                match socket.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        raw.extend_from_slice(&chunk[..read]);
                        if raw.windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                    }
                }
            }

            let received = echo_report(&String::from_utf8_lossy(&raw));
            let body = received.to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: \
                 {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
    });

    EchoServer {
        endpoint: Endpoint {
            scheme: Scheme::Http,
            host: host.to_owned(),
            port,
        },
    }
}

impl EchoServer {
    /// The endpoint to configure the upstream with.
    fn endpoint(&self) -> Endpoint {
        self.endpoint.clone()
    }
}

/// Parse the received request into `{ path, headers }`.
///
/// Header names are lowercased; a repeated header keeps only its first value,
/// which is enough for these assertions.
fn echo_report(raw: &str) -> Value {
    let mut lines = raw.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_owned();

    let mut headers = std::collections::BTreeMap::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers
                .entry(name.trim().to_ascii_lowercase())
                .or_insert_with(|| value.trim().to_owned());
        }
    }

    json!({
        "path": path,
        "headers": headers,
    })
}

/// The `name` header the echo upstream reports, if any.
fn echoed<'a>(received: &'a Value, name: &str) -> Option<&'a str> {
    received["headers"][name].as_str()
}

// ---------------------------------------------------------------------------
// Alias resolution
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_alias_reaches_the_upstream_and_the_response_is_passed_through() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let mock = upstream.mock(|when, then| {
        when.path("/v1/models");
        then.status(200).body("ok").header("x-upstream", "yes");
    });

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        vec![http_endpoint(&upstream)],
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, headers, bytes) =
        proxy(&harness, "GET", "api.openai.com/v1/models", &[], None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(error_source(&headers).as_deref(), Some("upstream"));
    assert_eq!(bytes, b"ok");
    assert_eq!(
        headers
            .get("x-upstream")
            .and_then(|value| value.to_str().ok()),
        Some("yes"),
        "upstream response headers are forwarded"
    );
    mock.assert_calls(1);
}

#[tokio::test]
async fn alias_resolution_is_case_insensitive_and_tolerates_a_trailing_dot() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let mock = upstream.mock(|when, then| {
        when.path("/v1/models");
        then.status(204);
    });

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        vec![http_endpoint(&upstream)],
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, _, _) = proxy(&harness, "GET", "API.OpenAI.COM./v1/models", &[], None).await;

    assert_eq!(status, StatusCode::NO_CONTENT);
    mock.assert_calls(1);
}

#[tokio::test]
async fn the_alias_alone_is_proxied_without_a_sub_path() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let mock = upstream.mock(|when, then| {
        when.path("/");
        then.status(204);
    });

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        vec![http_endpoint(&upstream)],
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, _, _) = proxy(&harness, "GET", "api.openai.com", &[], None).await;

    assert_eq!(status, StatusCode::NO_CONTENT);
    mock.assert_calls(1);
}

#[tokio::test]
async fn an_unknown_alias_is_a_404_problem_document() {
    let harness = harness(vec![]);

    let (status, headers, bytes) =
        proxy(&harness, "GET", "missing.example.com/v1", &[], None).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_problem(
        status,
        &headers,
        &bytes,
        "gts.cf.core.errors.err.v1~cf.oagw.upstream.not_found.v1",
    );
    assert_eq!(problem(&bytes)["alias"], "missing.example.com");
}

#[tokio::test]
async fn an_ancestor_upstream_is_reachable_through_the_tenant_chain() {
    let harness = harness(vec![ROOT]);
    let upstream = MockServer::start_async().await;
    let mock = upstream.mock(|when, then| {
        when.path("/inherited");
        then.status(200);
    });

    let id = seed_upstream(
        &harness,
        ROOT,
        "shared.vendor.com",
        vec![http_endpoint(&upstream)],
    );
    seed_route(&harness, ROOT, id, "/");

    let (status, _, _) = proxy(&harness, "GET", "shared.vendor.com/inherited", &[], None).await;

    assert_eq!(status, StatusCode::OK, "the ancestor upstream is inherited");
    mock.assert_calls(1);
}

#[tokio::test]
async fn the_descendant_shadows_the_ancestor_upstream() {
    let harness = harness(vec![ROOT]);
    let ancestor = MockServer::start_async().await;
    let descendant = MockServer::start_async().await;
    let ancestor_mock = ancestor.mock(|_when, then| {
        then.status(500);
    });
    let descendant_mock = descendant.mock(|when, then| {
        when.path("/same");
        then.status(200);
    });

    let ancestor_id = seed_upstream(
        &harness,
        ROOT,
        "shared.vendor.com",
        vec![http_endpoint(&ancestor)],
    );
    let descendant_id = seed_upstream(
        &harness,
        TENANT,
        "shared.vendor.com",
        vec![http_endpoint(&descendant)],
    );
    seed_route(&harness, ROOT, ancestor_id, "/");
    seed_route(&harness, TENANT, descendant_id, "/");

    let (status, _, _) = proxy(&harness, "GET", "shared.vendor.com/same", &[], None).await;

    assert_eq!(status, StatusCode::OK, "the nearest tenant wins");
    assert_eq!(descendant_mock.calls(), 1);
    assert_eq!(ancestor_mock.calls(), 0);
}

#[tokio::test]
async fn a_disabled_upstream_is_a_503_and_does_not_reveal_the_ancestor() {
    let harness = harness(vec![ROOT]);
    let ancestor = MockServer::start_async().await;
    let ancestor_mock = ancestor.mock(|_when, then| {
        then.status(200);
    });

    seed_upstream_with(
        &harness,
        TENANT,
        "down.vendor.com",
        vec![tls_endpoint("down.vendor.com")],
        false,
        None,
    );
    let ancestor_id = seed_upstream(
        &harness,
        ROOT,
        "down.vendor.com",
        vec![http_endpoint(&ancestor)],
    );
    seed_route(&harness, ROOT, ancestor_id, "/");

    let (status, headers, bytes) = proxy(&harness, "GET", "down.vendor.com/x", &[], None).await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(error_source(&headers).as_deref(), Some("gateway"));
    let document = problem(&bytes);
    assert_eq!(
        document["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1"
    );
    assert!(
        headers.get("retry-after").is_some(),
        "a disabled upstream is retriable"
    );
    assert_eq!(ancestor_mock.calls(), 0, "the ancestor is never consulted");
}

#[tokio::test]
async fn a_tenant_outside_the_chain_cannot_reach_another_tenants_upstream() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let mock = upstream.mock(|_when, then| {
        then.status(200);
    });

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        vec![http_endpoint(&upstream)],
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, headers, bytes) = send(
        &harness,
        "GET",
        &format!("{PROXY}/api.openai.com/v1"),
        &[],
        None,
        Some(UNRELATED),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "the alias is invisible to an unrelated tenant"
    );
    assert_eq!(error_source(&headers).as_deref(), Some("gateway"));
    assert_eq!(mock.calls(), 0);
    assert_eq!(
        problem(&bytes)["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.upstream.not_found.v1"
    );
}

#[tokio::test]
async fn an_unauthenticated_proxy_request_is_rejected_with_401() {
    let harness = harness(vec![]);

    let (status, headers, bytes) = send(
        &harness,
        "GET",
        &format!("{PROXY}/api.openai.com/v1"),
        &[],
        None,
        None,
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(error_source(&headers).as_deref(), Some("gateway"));
    assert_eq!(
        problem(&bytes)["instance"],
        json!(format!("{PROXY}/api.openai.com/v1"))
    );
}

// ---------------------------------------------------------------------------
// Route matching
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_longest_matching_route_prefix_wins() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    upstream.mock(|_when, then| {
        then.status(201).body("specific");
    });

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        vec![http_endpoint(&upstream)],
    );
    // The broad route allows no query parameter at all, the specific one allows
    // exactly `specific`: only the specific route can answer `?specific=1`, so
    // the parameter is observable proof of which route won.
    seed_route_with(
        &harness,
        TENANT,
        id,
        HttpMatch {
            methods: vec![RouteMethod::Get],
            path: "/v1".to_owned(),
            query_allowlist: Vec::new(),
            path_suffix_mode: PathSuffixMode::Append,
        },
    );
    seed_route_with(
        &harness,
        TENANT,
        id,
        HttpMatch {
            methods: vec![RouteMethod::Get],
            path: "/v1/chat".to_owned(),
            query_allowlist: vec!["specific".to_owned()],
            path_suffix_mode: PathSuffixMode::Append,
        },
    );

    let (status, _, bytes) = proxy(
        &harness,
        "GET",
        "api.openai.com/v1/chat/completions?specific=1",
        &[],
        None,
    )
    .await;

    assert_eq!(status, StatusCode::CREATED, "the longest prefix wins");
    assert_eq!(bytes, b"specific");

    // Without the parameter the broad route answers, which shows both routes
    // are live and the choice above was made by path length, not by accident.
    let (status, _, _) = proxy(&harness, "GET", "api.openai.com/v1/other", &[], None).await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
async fn a_method_outside_the_route_allowlist_is_a_404() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        vec![http_endpoint(&upstream)],
    );
    seed_route(&harness, TENANT, id, "/v1");

    let (status, headers, bytes) = proxy(&harness, "POST", "api.openai.com/v1", &[], None).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_problem(
        status,
        &headers,
        &bytes,
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1",
    );
}

#[tokio::test]
async fn a_disabled_route_is_simply_not_matched() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        vec![http_endpoint(&upstream)],
    );
    seed_disabled_route(&harness, TENANT, id, "/v1");

    let (status, headers, bytes) =
        proxy(&harness, "GET", "api.openai.com/v1/models", &[], None).await;

    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a disabled route is invisible"
    );
    assert_problem(
        status,
        &headers,
        &bytes,
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1",
    );
}

#[tokio::test]
async fn the_transport_accepts_any_method_and_the_domain_filters_it() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let patch_mock = upstream.mock(|when, then| {
        when.method("PATCH").path("/v1/thing");
        then.status(200);
    });

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        vec![http_endpoint(&upstream)],
    );
    seed_route_with(
        &harness,
        TENANT,
        id,
        HttpMatch {
            methods: vec![RouteMethod::Get, RouteMethod::Patch],
            path: "/v1".to_owned(),
            query_allowlist: Vec::new(),
            path_suffix_mode: PathSuffixMode::Append,
        },
    );

    let (status, _, _) = proxy(&harness, "PATCH", "api.openai.com/v1/thing", &[], None).await;

    assert_eq!(
        status,
        StatusCode::OK,
        "PATCH is forwarded, not filtered by the transport"
    );
    patch_mock.assert_calls(1);
}

#[tokio::test]
async fn path_suffix_mode_disabled_rejects_a_suffix_with_400() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        vec![http_endpoint(&upstream)],
    );
    seed_route_with(
        &harness,
        TENANT,
        id,
        HttpMatch {
            methods: vec![RouteMethod::Get],
            path: "/v1/status".to_owned(),
            query_allowlist: Vec::new(),
            path_suffix_mode: PathSuffixMode::Disabled,
        },
    );

    let (status, headers, bytes) =
        proxy(&harness, "GET", "api.openai.com/v1/status/extra", &[], None).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_problem(
        status,
        &headers,
        &bytes,
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
    );
    assert_eq!(
        problem(&bytes)["field"],
        json!("match.http.path_suffix_mode"),
        "the suffix mode is what rejected the request"
    );
}

#[tokio::test]
async fn a_query_parameter_outside_the_allowlist_is_a_400() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    upstream.mock(|_when, then| {
        then.status(200);
    });

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        vec![http_endpoint(&upstream)],
    );
    seed_route_with(
        &harness,
        TENANT,
        id,
        HttpMatch {
            methods: vec![RouteMethod::Get],
            path: "/v1".to_owned(),
            query_allowlist: vec!["api-version".to_owned()],
            path_suffix_mode: PathSuffixMode::Append,
        },
    );

    let (status, _, _) = proxy(
        &harness,
        "GET",
        "api.openai.com/v1/models?debug=1",
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _, _) = proxy(
        &harness,
        "GET",
        "api.openai.com/v1/models?api-version=1",
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the allowlisted parameter passes");
}

#[tokio::test]
async fn an_empty_allowlist_allows_no_query_parameter() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    upstream.mock(|_when, then| {
        then.status(200);
    });

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        vec![http_endpoint(&upstream)],
    );
    seed_route(&harness, TENANT, id, "/v1");

    let (status, _, _) = proxy(&harness, "GET", "api.openai.com/v1/models", &[], None).await;
    assert_eq!(status, StatusCode::OK);

    let (status, _, _) = proxy(
        &harness,
        "GET",
        "api.openai.com/v1/models?api-version=1",
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_descendants_upstream_shadows_the_ancestors_routes() {
    let harness = harness(vec![ROOT]);
    let ancestor = MockServer::start_async().await;
    let descendant = MockServer::start_async().await;
    let ancestor_mock = ancestor.mock(|_when, then| {
        then.status(500);
    });
    let descendant_mock = descendant.mock(|_when, then| {
        then.status(202);
    });

    // One alias owned by both tenants, each with its own endpoint: only the
    // descendant's upstream and its routes may be consulted.
    let ancestor_id = seed_upstream(
        &harness,
        ROOT,
        "shared.vendor.com",
        vec![http_endpoint(&ancestor)],
    );
    let descendant_id = seed_upstream(
        &harness,
        TENANT,
        "shared.vendor.com",
        vec![http_endpoint(&descendant)],
    );
    seed_route(&harness, ROOT, ancestor_id, "/v1");
    seed_route(&harness, TENANT, descendant_id, "/v1/shared");

    let (status, _, _) = proxy(&harness, "GET", "shared.vendor.com/v1/shared", &[], None).await;

    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(descendant_mock.calls(), 1);
    assert_eq!(ancestor_mock.calls(), 0);
}

#[tokio::test]
async fn a_grpc_upstream_is_not_proxied_by_the_http_data_plane() {
    let harness = harness(vec![]);

    let spec = UpstreamSpec {
        alias: Some("svc.local".to_owned()),
        server: ServerConfig {
            endpoints: vec![tls_endpoint("svc.local")],
        },
        protocol: Protocol::Grpc,
        ..UpstreamSpec::default()
    }
    .validate()
    .expect("the grpc spec normalizes");

    let id = harness
        .upstreams
        .insert(Upstream {
            id: Uuid::new_v4(),
            tenant_id: TENANT,
            alias: "svc.local".to_owned(),
            created_at: 0,
            updated_at: 0,
            spec,
        })
        .expect("the upstream inserts")
        .id;
    seed_route(&harness, TENANT, id, "/");

    let (status, headers, bytes) = proxy(&harness, "GET", "svc.local/call", &[], None).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(error_source(&headers).as_deref(), Some("gateway"));
    let document = problem(&bytes);
    assert!(
        document["upstream_id"]
            .as_str()
            .is_some_and(|id| id.starts_with("gts.cf.core.oagw.upstream.v1~")),
        "the upstream id extension is present, got {document}"
    );
}

// ---------------------------------------------------------------------------
// Endpoint selection: the ADR-0001 Appendix A matrix
// ---------------------------------------------------------------------------

#[tokio::test]
async fn matrix_a_single_endpoint_needs_no_target_host() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let mock = upstream.mock(|_when, then| {
        then.status(200);
    });

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.vendor.com",
        vec![http_endpoint(&upstream)],
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, _, _) = proxy(&harness, "GET", "api.vendor.com/v1", &[], None).await;
    assert_eq!(status, StatusCode::OK, "no header needed");
    mock.assert_calls(1);
}

#[tokio::test]
async fn matrix_a_single_endpoint_validates_an_optional_target_host() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let mock = upstream.mock(|_when, then| {
        then.status(200);
    });

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.vendor.com",
        vec![http_endpoint(&upstream)],
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, _, _) = proxy(
        &harness,
        "GET",
        "api.vendor.com/v1",
        &[("x-oagw-target-host", "127.0.0.1")],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the header is optional but valid");
    mock.assert_calls(1);
}

#[tokio::test]
async fn matrix_a_common_suffix_alias_requires_the_target_host() {
    let harness = harness(vec![]);
    let id = seed_upstream(
        &harness,
        TENANT,
        "vendor.com",
        vec![tls_endpoint("us.vendor.com"), tls_endpoint("eu.vendor.com")],
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, headers, bytes) = proxy(&harness, "GET", "vendor.com/v1", &[], None).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_problem(
        status,
        &headers,
        &bytes,
        "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1",
    );
    let document = problem(&bytes);
    assert_eq!(
        document["valid_hosts"].as_array().map(Vec::len),
        Some(2),
        "the caller is told which hosts are valid, got {document}"
    );
}

#[tokio::test]
async fn matrix_an_unknown_target_host_is_a_400() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.vendor.com",
        vec![http_endpoint(&upstream)],
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, headers, bytes) = proxy(
        &harness,
        "GET",
        "api.vendor.com/v1",
        &[("x-oagw-target-host", "elsewhere.example.com")],
        None,
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_problem(
        status,
        &headers,
        &bytes,
        "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1",
    );
    assert_eq!(problem(&bytes)["invalid_value"], "elsewhere.example.com");
}

#[tokio::test]
async fn matrix_a_malformed_target_host_is_a_400() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.vendor.com",
        vec![http_endpoint(&upstream)],
    );
    seed_route(&harness, TENANT, id, "/");

    for value in ["127.0.0.1:8000", "127.0.0.1/path", "not a host", ""] {
        let (status, headers, bytes) = proxy(
            &harness,
            "GET",
            "api.vendor.com/v1",
            &[("x-oagw-target-host", value)],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "'{value}' is not a host");
        assert_problem(
            status,
            &headers,
            &bytes,
            "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1",
        );
    }
}

#[tokio::test]
async fn matrix_a_multi_endpoint_pool_round_robins_from_the_first_endpoint() {
    let harness = harness(vec![]);
    let first = MockServer::start_async().await;
    let second = MockServer::start_async().await;
    let first_mock = first.mock(|_when, then| {
        then.status(200).body("first");
    });
    let second_mock = second.mock(|_when, then| {
        then.status(200).body("second");
    });

    let id = seed_pool(
        &harness,
        TENANT,
        "my-service",
        vec![http_endpoint(&first), http_endpoint(&second)],
    );
    seed_route(&harness, TENANT, id, "/");

    let mut bodies = Vec::new();
    for _ in 0..4 {
        let (status, _, bytes) = proxy(&harness, "GET", "my-service/v1", &[], None).await;
        assert_eq!(status, StatusCode::OK);
        bodies.push(String::from_utf8(bytes).expect("the body is text"));
    }

    assert_eq!(bodies, ["first", "second", "first", "second"]);
    assert_eq!(first_mock.calls(), 2);
    assert_eq!(second_mock.calls(), 2);
}

#[tokio::test]
async fn matrix_a_valid_target_host_pins_the_endpoint() {
    let harness = harness(vec![]);
    let first = MockServer::start_async().await;
    let second = MockServer::start_async().await;
    let first_mock = first.mock(|_when, then| {
        then.status(200).body("first");
    });
    let second_mock = second.mock(|_when, then| {
        then.status(200).body("second");
    });

    let id = seed_pool(
        &harness,
        TENANT,
        "my-service",
        vec![http_endpoint(&first), http_endpoint(&second)],
    );
    seed_route(&harness, TENANT, id, "/");

    for _ in 0..2 {
        let (status, _, bytes) = proxy(
            &harness,
            "GET",
            "my-service/v1",
            &[("x-oagw-target-host", "127.0.0.1")],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            bytes, b"first",
            "the header pins the first matching endpoint"
        );
    }

    assert_eq!(first_mock.calls(), 2);
    assert_eq!(
        second_mock.calls(),
        0,
        "the header bypasses the round robin"
    );
}

// ---------------------------------------------------------------------------
// Header transformation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn caller_credentials_are_not_forwarded_by_default() {
    let harness = harness(vec![]);
    let upstream = echo_server().await;

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        vec![upstream.endpoint()],
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, _, bytes) = proxy(
        &harness,
        "GET",
        "api.openai.com/v1",
        &[
            ("authorization", "Bearer tenant-token"),
            ("cookie", "session=1"),
            ("x-oagw-target-host", "127.0.0.1"),
        ],
        None,
    )
    .await;
    let received = problem(&bytes);

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        echoed(&received, "authorization"),
        None,
        "the caller's credentials never reach the upstream by default"
    );
    assert_eq!(echoed(&received, "cookie"), None);
    assert_eq!(
        echoed(&received, "x-oagw-target-host"),
        None,
        "the gateway-owned header is stripped"
    );
    assert_eq!(
        echoed(&received, "host"),
        Some(format!("127.0.0.1:{}", upstream.endpoint.port).as_str()),
        "Host is replaced with the endpoint authority"
    );
}

#[tokio::test]
async fn caller_credentials_reach_the_upstream_only_with_passthrough_all() {
    let harness = harness(vec![]);
    let upstream = echo_server().await;

    let id = seed_upstream_with(
        &harness,
        TENANT,
        "api.openai.com",
        vec![upstream.endpoint()],
        true,
        Some(HeadersConfig {
            request: Some(oagw::domain::types::HeaderTransform {
                passthrough: oagw::domain::types::PassthroughMode::All,
                ..oagw::domain::types::HeaderTransform::default()
            }),
            response: None,
        }),
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, _, bytes) = proxy(
        &harness,
        "GET",
        "api.openai.com/v1",
        &[("authorization", "Bearer tenant-token"), ("cookie", "a=b")],
        None,
    )
    .await;
    let received = problem(&bytes);

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        echoed(&received, "authorization"),
        Some("Bearer tenant-token"),
        "passthrough all forwards the caller's credentials"
    );
    assert_eq!(echoed(&received, "cookie"), Some("a=b"));
}

#[tokio::test]
async fn gateway_owned_response_headers_never_reach_the_upstream_even_with_passthrough_all() {
    let harness = harness(vec![]);
    let upstream = echo_server().await;

    let id = seed_upstream_with(
        &harness,
        TENANT,
        "api.openai.com",
        vec![upstream.endpoint()],
        true,
        Some(HeadersConfig {
            request: Some(oagw::domain::types::HeaderTransform {
                passthrough: oagw::domain::types::PassthroughMode::All,
                ..oagw::domain::types::HeaderTransform::default()
            }),
            response: None,
        }),
    );
    seed_route(&harness, TENANT, id, "/");

    // Every header the gateway computes itself, sent by a caller who would like
    // the upstream to see a budget that was never spent, a CORS answer that was
    // never given, or a cache key that never varied.
    let smuggled: &[(&str, &str)] = &[
        ("x-ratelimit-limit", "1000000/second"),
        ("x-ratelimit-remaining", "99999"),
        ("x-ratelimit-reset", "0"),
        ("access-control-allow-origin", "*"),
        ("access-control-allow-methods", "DELETE"),
        ("access-control-allow-headers", "authorization"),
        ("access-control-expose-headers", "x-secret"),
        ("access-control-max-age", "999999"),
        ("access-control-allow-credentials", "true"),
        ("access-control-request-method", "DELETE"),
        ("access-control-request-headers", "authorization"),
        ("vary", "*"),
    ];

    let (status, _, bytes) = proxy(&harness, "GET", "api.openai.com/v1", smuggled, None).await;
    let received = problem(&bytes);

    assert_eq!(status, StatusCode::OK);
    for (name, _) in smuggled {
        assert_eq!(
            echoed(&received, name),
            None,
            "the gateway owns {name}; a caller cannot supply it: {received}"
        );
    }

    // The policy is not over-eager: an ordinary caller header is still forwarded
    // under `passthrough: all`.
    let (status, _, bytes) = proxy(
        &harness,
        "GET",
        "api.openai.com/v1",
        &[("x-caller-header", "still-mine")],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        echoed(&problem(&bytes), "x-caller-header"),
        Some("still-mine"),
        "only the gateway-owned names are dropped"
    );
}

#[tokio::test]
async fn passthrough_allowlist_forwards_only_the_listed_headers() {
    let harness = harness(vec![]);
    let upstream = echo_server().await;

    let id = seed_upstream_with(
        &harness,
        TENANT,
        "api.openai.com",
        vec![upstream.endpoint()],
        true,
        Some(HeadersConfig {
            request: Some(oagw::domain::types::HeaderTransform {
                passthrough: oagw::domain::types::PassthroughMode::Allowlist,
                passthrough_allowlist: vec!["x-trace-context".to_owned()],
                ..oagw::domain::types::HeaderTransform::default()
            }),
            response: None,
        }),
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, headers, bytes) = proxy(
        &harness,
        "GET",
        "api.openai.com/v1",
        &[
            ("x-trace-context", "trace=1"),
            ("authorization", "Bearer tenant-token"),
            ("x-unlisted", "nope"),
        ],
        None,
    )
    .await;
    let received = problem(&bytes);

    assert_eq!(status, StatusCode::OK);
    assert_eq!(error_source(&headers).as_deref(), Some("upstream"));
    assert_eq!(
        echoed(&received, "x-trace-context"),
        Some("trace=1"),
        "the allowlisted header is forwarded"
    );
    assert_eq!(
        echoed(&received, "authorization"),
        None,
        "credentials are not on the allowlist, so they are dropped"
    );
    assert_eq!(
        echoed(&received, "x-unlisted"),
        None,
        "a header outside the allowlist is dropped"
    );
}

#[tokio::test]
async fn hop_by_hop_headers_and_the_target_host_never_reach_the_upstream() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let mock = upstream.mock(|when, then| {
        when.header_missing("connection")
            .header_missing("keep-alive")
            .header_missing("x-oagw-target-host");
        then.status(200);
    });

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        vec![http_endpoint(&upstream)],
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, _, _) = proxy(
        &harness,
        "GET",
        "api.openai.com/v1",
        &[
            ("connection", "keep-alive"),
            ("keep-alive", "timeout=5"),
            ("x-oagw-target-host", "127.0.0.1"),
        ],
        None,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    mock.assert_calls(1);
}

#[tokio::test]
async fn host_is_replaced_with_the_endpoint_authority_and_request_rules_apply() {
    let harness = harness(vec![]);
    let upstream = echo_server().await;

    let mut set = std::collections::BTreeMap::new();
    set.insert("x-injected".to_owned(), "by-oagw".to_owned());
    let id = seed_upstream_with(
        &harness,
        TENANT,
        "api.openai.com",
        vec![upstream.endpoint()],
        true,
        Some(HeadersConfig {
            request: Some(oagw::domain::types::HeaderTransform {
                set,
                remove: vec!["x-dropped".to_owned()],
                passthrough: oagw::domain::types::PassthroughMode::All,
                ..oagw::domain::types::HeaderTransform::default()
            }),
            response: None,
        }),
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, _, bytes) = proxy(
        &harness,
        "GET",
        "api.openai.com/v1",
        &[("x-dropped", "yes"), ("host", "caller.example.com")],
        None,
    )
    .await;
    let received = problem(&bytes);

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        echoed(&received, "host"),
        Some(format!("127.0.0.1:{}", upstream.endpoint.port).as_str()),
        "Host is the endpoint authority, not the caller's"
    );
    assert_eq!(echoed(&received, "x-injected"), Some("by-oagw"));
    assert_eq!(
        echoed(&received, "x-dropped"),
        None,
        "the remove rule dropped the header"
    );
}

#[tokio::test]
async fn upstream_response_headers_are_transformed() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    upstream.mock(|_when, then| {
        then.status(200)
            .body("ok")
            .header("x-upstream", "yes")
            .header("x-server", "mock");
    });

    let mut set = std::collections::BTreeMap::new();
    set.insert("x-gateway".to_owned(), "oagw".to_owned());
    let id = seed_upstream_with(
        &harness,
        TENANT,
        "api.openai.com",
        vec![http_endpoint(&upstream)],
        true,
        Some(HeadersConfig {
            request: None,
            response: Some(oagw::domain::types::HeaderTransformResponse {
                set,
                remove: vec!["x-upstream".to_owned()],
                ..oagw::domain::types::HeaderTransformResponse::default()
            }),
        }),
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, headers, _) = proxy(&harness, "GET", "api.openai.com/v1", &[], None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get("x-gateway")
            .and_then(|value| value.to_str().ok()),
        Some("oagw"),
        "the response rule set the header"
    );
    assert!(
        headers.get("x-upstream").is_none(),
        "the response rule removed the upstream header"
    );
    assert_eq!(
        headers
            .get("x-server")
            .and_then(|value| value.to_str().ok()),
        Some("mock"),
        "unrelated upstream headers pass through"
    );
    assert_eq!(error_source(&headers).as_deref(), Some("upstream"));
}

// ---------------------------------------------------------------------------
// Forwarding
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_connect_failure_is_a_503_link_unavailable() {
    let harness = harness(vec![]);
    let port = closed_port();

    let id = seed_upstream(
        &harness,
        TENANT,
        "unreachable.vendor.com",
        vec![Endpoint {
            scheme: Scheme::Http,
            host: "127.0.0.1".to_owned(),
            port,
        }],
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, headers, bytes) =
        proxy(&harness, "GET", "unreachable.vendor.com/never", &[], None).await;

    assert_problem(
        status,
        &headers,
        &bytes,
        "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1",
    );
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let document = problem(&bytes);
    assert_eq!(document["status"], 503);
    assert_eq!(document["retry_after_seconds"], 1);
    assert_eq!(
        headers
            .get("retry-after")
            .and_then(|value| value.to_str().ok()),
        Some("1"),
        "the link is retryable, and the gateway says when"
    );
    assert!(document["host"].as_str().is_some());
    assert!(document["trace_id"].as_str().is_some());
}

#[tokio::test]
async fn an_upstream_that_does_not_answer_in_time_is_a_504() {
    let harness = harness_with(1, true, vec![]);
    let upstream = MockServer::start_async().await;
    upstream.mock(|_when, then| {
        then.status(200).delay(std::time::Duration::from_secs(5));
    });

    let id = seed_upstream(
        &harness,
        TENANT,
        "slow.vendor.com",
        vec![http_endpoint(&upstream)],
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, headers, bytes) = proxy(&harness, "GET", "slow.vendor.com/v1", &[], None).await;

    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(error_source(&headers).as_deref(), Some("gateway"));
    let document = problem(&bytes);
    assert_eq!(
        document["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1"
    );
    assert_eq!(document["timeout_seconds"], 1);
}

#[tokio::test]
async fn a_tls_endpoint_is_reported_as_unreachable_rather_than_downgraded() {
    let harness = harness(vec![]);

    let id = seed_upstream(
        &harness,
        TENANT,
        "tls.vendor.com",
        vec![tls_endpoint("tls.vendor.com")],
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, headers, bytes) = proxy(&harness, "GET", "tls.vendor.com/v1", &[], None).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(error_source(&headers).as_deref(), Some("gateway"));
    let document = problem(&bytes);
    assert_eq!(
        document["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1"
    );
    assert!(
        document["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("https"),
        "the failure names the scheme instead of silently downgrading it"
    );
}

#[tokio::test]
async fn the_forwarded_path_and_query_reach_the_upstream() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let mock = upstream.mock(|when, then| {
        when.path("/v1/models").query_param_exists("api-version");
        then.status(200);
    });

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        vec![http_endpoint(&upstream)],
    );
    seed_route_with(
        &harness,
        TENANT,
        id,
        HttpMatch {
            methods: vec![RouteMethod::Get],
            path: "/v1".to_owned(),
            query_allowlist: vec!["api-version".to_owned()],
            path_suffix_mode: PathSuffixMode::Append,
        },
    );

    let (status, _, _) = proxy(
        &harness,
        "GET",
        "api.openai.com/v1/models?api-version=2024",
        &[],
        None,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    mock.assert_calls(1);
}

// ---------------------------------------------------------------------------
// Body rules
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_malformed_content_length_is_a_400() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let mock = upstream.mock(|_when, then| {
        then.status(200);
    });

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        vec![http_endpoint(&upstream)],
    );
    post_route(&harness, TENANT, id);

    let (status, headers, bytes) = proxy(
        &harness,
        "POST",
        "api.openai.com/v1",
        &[("content-length", "not-a-number")],
        Some("payload"),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(mock.calls(), 0, "nothing is forwarded");
    assert_problem(
        status,
        &headers,
        &bytes,
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
    );
}

#[tokio::test]
async fn a_body_over_the_hard_limit_is_a_413() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let mock = upstream.mock(|_when, then| {
        then.status(200);
    });

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        vec![http_endpoint(&upstream)],
    );
    post_route(&harness, TENANT, id);

    let declared = 200_u64 * 1024 * 1024;
    let (status, headers, bytes) = proxy(
        &harness,
        "POST",
        "api.openai.com/v1",
        &[("content-length", &declared.to_string())],
        Some("tiny"),
    )
    .await;

    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(error_source(&headers).as_deref(), Some("gateway"));
    assert_eq!(mock.calls(), 0);
    assert_eq!(
        problem(&bytes)["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1"
    );
}

#[tokio::test]
async fn a_transfer_encoding_other_than_chunked_is_a_400() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let mock = upstream.mock(|_when, then| {
        then.status(200);
    });

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        vec![http_endpoint(&upstream)],
    );
    post_route(&harness, TENANT, id);

    let (status, _, bytes) = proxy(
        &harness,
        "POST",
        "api.openai.com/v1",
        &[("transfer-encoding", "gzip")],
        Some("payload"),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(mock.calls(), 0);
    assert_eq!(
        problem(&bytes)["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
}

#[tokio::test]
async fn the_request_body_is_streamed_to_the_upstream() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let mock = upstream.mock(|when, then| {
        when.body("{\"a\":1}");
        then.status(201).body("created");
    });

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        vec![http_endpoint(&upstream)],
    );
    post_route(&harness, TENANT, id);

    let (status, _, bytes) = proxy(
        &harness,
        "POST",
        "api.openai.com/v1/things",
        &[("content-length", "7")],
        Some("{\"a\":1}"),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(bytes, b"created");
    mock.assert_calls(1);
}

#[tokio::test]
async fn a_body_shorter_than_its_content_length_is_rejected() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let mock = upstream.mock(|_when, then| {
        then.status(200);
    });

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        vec![http_endpoint(&upstream)],
    );
    post_route(&harness, TENANT, id);

    // The body announces 8 bytes and delivers 3: the length guard fails the
    // request instead of forwarding a truncated payload.
    let (status, headers, bytes) = proxy(
        &harness,
        "POST",
        "api.openai.com/v1",
        &[("content-length", "8")],
        Some("abc"),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(mock.calls(), 0);
    assert_problem(
        status,
        &headers,
        &bytes,
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
    );
    let document = problem(&bytes);
    assert_eq!(document["field"], json!("Content-Length"));
    assert!(
        document["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("8 bytes")),
        "the detail is the guard's, not a transport symptom, got {document}"
    );
}

// ---------------------------------------------------------------------------
// Path-suffix traversal
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_dot_segment_in_the_suffix_is_a_400_and_is_never_forwarded() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let mock = upstream.mock(|_when, then| {
        then.status(200);
    });

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        vec![http_endpoint(&upstream)],
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, headers, bytes) = proxy(
        &harness,
        "GET",
        "api.openai.com/v1/public/../private",
        &[],
        None,
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(mock.calls(), 0, "a traversal is never dialed");
    assert_problem(
        status,
        &headers,
        &bytes,
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
    );
    let document = problem(&bytes);
    assert!(
        document["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("..")),
        "the detail names the dot segment, got {document}"
    );
}

#[tokio::test]
async fn an_encoded_dot_segment_is_a_400_too() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let mock = upstream.mock(|_when, then| {
        then.status(200);
    });

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        vec![http_endpoint(&upstream)],
    );
    seed_route(&harness, TENANT, id, "/");

    // `%2e%2e` decodes to `..` *after* the router has matched, which is exactly
    // the encoding a traversal hides behind.
    let (status, headers, bytes) = proxy(
        &harness,
        "GET",
        "api.openai.com/v1/%2e%2e/private",
        &[],
        None,
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(mock.calls(), 0);
    assert_problem(
        status,
        &headers,
        &bytes,
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
    );
}

#[tokio::test]
async fn an_encoded_path_separator_in_the_suffix_is_a_400() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let mock = upstream.mock(|_when, then| {
        then.status(200);
    });

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        vec![http_endpoint(&upstream)],
    );
    seed_route(&harness, TENANT, id, "/");

    // An encoded separator turns one segment into two *after* the router has
    // matched, so the raw request line has to be inspected for it.
    for (raw, decoded) in [
        ("api.openai.com/v1/public%2fprivate", "/v1/public/private"),
        ("api.openai.com/v1/public%2Fprivate", "/v1/public/private"),
    ] {
        let (status, headers, bytes) = proxy(&harness, "GET", raw, &[], None).await;

        assert_eq!(status, StatusCode::BAD_REQUEST, "{raw}");
        assert_eq!(mock.calls(), 0, "{raw} is never forwarded");
        assert_problem(
            status,
            &headers,
            &bytes,
            "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
        );
        let document = problem(&bytes);
        assert_eq!(document["field"], json!("path"), "{raw}");
        assert_eq!(
            document["instance"],
            format!("{PROXY}/{raw}"),
            "the instance is the requested path"
        );
        assert_eq!(decoded, "/v1/public/private");
    }
}

#[tokio::test]
async fn a_suffix_with_an_empty_segment_is_a_400() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let mock = upstream.mock(|_when, then| {
        then.status(200);
    });

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        vec![http_endpoint(&upstream)],
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, headers, bytes) =
        proxy(&harness, "GET", "api.openai.com/v1//private", &[], None).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(mock.calls(), 0);
    assert_problem(
        status,
        &headers,
        &bytes,
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
    );
}

#[tokio::test]
async fn an_ordinary_suffix_is_still_forwarded() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let mock = upstream.mock(|when, then| {
        when.path("/v1/public/private");
        then.status(200);
    });

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        vec![http_endpoint(&upstream)],
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, _, _) = proxy(
        &harness,
        "GET",
        "api.openai.com/v1/public/private",
        &[],
        None,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    mock.assert_calls(1);
}

#[tokio::test]
async fn a_disabled_suffix_mode_still_rejects_an_encoded_separator() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let mock = upstream.mock(|_when, then| {
        then.status(200);
    });

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        vec![http_endpoint(&upstream)],
    );
    seed_route_with(
        &harness,
        TENANT,
        id,
        HttpMatch {
            methods: vec![RouteMethod::Get],
            path: "/v1/status".to_owned(),
            query_allowlist: Vec::new(),
            path_suffix_mode: PathSuffixMode::Disabled,
        },
    );

    // The suffix is rejected before any route matching happens, so a route that
    // accepts no suffix at all still answers 400 — never 404, never a dial.
    let (status, headers, bytes) =
        proxy(&harness, "GET", "api.openai.com/v1/status%2Fx", &[], None).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(mock.calls(), 0);
    assert_problem(
        status,
        &headers,
        &bytes,
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
    );
}

// ---------------------------------------------------------------------------
// Scheme gate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_plaintext_endpoint_is_blocked_when_allow_http_upstream_is_false() {
    let harness = harness_with(2, false, vec![]);
    let upstream = MockServer::start_async().await;
    let mock = upstream.mock(|_when, then| {
        then.status(200);
    });

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        vec![http_endpoint(&upstream)],
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, headers, bytes) =
        proxy(&harness, "GET", "api.openai.com/v1/models", &[], None).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(mock.calls(), 0, "the plaintext endpoint is not dialed");
    assert_problem(
        status,
        &headers,
        &bytes,
        "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1",
    );
    let document = problem(&bytes);
    assert_eq!(document["scheme"], json!("http"));
    assert_eq!(document["allow_http_upstream"], json!(false));
    assert!(
        document["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("allow_http_upstream")),
        "the detail names the configuration that blocked it, got {document}"
    );
}

#[tokio::test]
async fn a_plaintext_endpoint_is_forwarded_when_allow_http_upstream_is_true() {
    let harness = harness_with(2, true, vec![]);
    let upstream = MockServer::start_async().await;
    let mock = upstream.mock(|_when, then| {
        then.status(200);
    });

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        vec![http_endpoint(&upstream)],
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, _, _) = proxy(&harness, "GET", "api.openai.com/v1/models", &[], None).await;

    assert_eq!(status, StatusCode::OK);
    mock.assert_calls(1);
}

// ---------------------------------------------------------------------------
// Streaming body guards
// ---------------------------------------------------------------------------

/// A raw server that answers only once the request body has ended.
///
/// The guard aborts a body that exceeds the limit, and the error it captured is
/// only reported once the forward attempt has finished — so the upstream has to
/// *wait* for the body rather than answer early, or the response would race the
/// guard.
async fn sink_server() -> Endpoint {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port binds");
    let port = listener.local_addr().expect("the address is known").port();

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                continue;
            };
            let mut chunk = [0_u8; 8192];
            // Read until the client stops sending: the oversized stream aborts.
            while let Ok(read) = socket.read(&mut chunk).await {
                if read == 0 {
                    break;
                }
            }
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok")
                .await;
            let _ = socket.shutdown().await;
        }
    });

    Endpoint {
        scheme: Scheme::Http,
        host: "127.0.0.1".to_owned(),
        port,
    }
}

#[tokio::test]
async fn a_chunked_body_over_the_limit_is_a_413_even_while_streaming() {
    let harness = harness(vec![]);
    let endpoint = sink_server().await;

    let id = seed_upstream(&harness, TENANT, "api.openai.com", vec![endpoint]);
    post_route(&harness, TENANT, id);

    // No `Content-Length`: the body is streamed, so the guard is what counts it.
    let (status, headers, bytes) = send_streaming(
        &harness,
        "POST",
        &format!("{PROXY}/api.openai.com/v1"),
        &[],
        oversized_chunked_body(110),
    )
    .await;

    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_problem(
        status,
        &headers,
        &bytes,
        "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1",
    );
}

// ---------------------------------------------------------------------------
// IPv6 endpoints
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_ipv6_endpoint_is_dialed_with_a_bracketed_authority() {
    // An IPv6 echo server: `::1` on a random port, announced as an IPv6 host.
    let upstream = echo_server_on("[::1]", "[::1]:0").await;
    let harness = harness(vec![]);

    // The stored endpoint is normalized to the unbracketed literal; the
    // authority it dials and the `Host` it sends have to put the brackets back.
    let id = seed_upstream(
        &harness,
        TENANT,
        "ipv6.vendor.com",
        vec![Endpoint {
            scheme: Scheme::Http,
            host: "[::1]".to_owned(),
            port: upstream.endpoint.port,
        }],
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, headers, bytes) =
        proxy(&harness, "GET", "ipv6.vendor.com/v1/models", &[], None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(error_source(&headers).as_deref(), Some("upstream"));
    let received = problem(&bytes);
    assert_eq!(
        echoed(&received, "host"),
        Some(format!("[::1]:{}", upstream.endpoint.port).as_str()),
        "Host carries the bracketed IPv6 authority, got {received}"
    );
}

// ---------------------------------------------------------------------------
// Correlation id
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_gateway_error_carries_the_correlation_id_of_its_request() {
    let harness = harness(vec![]);
    let port = closed_port();

    let id = seed_upstream(
        &harness,
        TENANT,
        "unreachable.vendor.com",
        vec![Endpoint {
            scheme: Scheme::Http,
            host: "127.0.0.1".to_owned(),
            port,
        }],
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, headers, bytes) =
        proxy(&harness, "GET", "unreachable.vendor.com/v1", &[], None).await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(error_source(&headers).as_deref(), Some("gateway"));

    let document = problem(&bytes);
    let trace_id = document["trace_id"].as_str().expect("a trace_id member");
    Uuid::parse_str(trace_id).expect("the correlation id is a UUID");

    // A second request gets its own id, so two failures never share one.
    let (_, _, bytes) = proxy(&harness, "GET", "unreachable.vendor.com/v1", &[], None).await;
    assert_ne!(
        problem(&bytes)["trace_id"],
        document["trace_id"],
        "the correlation id is minted per request"
    );
}

// ---------------------------------------------------------------------------
// Request framing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn conflicting_content_lengths_are_a_400_and_are_never_forwarded() {
    let harness = harness(vec![]);
    let upstream = MockServer::start_async().await;
    let mock = upstream.mock(|_when, then| {
        then.status(200);
    });

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.openai.com",
        vec![http_endpoint(&upstream)],
    );
    post_route(&harness, TENANT, id);

    let mut request = Request::builder()
        .method("POST")
        .uri(format!("{PROXY}/api.openai.com/v1"))
        .header("content-length", "5")
        .header("content-length", "9")
        .extension(security_context(TENANT))
        .body(Body::from("payload"))
        .expect("the request builds");
    *request.version_mut() = axum::http::Version::HTTP_11;

    let response = harness
        .router
        .clone()
        .oneshot(request)
        .await
        .expect("the router responds");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("the body is readable")
        .to_bytes()
        .to_vec();

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(mock.calls(), 0, "an ambiguous body is not forwarded");
    assert_problem(
        status,
        &headers,
        &bytes,
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
    );
    assert_eq!(
        problem(&bytes)["field"],
        json!("Content-Length"),
        "the framing header is named as the problem"
    );
}
