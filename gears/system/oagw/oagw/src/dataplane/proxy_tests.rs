// Created: 2026-09-04 by Constructor Tech
//! Integration tests of the proxy engine.
//!
//! The upstream is an in-test HTTP/1.1 server bound to an ephemeral port and
//! dropped when the test ends; the data plane is exercised through the router
//! mounted by [`register_proxy_routes`].

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use futures_util::StreamExt;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tower::ServiceExt;
use uuid::Uuid;

use super::*;
use crate::config::SsrfPolicy;
use crate::controlplane::{service::ControlPlaneService, store::ControlPlaneStore};
use crate::dataplane::{RATE_LIMIT_LIMIT_HEADER, RATE_LIMIT_REMAINING_HEADER};
use crate::domain::plugin::{GUARD_REQUIRED_HEADERS, TRANSFORM_REQUEST_ID};
use crate::domain::{
    Alias, AllowedOrigin, BurstCapacity, CorsConfig, Endpoint, EndpointScheme, HeaderPassthrough,
    HeadersConfig, HttpMatch, HttpMethod, PathSuffixMode, PluginChain, PluginKind, PluginRef,
    Protocol, RateLimitAlgorithm, RateLimitConfig, RateLimitScope, RateLimitStrategy,
    RateLimitWindow, RequestHeaderRules, ResponseHeaderRules, Route, RouteMatch, RouteSpec,
    ServerConfig, SharingMode, SustainedRate, Upstream, UpstreamSpec,
};
use crate::error::{ERROR_SOURCE_GATEWAY, ERROR_SOURCE_HEADER, ERROR_SOURCE_UPSTREAM};

/// Fixed tenant of every request in this module.
fn tenant_id() -> Uuid {
    Uuid::from_u128(0x0A6D)
}

/// A `SecurityContext` bound to [`tenant_id`], as the auth middleware would
/// inject it.
fn security_context() -> toolkit_security::SecurityContext {
    toolkit_security::SecurityContext::builder()
        .subject_id(Uuid::from_u128(0x5EB))
        .subject_tenant_id(tenant_id())
        .build()
        .unwrap()
}

/// The gear configuration of the tests.
fn config(proxy_timeout_secs: u64, allow_http: bool) -> OagwConfig {
    OagwConfig {
        proxy_timeout_secs,
        allow_http_upstream: allow_http,
        ssrf_policy: SsrfPolicy::default(),
        max_body_bytes: 1024 * 1024,
    }
}

/// The router, the control plane and the data plane of one test.
struct Harness {
    router: Router,
    svc: Arc<ControlPlaneService>,
    plane: Arc<DataPlane>,
}

fn harness(config: OagwConfig) -> Harness {
    let svc = Arc::new(ControlPlaneService::new(Arc::new(ControlPlaneStore::new())));
    let plane = Arc::new(DataPlane::new(
        Arc::clone(&svc),
        Arc::new(config),
        never_cancelled(),
    ));
    let router = register_proxy_routes(Router::new(), Arc::clone(&plane));
    Harness { router, svc, plane }
}

/// An upstream of the tests, registered with an explicit alias.
fn registered_upstream(h: &Harness, alias: &str, endpoints: Vec<Endpoint>) -> Upstream {
    let spec = upstream_spec(alias, endpoints);
    h.svc.create_upstream(&spec).unwrap()
}

/// The spec of a plaintext upstream with an explicit alias.
fn upstream_spec(alias: &str, endpoints: Vec<Endpoint>) -> UpstreamSpec {
    UpstreamSpec {
        tenant_id: tenant_id(),
        alias: Some(Alias::parse(alias).unwrap()),
        protocol: Protocol::Http,
        enabled: true,
        server: ServerConfig::new(endpoints).unwrap(),
        auth: None,
        headers: None,
        plugins: None,
        rate_limit: None,
        cors: None,
        tags: Vec::new(),
    }
}

/// An `http` endpoint on `host` at `port`.
fn http_endpoint(host: &str, port: u16) -> Endpoint {
    Endpoint::new(EndpointScheme::Http, host, Some(port)).unwrap()
}

/// A route of `upstream` matching `path` for `GET` and `POST`.
fn registered_route(h: &Harness, upstream_id: Uuid, path: &str, allowlist: &[&str]) -> Route {
    let spec = RouteSpec {
        tenant_id: tenant_id(),
        upstream_id,
        r#match: RouteMatch::Http(
            HttpMatch::new(
                vec![
                    HttpMethod::parse("GET").unwrap(),
                    HttpMethod::parse("POST").unwrap(),
                ],
                path.to_owned(),
                allowlist.iter().map(|name| (*name).to_owned()).collect(),
                PathSuffixMode::Append,
            )
            .unwrap(),
        ),
        plugins: None,
        rate_limit: None,
        cors: None,
        enabled: true,
        tags: Vec::new(),
    };
    h.svc.create_route(&spec).unwrap()
}

/// Replaces `upstream`, patching the slots of the fixture.
fn replace_upstream(h: &Harness, upstream: &Upstream, patch: Patch) -> Upstream {
    let mut spec = upstream_spec(
        upstream.alias.as_str(),
        upstream.server.endpoints().to_vec(),
    );
    if let Some(cors) = patch.cors {
        spec.cors = Some(cors);
    }
    if let Some(rate_limit) = patch.rate_limit {
        spec.rate_limit = Some(rate_limit);
    }
    if let Some(plugins) = patch.plugins {
        spec.plugins = Some(plugins);
    }
    if let Some(headers) = patch.headers {
        spec.headers = Some(headers);
    }
    if let Some(auth) = patch.auth {
        spec.auth = Some(auth);
    }
    h.svc
        .replace_upstream(upstream.tenant_id, upstream.id, &spec)
        .unwrap()
}

/// The optional slots of an upstream patch.
#[derive(Default)]
struct Patch {
    cors: Option<CorsConfig>,
    rate_limit: Option<RateLimitConfig>,
    plugins: Option<PluginChain>,
    headers: Option<HeadersConfig>,
    auth: Option<crate::domain::AuthConfig>,
}

/// Sends a proxied request through the mounted router.
async fn call(
    router: &Router,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    body: &'static [u8],
) -> axum::response::Response {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let mut request = builder.body(Body::from(body)).unwrap();
    request.extensions_mut().insert(security_context());
    router.clone().oneshot(request).await.unwrap()
}

/// The whole body of a response.
async fn body_of(response: &mut axum::response::Response) -> String {
    let bytes = to_bytes(
        std::mem::replace(response.body_mut(), Body::empty()),
        1 << 20,
    )
    .await
    .unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The problem body of a response, as JSON.
async fn problem_of(response: &mut axum::response::Response) -> Value {
    serde_json::from_str(&body_of(response).await).unwrap()
}

/// Asserts `response` is a gateway problem with `status`.
fn assert_gateway_problem(response: &axum::response::Response, status: u16) {
    assert_eq!(response.status().as_u16(), status);
    assert_eq!(
        response.headers().get(ERROR_SOURCE_HEADER).unwrap(),
        ERROR_SOURCE_GATEWAY
    );
    assert_eq!(
        response.headers().get(http::header::CONTENT_TYPE).unwrap(),
        "application/problem+json"
    );
}

/// A captured upstream request.
#[derive(Debug, Clone)]
struct Captured {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Captured {
    /// Value of a header (lowercase name), `None` when absent.
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// Behaviour of the mock upstream.
#[derive(Clone, Copy)]
enum Behaviour {
    /// A response with a status, a body and extra headers.
    Respond(u16, &'static str, &'static [(&'static str, &'static str)]),
    /// Accept the connection and never answer.
    Stall,
    /// An event stream sent in two chunks separated by a pause in millis.
    TwoChunks(u64),
}

/// A throwaway HTTP/1.1 upstream bound to an ephemeral port.
struct MockUpstream {
    addr: std::net::SocketAddr,
    requests: Arc<Mutex<Vec<Captured>>>,
    worker: Option<JoinHandle<()>>,
}

impl Drop for MockUpstream {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.abort();
        }
    }
}

impl MockUpstream {
    /// Starts a mock upstream serving `behaviour` to every connection.
    async fn start(behaviour: Behaviour) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let worker = tokio::spawn(serve(listener, behaviour, Arc::clone(&requests)));
        Self {
            addr,
            requests,
            worker: Some(worker),
        }
    }

    /// The requests the mock received so far.
    fn captured(&self) -> Vec<Captured> {
        self.requests.lock().unwrap().clone()
    }
}

/// Accepts connections until the listener is dropped.
async fn serve(listener: TcpListener, behaviour: Behaviour, requests: Arc<Mutex<Vec<Captured>>>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let requests = Arc::clone(&requests);
        tokio::spawn(handle(stream, behaviour, requests));
    }
}

/// Serves one connection of the mock upstream.
async fn handle(mut stream: TcpStream, behaviour: Behaviour, requests: Arc<Mutex<Vec<Captured>>>) {
    let Some(captured) = read_request(&mut stream).await else {
        return;
    };
    requests.lock().unwrap().push(captured);
    let _ = write_response(&mut stream, behaviour).await;
    let _ = stream.shutdown().await;
}

/// Reads the request head and body of the proxied request.
async fn read_request(stream: &mut TcpStream) -> Option<Captured> {
    let mut raw: Vec<u8> = Vec::new();
    let mut chunk = [0_u8; 2048];
    loop {
        if let Some(captured) = parse_request(&raw) {
            return Some(captured);
        }
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return parse_request(&raw);
        }
        raw.extend_from_slice(&chunk[..read]);
    }
}

/// Parses a complete request out of `raw`, or `None` while it is incomplete.
fn parse_request(raw: &[u8]) -> Option<Captured> {
    let head_end = raw.windows(4).position(|window| window == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&raw[..head_end]).ok()?;
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_owned();
    let target = parts.next()?.to_owned();
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
    }
    let length: usize = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(0);
    let body_start = head_end + 4;
    if raw.len() < body_start + length {
        return None;
    }
    let body = String::from_utf8_lossy(&raw[body_start..body_start + length]).into_owned();
    Some(Captured {
        method,
        target,
        headers,
        body,
    })
}

/// Writes the response of the mock upstream.
async fn write_response(stream: &mut TcpStream, behaviour: Behaviour) -> std::io::Result<()> {
    match behaviour {
        Behaviour::Respond(status, body, extra) => {
            let head = format!(
                "HTTP/1.1 {status} {reason}\r\ncontent-length: {length}\r\n{headers}\r\n",
                reason = reason_of(status),
                length = body.len(),
                headers = extra
                    .iter()
                    .map(|(name, value)| format!("{name}: {value}\r\n"))
                    .collect::<String>(),
            );
            stream.write_all(head.as_bytes()).await?;
            stream.write_all(body.as_bytes()).await?;
            stream.flush().await
        }
        Behaviour::Stall => {
            // Never answer: the caller has to time out.
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(())
        }
        Behaviour::TwoChunks(millis) => {
            let head = String::from("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n");
            stream.write_all(head.as_bytes()).await?;
            stream.write_all(b"data: one\n\n").await?;
            stream.flush().await?;
            tokio::time::sleep(Duration::from_millis(millis)).await;
            stream.write_all(b"data: two\n\n").await?;
            stream.flush().await
        }
    }
}

/// Reason phrase of the statuses the mock answers with.
fn reason_of(status: u16) -> &'static str {
    match status {
        200 => "OK",
        500 => "Internal Server Error",
        _ => "Response",
    }
}

// ------------------------------------------------------------------ fixtures

/// A working upstream on a live mock, with a `GET /v1` route.
async fn happy_setup() -> (Harness, MockUpstream, String) {
    let mock = MockUpstream::start(Behaviour::Respond(200, "pong", &[])).await;
    let harness = harness(config(5, true));
    let upstream = registered_upstream(
        &harness,
        "mock.internal",
        vec![http_endpoint("127.0.0.1", mock.addr.port())],
    );
    registered_route(&harness, upstream.id, "/v1", &["api-version"]);
    let uri = String::from("/oagw/v1/proxy/mock.internal/v1/ping?api-version=1.0");
    (harness, mock, uri)
}

// --------------------------------------------------------------------- tests

#[tokio::test]
async fn forwards_the_request_and_passes_the_response_through() {
    let (harness, mock, uri) = happy_setup().await;
    let mut response = call(
        &harness.router,
        "GET",
        &uri,
        &[("x-tenant", "t1"), ("api-version", "1.0")],
        b"",
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_of(&mut response).await, "pong");
    assert_eq!(
        response.headers().get(ERROR_SOURCE_HEADER).unwrap(),
        ERROR_SOURCE_UPSTREAM
    );

    let captured = mock.captured();
    assert_eq!(captured.len(), 1);
    let request = &captured[0];
    assert_eq!(request.method, "GET");
    assert_eq!(request.target, "/v1/ping?api-version=1.0");
    assert_eq!(request.body, "");
    assert_eq!(
        request.header("host"),
        Some(format!("127.0.0.1:{}", mock.addr.port()).as_str())
    );
    // No header rule is configured, so no inbound header is forwarded.
    assert_eq!(request.header("x-tenant"), None);
}

#[tokio::test]
async fn an_unknown_alias_is_a_gateway_404_problem() {
    let (harness, _mock, _uri) = happy_setup().await;
    let mut response = call(
        &harness.router,
        "GET",
        &format!("{PROXY_ALIAS_PREFIX}ghost.internal/v1/ping"),
        &[],
        b"",
    )
    .await;

    assert_gateway_problem(&response, 404);
    let problem = problem_of(&mut response).await;
    assert_eq!(problem["status"], 404);
    assert!(
        problem["type"]
            .as_str()
            .unwrap()
            .contains("cf.oagw.route.not_found.v1")
    );
    assert_eq!(problem["error_code"].as_str(), Some("RouteNotFound"));
    assert_eq!(problem["error_domain"].as_str(), Some("oagw.v1"));
    assert!(!problem["title"].as_str().unwrap().is_empty());
    assert_eq!(problem["context"]["error_source"], "gateway");
}

#[tokio::test]
async fn an_unmatched_route_is_a_gateway_404_problem() {
    let mock = MockUpstream::start(Behaviour::Respond(200, "pong", &[])).await;
    let harness = harness(config(5, true));
    let upstream = registered_upstream(
        &harness,
        "mock.internal",
        vec![http_endpoint("127.0.0.1", mock.addr.port())],
    );
    registered_route(&harness, upstream.id, "/v1", &[]);

    // The upstream matches, but no route accepts DELETE.
    let mut response = call(
        &harness.router,
        "DELETE",
        &format!("{PROXY_ALIAS_PREFIX}mock.internal/v1/ping"),
        &[],
        b"",
    )
    .await;

    assert_gateway_problem(&response, 404);
    let problem = problem_of(&mut response).await;
    assert_eq!(problem["status"], 404);
    assert!(mock.captured().is_empty(), "the upstream is never reached");
}

#[tokio::test]
async fn hop_by_hop_headers_are_stripped_in_both_directions() {
    let mock = MockUpstream::start(Behaviour::Respond(
        200,
        "pong",
        &[("connection", "close"), ("x-app", "kept")],
    ))
    .await;
    let harness = harness(config(5, true));
    let upstream = registered_upstream(
        &harness,
        "mock.internal",
        vec![http_endpoint("127.0.0.1", mock.addr.port())],
    );
    registered_route(&harness, upstream.id, "/v1", &[]);

    let response = call(
        &harness.router,
        "GET",
        &format!("{PROXY_ALIAS_PREFIX}mock.internal/v1/ping"),
        &[
            ("connection", "x-doomed, te"),
            ("x-doomed", "dropped"),
            ("te", "trailers"),
            ("transfer-encoding", "chunked"),
        ],
        b"",
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response.headers().contains_key("connection"));
    assert!(!response.headers().contains_key("te"));
    assert!(!response.headers().contains_key("transfer-encoding"));
    assert_eq!(response.headers().get("x-app").unwrap(), "kept");

    let captured = mock.captured();
    assert_eq!(captured.len(), 1);
    let request = &captured[0];
    assert!(request.header("connection").is_none());
    assert!(request.header("x-doomed").is_none());
    assert!(request.header("te").is_none());
    assert!(request.header("host").is_some());
    assert!(request.header("x-oagw-target-host").is_none());
}

#[tokio::test]
async fn upstream_header_rules_apply_in_both_directions() {
    let mock = MockUpstream::start(Behaviour::Respond(200, "pong", &[])).await;
    let harness = harness(config(5, true));
    let upstream = registered_upstream(
        &harness,
        "mock.internal",
        vec![http_endpoint("127.0.0.1", mock.addr.port())],
    );
    let mut request_rules = RequestHeaderRules {
        set: BTreeMap::new(),
        add: BTreeMap::new(),
        remove: Vec::new(),
        passthrough: HeaderPassthrough::Allowlist,
        passthrough_allowlist: vec![String::from("x-tenant")],
    };
    request_rules
        .set
        .insert(String::from("x-set"), String::from("value"));
    replace_upstream(
        &harness,
        &upstream,
        Patch {
            headers: Some(HeadersConfig {
                request: Some(request_rules),
                response: Some(ResponseHeaderRules {
                    set: BTreeMap::from([(String::from("x-frame-options"), String::from("DENY"))]),
                    add: BTreeMap::new(),
                    remove: vec![String::from("x-server")],
                }),
            }),
            ..Patch::default()
        },
    );
    registered_route(&harness, upstream.id, "/v1", &[]);

    let response = call(
        &harness.router,
        "GET",
        &format!("{PROXY_ALIAS_PREFIX}mock.internal/v1/ping"),
        &[("x-tenant", "t1"), ("x-secret", "nope")],
        b"",
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers().get("x-frame-options").unwrap(), "DENY");

    let captured = mock.captured();
    assert_eq!(captured[0].header("x-tenant"), Some("t1"));
    assert_eq!(captured[0].header("x-set"), Some("value"));
    assert_eq!(captured[0].header("x-secret"), None);
}

#[tokio::test]
async fn target_host_header_selects_the_endpoint() {
    let mock = MockUpstream::start(Behaviour::Respond(200, "pinned", &[])).await;
    let harness = harness(config(5, true));
    let upstream = registered_upstream(
        &harness,
        "pool.internal",
        vec![
            http_endpoint("127.0.0.1", mock.addr.port()),
            http_endpoint("api.internal", mock.addr.port()),
        ],
    );
    registered_route(&harness, upstream.id, "/v1", &[]);

    let mut response = call(
        &harness.router,
        "GET",
        &format!("{PROXY_ALIAS_PREFIX}pool.internal/v1/ping"),
        &[("x-oagw-target-host", "127.0.0.1")],
        b"",
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_of(&mut response).await, "pinned");

    let captured = mock.captured();
    assert_eq!(captured.len(), 1, "the pinned endpoint was dialed");
    assert_eq!(
        captured[0].header("host"),
        Some(format!("127.0.0.1:{}", mock.addr.port()).as_str())
    );
    assert!(captured[0].header("x-oagw-target-host").is_none());
}

#[tokio::test]
async fn a_foreign_target_host_is_rejected_without_dialing() {
    let mock = MockUpstream::start(Behaviour::Respond(200, "pinned", &[])).await;
    let harness = harness(config(5, true));
    let upstream = registered_upstream(
        &harness,
        "pool.internal",
        vec![
            http_endpoint("127.0.0.1", mock.addr.port()),
            http_endpoint("api.internal", mock.addr.port()),
        ],
    );
    registered_route(&harness, upstream.id, "/v1", &[]);

    let mut response = call(
        &harness.router,
        "GET",
        &format!("{PROXY_ALIAS_PREFIX}pool.internal/v1/ping"),
        &[("x-oagw-target-host", "evil.internal")],
        b"",
    )
    .await;

    assert_gateway_problem(&response, 400);
    let problem = problem_of(&mut response).await;
    assert!(
        problem["type"]
            .as_str()
            .unwrap()
            .contains("unknown_target_host")
    );
    assert!(mock.captured().is_empty(), "no endpoint is dialed");
}

#[tokio::test]
async fn a_malformed_target_host_is_rejected_without_dialing() {
    let mock = MockUpstream::start(Behaviour::Respond(200, "pinned", &[])).await;
    let harness = harness(config(5, true));
    let upstream = registered_upstream(
        &harness,
        "pool.internal",
        vec![
            http_endpoint("127.0.0.1", mock.addr.port()),
            http_endpoint("api.internal", mock.addr.port()),
        ],
    );
    registered_route(&harness, upstream.id, "/v1", &[]);

    let mut response = call(
        &harness.router,
        "GET",
        &format!("{PROXY_ALIAS_PREFIX}pool.internal/v1/ping"),
        &[("x-oagw-target-host", "https://api.internal/x")],
        b"",
    )
    .await;

    assert_gateway_problem(&response, 400);
    let problem = problem_of(&mut response).await;
    assert!(
        problem["type"]
            .as_str()
            .unwrap()
            .contains("invalid_target_host")
    );
    assert!(mock.captured().is_empty());
}

#[tokio::test]
async fn a_pool_with_a_common_suffix_alias_requires_the_target_host() {
    // A pool whose hosts share a registrable common suffix: the alias names
    // the pool, so the caller has to pin the endpoint
    // (`docs/ADR/0001-request-routing.md` "X-OAGW-Target-Host Behavior
    // Matrix").
    let harness = harness(config(5, true));
    let upstream = registered_upstream(
        &harness,
        "vendor.com",
        vec![
            http_endpoint("us.vendor.com", 80),
            http_endpoint("eu.vendor.com", 80),
        ],
    );
    registered_route(&harness, upstream.id, "/v1", &[]);

    let mut response = call(
        &harness.router,
        "GET",
        &format!("{PROXY_ALIAS_PREFIX}vendor.com/v1/ping"),
        &[],
        b"",
    )
    .await;

    assert_gateway_problem(&response, 400);
    let problem = problem_of(&mut response).await;
    assert!(
        problem["type"]
            .as_str()
            .unwrap()
            .contains("missing_target_host")
    );

    // Pinning one endpoint of the pool routes the request.
    let pinned = call(
        &harness.router,
        "GET",
        &format!("{PROXY_ALIAS_PREFIX}vendor.com/v1/ping"),
        &[("x-oagw-target-host", "eu.vendor.com")],
        b"",
    )
    .await;
    assert_eq!(
        pinned.status(),
        StatusCode::BAD_GATEWAY,
        "the pinned host is accepted and then dialed"
    );
}

#[tokio::test]
async fn a_single_endpoint_is_selected_without_the_target_host() {
    let (harness, mock, uri) = happy_setup().await;
    let response = call(&harness.router, "GET", &uri, &[], b"").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(mock.captured().len(), 1);
}

#[tokio::test]
async fn a_disabled_plaintext_egress_is_rejected_without_dialing() {
    let mock = MockUpstream::start(Behaviour::Respond(200, "pong", &[])).await;
    let harness = harness(config(5, false));
    let upstream = registered_upstream(
        &harness,
        "mock.internal",
        vec![http_endpoint("127.0.0.1", mock.addr.port())],
    );
    registered_route(&harness, upstream.id, "/v1", &[]);

    let mut response = call(
        &harness.router,
        "GET",
        &format!("{PROXY_ALIAS_PREFIX}mock.internal/v1/ping"),
        &[],
        b"",
    )
    .await;

    assert_gateway_problem(&response, 503);
    let problem = problem_of(&mut response).await;
    assert_eq!(problem["status"], 503);
    assert!(mock.captured().is_empty(), "no connection attempt is made");
}

#[tokio::test]
async fn an_unreachable_upstream_is_a_gateway_502() {
    // A port nobody listens on: the connection is refused.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let harness = harness(config(2, true));
    let upstream = registered_upstream(
        &harness,
        "mock.internal",
        vec![http_endpoint("127.0.0.1", port)],
    );
    registered_route(&harness, upstream.id, "/v1", &[]);

    let mut response = call(
        &harness.router,
        "GET",
        &format!("{PROXY_ALIAS_PREFIX}mock.internal/v1/ping"),
        &[],
        b"",
    )
    .await;

    assert_gateway_problem(&response, 502);
    let problem = problem_of(&mut response).await;
    assert_eq!(problem["status"], 502);
}

#[tokio::test]
async fn a_stalled_upstream_is_a_gateway_504() {
    let mock = MockUpstream::start(Behaviour::Stall).await;
    let harness = harness(config(1, true));
    let upstream = registered_upstream(
        &harness,
        "mock.internal",
        vec![http_endpoint("127.0.0.1", mock.addr.port())],
    );
    registered_route(&harness, upstream.id, "/v1", &[]);

    let started = Instant::now();
    let mut response = call(
        &harness.router,
        "GET",
        &format!("{PROXY_ALIAS_PREFIX}mock.internal/v1/ping"),
        &[],
        b"",
    )
    .await;

    assert_gateway_problem(&response, 504);
    let problem = problem_of(&mut response).await;
    assert_eq!(problem["status"], 504);
    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "the proxy timeout was enforced"
    );
}

#[tokio::test]
async fn rate_limiting_rejects_with_headers_then_recovers() {
    let mock = MockUpstream::start(Behaviour::Respond(200, "pong", &[])).await;
    let harness = harness(config(5, true));
    let upstream = registered_upstream(
        &harness,
        "mock.internal",
        vec![http_endpoint("127.0.0.1", mock.addr.port())],
    );
    registered_route(&harness, upstream.id, "/v1", &[]);
    replace_upstream(
        &harness,
        &upstream,
        Patch {
            rate_limit: Some(RateLimitConfig {
                sharing: SharingMode::Inherit,
                algorithm: RateLimitAlgorithm::TokenBucket,
                sustained: SustainedRate {
                    rate: NonZeroU32::new(2).unwrap(),
                    window: RateLimitWindow::Second,
                },
                burst: Some(BurstCapacity {
                    capacity: NonZeroU32::new(2).unwrap(),
                }),
                scope: RateLimitScope::Ip,
                strategy: RateLimitStrategy::Reject,
                cost: NonZeroU32::new(1).unwrap(),
            }),
            ..Patch::default()
        },
    );
    let uri = format!("{PROXY_ALIAS_PREFIX}mock.internal/v1/ping");

    for _ in 0..2 {
        let response = call(&harness.router, "GET", &uri, &[], b"").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(RATE_LIMIT_LIMIT_HEADER).unwrap(),
            "2"
        );
    }

    let mut limited = call(&harness.router, "GET", &uri, &[], b"").await;
    assert_gateway_problem(&limited, 429);
    assert!(limited.headers().contains_key(http::header::RETRY_AFTER));
    assert_eq!(
        limited.headers().get(RATE_LIMIT_REMAINING_HEADER).unwrap(),
        "0"
    );
    assert!(
        limited
            .headers()
            .contains_key(ratelimit::LEGACY_LIMIT_HEADER)
    );
    let problem = problem_of(&mut limited).await;
    assert_eq!(problem["status"], 429);

    // The bucket refills from the sustained rate.
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    let recovered = call(&harness.router, "GET", &uri, &[], b"").await;
    assert_eq!(recovered.status(), StatusCode::OK);
    assert_eq!(mock.captured().len(), 3);
}

#[tokio::test]
async fn the_required_headers_guard_rejects_an_incomplete_request() {
    let mock = MockUpstream::start(Behaviour::Respond(200, "pong", &[])).await;
    let harness = harness(config(5, true));
    let upstream = registered_upstream(
        &harness,
        "mock.internal",
        vec![http_endpoint("127.0.0.1", mock.addr.port())],
    );
    registered_route(&harness, upstream.id, "/v1", &[]);
    replace_upstream(
        &harness,
        &upstream,
        Patch {
            plugins: Some(PluginChain {
                sharing: SharingMode::Inherit,
                items: vec![PluginRef::parse(PluginKind::Guard, GUARD_REQUIRED_HEADERS).unwrap()],
            }),
            // The plugin configuration slot of a proxied request is the auth
            // configuration of the upstream (see `plugin_config`).
            auth: Some(crate::domain::AuthConfig {
                sharing: SharingMode::Inherit,
                plugin: None,
                config: serde_json::json!({"required_request_headers": "x-tenant-id"}),
            }),
            ..Patch::default()
        },
    );
    let uri = format!("{PROXY_ALIAS_PREFIX}mock.internal/v1/ping");

    let mut missing = call(&harness.router, "GET", &uri, &[], b"").await;
    assert_gateway_problem(&missing, 400);
    let problem = problem_of(&mut missing).await;
    assert_eq!(problem["status"], 400);
    assert!(problem["detail"].as_str().unwrap().contains("x-tenant-id"));
    assert!(mock.captured().is_empty());

    let complete = call(
        &harness.router,
        "GET",
        &uri,
        &[("x-tenant-id", "tenant-a")],
        b"",
    )
    .await;
    assert_eq!(complete.status(), StatusCode::OK);
    assert_eq!(mock.captured().len(), 1);
}

#[tokio::test]
async fn the_request_id_transform_is_propagated_to_the_upstream_and_back() {
    let mock = MockUpstream::start(Behaviour::Respond(200, "pong", &[])).await;
    let harness = harness(config(5, true));
    let upstream = registered_upstream(
        &harness,
        "mock.internal",
        vec![http_endpoint("127.0.0.1", mock.addr.port())],
    );
    registered_route(&harness, upstream.id, "/v1", &[]);
    replace_upstream(
        &harness,
        &upstream,
        Patch {
            plugins: Some(PluginChain {
                sharing: SharingMode::Inherit,
                items: vec![PluginRef::parse(PluginKind::Transform, TRANSFORM_REQUEST_ID).unwrap()],
            }),
            ..Patch::default()
        },
    );
    let uri = format!("{PROXY_ALIAS_PREFIX}mock.internal/v1/ping");

    let generated = call(&harness.router, "GET", &uri, &[], b"").await;
    assert_eq!(generated.status(), StatusCode::OK);
    let request_id = generated
        .headers()
        .get("x-request-id")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        mock.captured()[0].header("x-request-id"),
        Some(request_id.as_str())
    );

    let echoed = call(
        &harness.router,
        "GET",
        &uri,
        &[("x-request-id", "trace-42")],
        b"",
    )
    .await;
    assert_eq!(echoed.headers().get("x-request-id").unwrap(), "trace-42");
    assert_eq!(mock.captured()[1].header("x-request-id"), Some("trace-42"));
}

#[tokio::test]
async fn an_upstream_failure_is_passed_through() {
    let mock = MockUpstream::start(Behaviour::Respond(
        500,
        "boom",
        &[("content-type", "application/json")],
    ))
    .await;
    let harness = harness(config(5, true));
    let upstream = registered_upstream(
        &harness,
        "mock.internal",
        vec![http_endpoint("127.0.0.1", mock.addr.port())],
    );
    registered_route(&harness, upstream.id, "/v1", &[]);

    let mut response = call(
        &harness.router,
        "GET",
        &format!("{PROXY_ALIAS_PREFIX}mock.internal/v1/ping"),
        &[],
        b"",
    )
    .await;

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        response.headers().get(ERROR_SOURCE_HEADER).unwrap(),
        ERROR_SOURCE_UPSTREAM
    );
    assert_eq!(body_of(&mut response).await, "boom");
}

#[tokio::test]
async fn a_preflight_is_answered_locally() {
    let mock = MockUpstream::start(Behaviour::Respond(200, "pong", &[])).await;
    let harness = harness(config(5, true));
    let upstream = registered_upstream(
        &harness,
        "mock.internal",
        vec![http_endpoint("127.0.0.1", mock.addr.port())],
    );
    registered_route(&harness, upstream.id, "/v1", &[]);
    replace_upstream(
        &harness,
        &upstream,
        Patch {
            cors: Some(cors_config()),
            ..Patch::default()
        },
    );

    let preflight = call(
        &harness.router,
        "OPTIONS",
        &format!("{PROXY_ALIAS_PREFIX}mock.internal/v1/ping"),
        &[
            ("origin", "https://app.example"),
            ("access-control-request-method", "POST"),
            ("access-control-request-headers", "content-type"),
        ],
        b"",
    )
    .await;

    assert_eq!(preflight.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        preflight
            .headers()
            .get("access-control-allow-origin")
            .unwrap(),
        "https://app.example"
    );
    assert_eq!(
        preflight
            .headers()
            .get("access-control-allow-methods")
            .unwrap(),
        "POST"
    );
    assert_eq!(
        preflight.headers().get("access-control-max-age").unwrap(),
        "86400"
    );
    assert!(preflight.headers().contains_key("vary"));
    assert!(
        mock.captured().is_empty(),
        "a preflight never reaches an upstream"
    );
}

#[tokio::test]
async fn a_cross_origin_request_outside_the_allowlist_is_rejected() {
    let mock = MockUpstream::start(Behaviour::Respond(200, "pong", &[])).await;
    let harness = harness(config(5, true));
    let upstream = registered_upstream(
        &harness,
        "mock.internal",
        vec![http_endpoint("127.0.0.1", mock.addr.port())],
    );
    registered_route(&harness, upstream.id, "/v1", &[]);
    replace_upstream(
        &harness,
        &upstream,
        Patch {
            cors: Some(cors_config()),
            ..Patch::default()
        },
    );
    let uri = format!("{PROXY_ALIAS_PREFIX}mock.internal/v1/ping");

    let mut rejected = call(
        &harness.router,
        "GET",
        &uri,
        &[("origin", "https://evil.example")],
        b"",
    )
    .await;
    assert_gateway_problem(&rejected, 403);
    let problem = problem_of(&mut rejected).await;
    assert_eq!(problem["status"], 403);
    assert!(mock.captured().is_empty());

    let allowed = call(
        &harness.router,
        "GET",
        &uri,
        &[("origin", "https://app.example")],
        b"",
    )
    .await;
    assert_eq!(allowed.status(), StatusCode::OK);
    assert_eq!(
        allowed
            .headers()
            .get("access-control-allow-origin")
            .unwrap(),
        "https://app.example"
    );
}

#[tokio::test]
async fn a_body_above_the_limit_is_rejected_before_buffering() {
    let mock = MockUpstream::start(Behaviour::Respond(200, "pong", &[])).await;
    let harness = harness(OagwConfig {
        proxy_timeout_secs: 5,
        allow_http_upstream: true,
        ssrf_policy: SsrfPolicy::default(),
        max_body_bytes: 8,
    });
    let upstream = registered_upstream(
        &harness,
        "mock.internal",
        vec![http_endpoint("127.0.0.1", mock.addr.port())],
    );
    registered_route(&harness, upstream.id, "/v1", &[]);

    let mut response = call(
        &harness.router,
        "POST",
        &format!("{PROXY_ALIAS_PREFIX}mock.internal/v1/ping"),
        &[],
        b"0123456789",
    )
    .await;

    assert_gateway_problem(&response, 413);
    let problem = problem_of(&mut response).await;
    assert_eq!(problem["status"], 413);
    assert!(mock.captured().is_empty());
}

#[tokio::test]
async fn a_streaming_response_is_not_broken_by_a_timeout() {
    // The upstream pauses longer than the proxy timeout between the two
    // chunks: a total timeout would truncate the stream.
    let mock = MockUpstream::start(Behaviour::TwoChunks(1_400)).await;
    let harness = harness(config(1, true));
    let upstream = registered_upstream(
        &harness,
        "mock.internal",
        vec![http_endpoint("127.0.0.1", mock.addr.port())],
    );
    registered_route(&harness, upstream.id, "/v1", &[]);

    let mut response = call(
        &harness.router,
        "GET",
        &format!("{PROXY_ALIAS_PREFIX}mock.internal/v1/ping"),
        &[],
        b"",
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/event-stream"
    );

    let started = Instant::now();
    let mut stream = std::mem::replace(response.body_mut(), Body::empty()).into_data_stream();
    let first = stream.next().await.unwrap().unwrap();
    let first_at = started.elapsed();
    assert!(
        first_at < Duration::from_millis(900),
        "the first chunk is forwarded as it arrives: {first_at:?}"
    );
    let second = stream.next().await.unwrap().unwrap();
    let total = started.elapsed();
    assert_eq!(std::str::from_utf8(&first).unwrap(), "data: one\n\n");
    assert_eq!(std::str::from_utf8(&second).unwrap(), "data: two\n\n");
    assert!(
        total >= Duration::from_millis(1_300),
        "the stream outlives the proxy timeout: {total:?} (first chunk at {first_at:?})"
    );
}

#[tokio::test]
async fn the_proxy_surface_is_mounted_gear_relative() {
    assert_eq!(PROXY_ROUTE_PATH, "/oagw/v1/proxy/{*alias}");
    assert_eq!(PROXY_ALIAS_PREFIX, "/oagw/v1/proxy/");
    assert_eq!(
        split_proxy_path("/oagw/v1/proxy/api.openai.com/v1/chat"),
        Some((String::from("api.openai.com"), String::from("/v1/chat")))
    );
    assert_eq!(
        split_proxy_path("/oagw/v1/proxy/api.openai.com"),
        Some((String::from("api.openai.com"), String::from("/")))
    );
    assert_eq!(split_proxy_path("/api/v1/proxy/api.openai.com"), None);
}

#[tokio::test]
async fn the_data_plane_exposes_its_seams() {
    let (harness, _mock, _uri) = happy_setup().await;
    assert!(!harness.plane.is_cancelled());
    assert!(harness.plane.config().allow_http_upstream);
    assert!(
        harness
            .plane
            .registry()
            .auth(&PluginRef::parse(PluginKind::Auth, crate::domain::plugin::AUTH_APIKEY).unwrap())
            .is_some()
    );
    assert!(std::ptr::eq(
        Arc::as_ptr(harness.plane.control_plane()),
        Arc::as_ptr(&harness.svc)
    ));
}

/// The CORS configuration of the tests.
fn cors_config() -> CorsConfig {
    CorsConfig {
        sharing: SharingMode::Inherit,
        enabled: true,
        allowed_origins: vec![AllowedOrigin::Exact(String::from("https://app.example"))],
        allowed_methods: vec![
            HttpMethod::parse("GET").unwrap(),
            HttpMethod::parse("POST").unwrap(),
        ],
        expose_headers: Vec::new(),
        allow_credentials: false,
    }
}
