#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Integration tests for the proxy transport and the pipeline behind it
//! (entry 2.4).
//!
//! In-crate integration tests only (DECOMPOSITION assumption 5): no e2e suite
//! is added under `testing/e2e/gears/oagw/`. The tests drive the mounted router
//! the way the host api-gateway does — the Bearer token is already resolved, so
//! the request carries the resolved
//! [`SecurityContext`](toolkit_security::SecurityContext) — and dial a stub
//! upstream listener the harness starts on the loopback interface, with
//! `allow_http_upstream: true` in the test configuration so the plaintext
//! endpoint is admitted. They assert the passthrough contract, the
//! `X-OAGW-Error-Source` classification on a success, on a gateway error and on
//! an upstream error, the preflight `204`, the streamed relay, the `403` CORS
//! rejections, the target-host errors and the no-retry and circuit-breaker
//! behaviour of `cpt-cf-oagw-dod-proxy-test-coverage`.

// @cpt-begin:cpt-cf-oagw-dod-proxy-test-coverage:p2:inst-full
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use futures_util::StreamExt;
use http_body_util::BodyExt;
use httpmock::MockServer;
use serde_json::{Value, json};
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

use oagw::api::rest::error::{ERROR_SOURCE_GATEWAY, ERROR_SOURCE_HEADER, ERROR_SOURCE_UPSTREAM};
use oagw::api::rest::routes::{MOUNT_ROOT, register_routes_with_config};
use oagw::config::OagwConfig;
use oagw::domain::model::PROTOCOL_HTTP;
use oagw::domain::sharing::{FlatHierarchy, StaticHierarchy, TenantHierarchy};
use oagw::infra::proxy::endpoint::TARGET_HOST_HEADER;
use oagw::infra::storage::OagwStore;

const TENANT: Uuid = uuid::uuid!("00000000-0000-0000-0000-000000000050");
const ANCESTOR: Uuid = uuid::uuid!("00000000-0000-0000-0000-000000000002");

/// The alias the loopback stub upstream is stored under.
const ALIAS: &str = "stub.internal";

/// The path the test routes match.
const ROUTE_PATH: &str = "/v1";

/// An address nothing in the test environment listens on, for the calls that
/// must fail.
const DEAD_PORT: u16 = 1;

/// Every scope a management caller may need.
const ALL: &[&str] = &["*"];

/// Host OpenAPI registry double that records nothing.
#[derive(Default)]
struct NoopRegistry;

impl toolkit::api::OpenApiRegistry for NoopRegistry {
    fn register_operation(&self, _spec: &toolkit::api::operation_builder::OperationSpec) {}

    fn ensure_schema_raw(
        &self,
        name: &str,
        _schemas: Vec<(String, utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>)>,
    ) -> String {
        name.to_owned()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// The mounted router over a fresh store and the given configuration.
fn mounted_with(config: &OagwConfig) -> Router {
    mounted_over(config, Arc::new(FlatHierarchy))
}

/// The mounted router over a fresh store, a configuration and a hierarchy.
fn mounted_over(config: &OagwConfig, hierarchy: Arc<dyn TenantHierarchy>) -> Router {
    register_routes_with_config(
        Router::new(),
        &NoopRegistry,
        Arc::new(OagwStore::new()),
        hierarchy,
        config,
    )
    .expect("the test configuration builds the proxy client")
}

/// The plaintext-opting configuration the stub upstream needs.
fn proxy_config(timeout: u64) -> OagwConfig {
    OagwConfig {
        proxy_timeout_secs: timeout,
        allow_http_upstream: true,
        ..OagwConfig::default()
    }
}

/// A security context the host api-gateway would inject for `tenant`.
fn context(tenant: Uuid, scopes: &[&str]) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_type("user")
        .subject_tenant_id(tenant)
        .token_scopes(scopes.iter().map(|scope| (*scope).to_owned()).collect())
        .build()
        .expect("context builds")
}

/// Send a proxy request with the given headers and body.
async fn call(
    router: Router,
    method: &str,
    uri: &str,
    caller: Option<SecurityContext>,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> axum::response::Response {
    let method = axum::http::Method::from_bytes(method.as_bytes()).expect("method");
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    if let Some(caller) = caller {
        builder = builder.extension(caller);
    }
    router
        .oneshot(
            builder
                .body(Body::from(body.unwrap_or("").to_owned()))
                .expect("request builds"),
        )
        .await
        .expect("request serves")
}

/// Send an authenticated proxy request without extra headers.
async fn proxy(router: Router, method: &str, uri: &str) -> axum::response::Response {
    call(router, method, uri, Some(context(TENANT, ALL)), &[], None).await
}

/// The error-source header value of a response, if it carries one.
fn error_source(response: &axum::response::Response) -> Option<String> {
    response
        .headers()
        .get(ERROR_SOURCE_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// The whole body of a response as a string.
async fn text(response: axum::response::Response) -> String {
    let bytes = Body::new(response)
        .collect()
        .await
        .expect("body collects")
        .to_bytes();
    String::from_utf8(bytes.to_vec()).expect("body is utf-8")
}

/// The whole body of a response as a JSON document.
async fn json_body(response: axum::response::Response) -> Value {
    serde_json::from_str(&text(response).await).expect("body is a JSON document")
}

/// The value of a response header.
fn header_of(response: &axum::response::Response, name: &str) -> Option<String> {
    headers_of(response.headers(), name)
}

/// The value of a header map member.
fn headers_of(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// Assert the canonical problem contract of a gateway failure.
async fn assert_problem(
    response: axum::response::Response,
    status: u16,
    type_suffix: &str,
    instance: &str,
) -> Value {
    // The whole response is read once, so a failing expectation reports the
    // document that was received rather than a moved-away response.
    let (parts, body) = response.into_parts();
    let bytes = Body::new(body)
        .collect()
        .await
        .expect("body collects")
        .to_bytes();
    let document: Value = serde_json::from_slice(&bytes).expect("body is a JSON document");
    assert_eq!(parts.status, StatusCode::from_u16(status).expect("status"), "{document}");
    assert_eq!(
        headers_of(&parts.headers, header::CONTENT_TYPE.as_str()).as_deref(),
        Some("application/problem+json"),
        "{status} carries the problem content type"
    );
    assert_eq!(
        headers_of(&parts.headers, ERROR_SOURCE_HEADER).as_deref(),
        Some(ERROR_SOURCE_GATEWAY),
        "{status} is classified as gateway-originated"
    );
    assert_eq!(
        document["type"],
        json!(format!("gts://gts.cf.core.errors.err.v1~cf.oagw.{type_suffix}")),
        "{document}"
    );
    assert_eq!(document["status"], json!(status), "{document}");
    assert_eq!(document["instance"], json!(instance), "{document}");
    document
}

/// The body of a plaintext stub upstream stored as `alias`.
fn stub_upstream(alias: Option<&str>, port: u16) -> Value {
    // `passthrough: all` is declared, because the schema default of
    // `headers.request.passthrough` is `none`: a gateway that declares no
    // header disposition forwards no client header at all.
    let mut body = json!({
        "server": {
            "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": port } ]
        },
        "protocol": PROTOCOL_HTTP,
        "headers": { "request": { "passthrough": "all" } }
    });
    if let Some(alias) = alias {
        body["alias"] = json!(alias);
    }
    body
}

/// Create an upstream through the management API.
///
/// Returns the stored representation, so a test reads the identifier and the
/// alias the record carries.
async fn create_upstream(router: &Router, tenant: Uuid, body: Value) -> Value {
    let response = call(
        router.clone(),
        "POST",
        &format!("{MOUNT_ROOT}/upstreams"),
        Some(context(tenant, ALL)),
        &[("content-type", "application/json")],
        Some(&body.to_string()),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "{}",
        text(response).await
    );
    json_body(response).await
}

/// Create a route through the management API.
async fn create_route(router: &Router, tenant: Uuid, body: Value) {
    let response = call(
        router.clone(),
        "POST",
        &format!("{MOUNT_ROOT}/routes"),
        Some(context(tenant, ALL)),
        &[("content-type", "application/json")],
        Some(&body.to_string()),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "{}",
        text(response).await
    );
}

/// A route body matching every forwarded method on `path`.
fn route_body(upstream: Uuid, path: &str) -> Value {
    json!({
        "upstream_id": upstream,
        "match": { "http": {
            "methods": ["GET", "POST", "PUT", "DELETE", "PATCH"],
            "path": path
        } }
    })
}

/// The identifier of the one upstream the calling tenant holds.
async fn only_upstream_id(router: &Router) -> Uuid {
    let response = call(
        router.clone(),
        "GET",
        &format!("{MOUNT_ROOT}/upstreams"),
        Some(context(TENANT, ALL)),
        &[],
        None,
    )
    .await;
    let page = json_body(response).await;
    let record = page.as_array().expect("page").first().expect("record");
    Uuid::parse_str(record["id"].as_str().expect("id")).expect("uuid")
}

/// Seed the loopback stub and its route, and return the proxy path the route
/// serves.
///
/// The route matches every forwarded method on [`ROUTE_PATH`] with the `append`
/// suffix mode, so `/proxy/{alias}/v1/things` forwards `/v1/things`.
async fn stubbed(router: &Router, server: &MockServer) -> String {
    let stored = create_upstream(router, TENANT, stub_upstream(Some(ALIAS), server.port())).await;
    let upstream = Uuid::parse_str(stored["id"].as_str().expect("id")).expect("uuid");
    create_route(router, TENANT, route_body(upstream, ROUTE_PATH)).await;
    format!("{MOUNT_ROOT}/proxy/{ALIAS}{ROUTE_PATH}")
}

/// Replace the stored upstream with `body`.
async fn replace_upstream(router: &Router, upstream: Uuid, body: Value) {
    let response = call(
        router.clone(),
        "PUT",
        &format!("{MOUNT_ROOT}/upstreams/{upstream}"),
        Some(context(TENANT, ALL)),
        &[("content-type", "application/json")],
        Some(&body.to_string()),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{}",
        text(response).await
    );
}

/// Stub an upstream that answers every request to `path` with `status`.
///
/// The handle is returned, so a test can count the requests the stub received.
async fn stub_ok<'a>(
    server: &'a MockServer,
    path: &'a str,
    status: u16,
    body: &'a str,
) -> httpmock::Mock<'a> {
    server
        .mock_async(|when, then| {
            when.method("GET").path(path);
            then.status(status)
                .header("x-upstream", "stub")
                .header("content-type", "application/json")
                .body(body);
        })
        .await
}

/// The two-host pool the balancer tests use: one port, two endpoint hosts.
fn pool_body(alias: &str, port: u16) -> Value {
    let mut body = stub_upstream(Some(alias), port);
    body["server"]["endpoints"] = json!([
        { "scheme": "http", "host": "127.0.0.1", "port": port },
        { "scheme": "http", "host": "localhost", "port": port }
    ]);
    body
}

/// The https pool whose alias is the common suffix of its two hosts.
fn suffix_pool() -> Value {
    json!({
        "server": { "endpoints": [
            { "scheme": "https", "host": "us.vendor.com", "port": 443 },
            { "scheme": "https", "host": "eu.vendor.com", "port": 443 }
        ] },
        "protocol": PROTOCOL_HTTP
    })
}

/// A loopback stub whose alias is derived from its host.
async fn dead_upstream(router: &Router) -> String {
    let stored = create_upstream(router, TENANT, stub_upstream(Some(ALIAS), DEAD_PORT)).await;
    let upstream = Uuid::parse_str(stored["id"].as_str().expect("id")).expect("uuid");
    create_route(router, TENANT, route_body(upstream, ROUTE_PATH)).await;
    format!("{MOUNT_ROOT}/proxy/{ALIAS}{ROUTE_PATH}")
}

// ---------- preflight and authentication ----------

#[tokio::test]
async fn a_preflight_is_answered_before_authentication_and_the_store() {
    // No security context and an alias no tenant holds: the preflight is
    // answered locally all the same, because it resolves nothing.
    let (router, store) = {
        let config = proxy_config(5);
        let hierarchy: Arc<dyn TenantHierarchy> = Arc::new(FlatHierarchy);
        let store = Arc::new(OagwStore::new());
        let router = register_routes_with_config(
            Router::new(),
            &NoopRegistry,
            Arc::clone(&store),
            hierarchy,
            &config,
        )
        .expect("the test configuration builds the proxy client");
        (router, store)
    };

    let response = call(
        router,
        "OPTIONS",
        &format!("{MOUNT_ROOT}/proxy/{ALIAS}/v1/things"),
        None,
        &[
            ("origin", "https://app.dev"),
            ("access-control-request-method", "POST"),
            ("access-control-request-headers", "x-trace, x-other"),
        ],
        None,
    )
    .await;

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        header_of(&response, "access-control-allow-origin").as_deref(),
        Some("https://app.dev")
    );
    assert_eq!(
        header_of(&response, "access-control-allow-methods").as_deref(),
        Some("POST")
    );
    assert_eq!(
        header_of(&response, "access-control-allow-headers").as_deref(),
        Some("x-trace, x-other")
    );
    assert!(
        header_of(&response, "access-control-max-age").is_some(),
        "the preflight answer carries the cool-down"
    );
    assert_eq!(error_source(&response).as_deref(), Some(ERROR_SOURCE_GATEWAY));
    assert!(
        text(response).await.is_empty(),
        "a preflight answer carries no body"
    );
    assert_eq!(store.list_upstreams(TENANT).len(), 0, "nothing was created");
}

#[tokio::test]
async fn a_preflight_shape_that_does_not_match_is_an_actual_proxy_request() {
    let router = mounted_with(&proxy_config(5));

    // `OPTIONS` with an `Origin` but no request-method header reaches the
    // authentication step of the proxy-request flow.
    let response = call(
        router,
        "OPTIONS",
        &format!("{MOUNT_ROOT}/proxy/{ALIAS}"),
        None,
        &[("origin", "https://app.dev")],
        None,
    )
    .await;
    assert_problem(
        response,
        401,
        "auth.failed.v1",
        &format!("{MOUNT_ROOT}/proxy/{ALIAS}"),
    )
    .await;
}

#[tokio::test]
async fn an_unauthenticated_proxy_request_is_a_401_problem() {
    let router = mounted_with(&proxy_config(5));
    let path = format!("{MOUNT_ROOT}/proxy/{ALIAS}/v1");

    // The alias is a routing fact of the target-host rows, not of an
    // authentication row, so the document names no upstream here.
    for caller in [None, Some(SecurityContext::anonymous())] {
        let response = call(router.clone(), "GET", &path, caller, &[], None).await;
        let document = assert_problem(response, 401, "auth.failed.v1", &path).await;
        assert!(document["alias"].is_null(), "{document}");
        assert!(document["valid_hosts"].is_null(), "{document}");
    }
}

#[tokio::test]
async fn a_token_without_the_invoke_permission_is_refused_before_the_walk() {
    let router = mounted_with(&proxy_config(5));
    let path = format!("{MOUNT_ROOT}/proxy/{ALIAS}/v1");
    let scopes = ["gts.cf.core.oagw.upstream.v1~:read"];

    let response = call(
        router,
        "GET",
        &path,
        Some(context(TENANT, &scopes)),
        &[],
        None,
    )
    .await;
    let document = assert_problem(response, 401, "auth.failed.v1", &path).await;
    assert!(
        document["detail"]
            .as_str()
            .expect("detail")
            .contains("oagw.proxy.v1~:invoke"),
        "the detail names the missing permission: {document}"
    );
}

// ---------- passthrough ----------

#[tokio::test]
async fn a_proxy_request_passes_the_upstream_response_through_unchanged() {
    let server = MockServer::start_async().await;
    let mock = stub_ok(&server, "/v1/things", 200, r#"{"id":42}"#).await;
    let router = mounted_with(&proxy_config(5));
    let path = stubbed(&router, &server).await;

    let response = call(
        router,
        "GET",
        &format!("{path}/things?page=1"),
        Some(context(TENANT, ALL)),
        &[(TARGET_HOST_HEADER, "127.0.0.1"), ("accept", "application/json")],
        None,
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(error_source(&response).as_deref(), Some(ERROR_SOURCE_UPSTREAM));
    assert_eq!(
        header_of(&response, "x-upstream").as_deref(),
        Some("stub"),
        "an upstream header is passed through"
    );
    assert_eq!(
        header_of(&response, "content-type").as_deref(),
        Some("application/json"),
        "the upstream content type is passed through"
    );
    assert_eq!(text(response).await, r#"{"id":42}"#, "the body is unchanged");
    assert_eq!(mock.calls_async().await, 1, "exactly one upstream call");
}

#[tokio::test]
async fn a_forwarded_request_carries_no_gateway_internal_header() {
    let server = MockServer::start_async().await;
    // The routing header and the caller credentials never reach the upstream:
    // the stub answers only a request that carries neither.
    let mock = server
        .mock_async(|when, then| {
            when.method("POST")
                .path(ROUTE_PATH)
                .header_missing("authorization")
                .header_missing("x-oagw-target-host")
                .header_missing("x-oagw-error-source")
                .header("content-type", "application/json");
            then.status(201).body(r#"{"created":true}"#);
        })
        .await;
    let router = mounted_with(&proxy_config(5));
    let path = stubbed(&router, &server).await;

    let response = call(
        router,
        "POST",
        &path,
        Some(context(TENANT, ALL)),
        &[
            ("content-type", "application/json"),
            (TARGET_HOST_HEADER, "127.0.0.1"),
        ],
        Some(r#"{"name":"thing"}"#),
    )
    .await;

    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "{}",
        text(response).await
    );
    assert_eq!(error_source(&response).as_deref(), Some(ERROR_SOURCE_UPSTREAM));
    assert_eq!(mock.calls_async().await, 1, "the body was forwarded once");
}

#[tokio::test]
async fn an_upstream_error_response_keeps_its_status_and_body() {
    let server = MockServer::start_async().await;
    let mock = stub_ok(&server, ROUTE_PATH, 503, "upstream unavailable").await;
    let router = mounted_with(&proxy_config(5));
    let path = stubbed(&router, &server).await;

    let response = proxy(router, "GET", &path).await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(error_source(&response).as_deref(), Some(ERROR_SOURCE_UPSTREAM));
    assert_eq!(text(response).await, "upstream unavailable");
    // No retry: an error answer is passed through, never re-issued.
    assert_eq!(mock.calls_async().await, 1, "one request, no retry");
}

/// Enable the CORS declaration of the stub upstream.
async fn enable_cors(router: &Router, server: &MockServer, cors: Value) {
    let upstream = only_upstream_id(router).await;
    let mut body = stub_upstream(Some(ALIAS), server.port());
    body["cors"] = cors;
    replace_upstream(router, upstream, body).await;
}

#[tokio::test]
async fn a_cross_origin_request_carries_the_cors_response_headers() {
    let server = MockServer::start_async().await;
    stub_ok(&server, ROUTE_PATH, 200, "ok").await;
    let router = mounted_with(&proxy_config(5));
    let path = stubbed(&router, &server).await;
    enable_cors(
        &router,
        &server,
        json!({
            "enabled": true,
            "allowed_origins": ["https://app.dev"],
            "allowed_methods": ["GET", "POST"]
        }),
    )
    .await;

    let response = call(
        router,
        "GET",
        &path,
        Some(context(TENANT, ALL)),
        &[("origin", "https://app.dev")],
        None,
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header_of(&response, "access-control-allow-origin").as_deref(),
        Some("https://app.dev")
    );
    assert_eq!(header_of(&response, "vary").as_deref(), Some("Origin"));
}

#[tokio::test]
async fn a_cross_origin_request_from_an_unlisted_origin_is_a_403() {
    let server = MockServer::start_async().await;
    stub_ok(&server, ROUTE_PATH, 200, "ok").await;
    let router = mounted_with(&proxy_config(5));
    let path = stubbed(&router, &server).await;
    enable_cors(
        &router,
        &server,
        json!({ "enabled": true, "allowed_origins": ["https://app.dev"] }),
    )
    .await;

    let response = call(
        router,
        "GET",
        &path,
        Some(context(TENANT, ALL)),
        &[("origin", "https://evil.example")],
        None,
    )
    .await;

    let document = assert_problem(response, 403, "cors.origin_not_allowed.v1", &path).await;
    assert!(
        document["detail"]
            .as_str()
            .expect("detail")
            .contains("evil.example"),
        "{document}"
    );
}

#[tokio::test]
async fn a_cross_origin_request_with_a_disallowed_method_is_a_403() {
    let server = MockServer::start_async().await;
    stub_ok(&server, ROUTE_PATH, 200, "ok").await;
    let router = mounted_with(&proxy_config(5));
    let path = stubbed(&router, &server).await;
    enable_cors(
        &router,
        &server,
        json!({
            "enabled": true,
            "allowed_origins": ["https://app.dev"],
            "allowed_methods": ["GET"]
        }),
    )
    .await;

    let response = call(
        router,
        "DELETE",
        &path,
        Some(context(TENANT, ALL)),
        &[("origin", "https://app.dev")],
        None,
    )
    .await;

    assert_problem(response, 403, "cors.method_not_allowed.v1", &path).await;
}

// ---------- routing ----------

#[tokio::test]
async fn an_unknown_alias_is_a_404_that_names_the_alias() {
    let router = mounted_with(&proxy_config(5));
    let path = format!("{MOUNT_ROOT}/proxy/{ALIAS}/v1");

    let response = proxy(router, "GET", &path).await;

    // The walk failed before it resolved an upstream, so the record names the
    // alias in its detail and carries no target-host extension member.
    let document = assert_problem(response, 404, "route.not_found.v1", &path).await;
    assert!(
        document["detail"]
            .as_str()
            .expect("detail")
            .contains(ALIAS),
        "{document}"
    );
    assert!(document["valid_hosts"].is_null(), "{document}");
}

#[tokio::test]
async fn a_request_matching_no_enabled_route_is_a_404() {
    let server = MockServer::start_async().await;
    let router = mounted_with(&proxy_config(5));
    // The upstream is enabled and reachable, but declares no route: the request
    // is refused by the route match and never reaches the upstream.
    let stored = create_upstream(&router, TENANT, stub_upstream(Some(ALIAS), server.port())).await;
    assert!(
        stored["enabled"].as_bool().expect("enabled"),
        "the stub upstream is enabled"
    );
    let uri = format!("{MOUNT_ROOT}/proxy/{ALIAS}{ROUTE_PATH}");
    let response = proxy(router, "GET", &uri).await;
    let document = assert_problem(response, 404, "route.not_found.v1", &uri).await;
    let detail = document["detail"].as_str().expect("detail");
    assert!(detail.contains(ROUTE_PATH), "{document}");
}

#[tokio::test]
async fn a_disabled_closest_match_is_a_503_that_shadows_the_ancestor() {
    let chain = BTreeMap::from([(TENANT, vec![ANCESTOR])]);
    let router = mounted_over(&proxy_config(5), Arc::new(StaticHierarchy::new(chain)));

    // The ancestor holds the alias enabled; the closest match of the calling
    // tenant holds it disabled.
    let mut ancestor = stub_upstream(Some(ALIAS), DEAD_PORT);
    ancestor["enabled"] = json!(true);
    create_upstream(&router, ANCESTOR, ancestor).await;
    let mut shadowed = stub_upstream(Some(ALIAS), DEAD_PORT + 1);
    shadowed["enabled"] = json!(false);
    create_upstream(&router, TENANT, shadowed).await;

    let path = format!("{MOUNT_ROOT}/proxy/{ALIAS}");
    let response = proxy(router, "GET", &path).await;

    let document = assert_problem(response, 503, "link.unavailable.v1", &path).await;
    let detail = document["detail"].as_str().expect("detail");
    assert!(detail.contains("disabled"), "the closest match decided: {document}");
    assert!(
        !detail.contains("connection"),
        "the ancestor target was never dialed: {document}"
    );
}

#[tokio::test]
async fn a_suffix_a_disabled_mode_rejects_is_a_400() {
    let server = MockServer::start_async().await;
    let router = mounted_with(&proxy_config(5));
    let stored = create_upstream(&router, TENANT, stub_upstream(Some(ALIAS), server.port())).await;
    let upstream = Uuid::parse_str(stored["id"].as_str().expect("id")).expect("uuid");
    let body = json!({
        "upstream_id": upstream,
        "match": { "http": {
            "methods": ["GET"],
            "path": ROUTE_PATH,
            "path_suffix_mode": "disabled"
        } }
    });
    create_route(&router, TENANT, body).await;

    let path = format!("{MOUNT_ROOT}/proxy/{ALIAS}{ROUTE_PATH}/things");
    let response = proxy(router, "GET", &path).await;
    assert_problem(response, 400, "validation.error.v1", &path).await;
}

#[tokio::test]
async fn a_query_parameter_outside_the_allowlist_is_a_400() {
    let server = MockServer::start_async().await;
    let router = mounted_with(&proxy_config(5));
    let stored = create_upstream(&router, TENANT, stub_upstream(Some(ALIAS), server.port())).await;
    let upstream = Uuid::parse_str(stored["id"].as_str().expect("id")).expect("uuid");
    let body = json!({
        "upstream_id": upstream,
        "match": { "http": {
            "methods": ["GET"],
            "path": ROUTE_PATH,
            "query_allowlist": ["page"]
        } }
    });
    create_route(&router, TENANT, body).await;

    let path = format!("{MOUNT_ROOT}/proxy/{ALIAS}{ROUTE_PATH}?page=1&dump=1");
    // The instance names the request path, which carries no query string.
    let requested = path.split('?').next().expect("path");
    let response = proxy(router, "GET", &path).await;
    let document = assert_problem(response, 400, "validation.error.v1", requested).await;
    assert!(
        document["detail"].as_str().expect("detail").contains("dump"),
        "{document}"
    );
}

// ---------- endpoint selection ----------

#[tokio::test]
async fn a_multi_endpoint_pool_balances_by_round_robin() {
    let server = MockServer::start_async().await;
    let authority = format!("127.0.0.1:{}", server.port());
    let loopback = format!("localhost:{}", server.port());
    let first = server
        .mock_async(|when, then| {
            when.method("GET").path(ROUTE_PATH).header("host", &authority);
            then.status(200).body("first");
        })
        .await;
    let second = server
        .mock_async(|when, then| {
            when.method("GET").path(ROUTE_PATH).header("host", &loopback);
            then.status(200).body("second");
        })
        .await;
    let router = mounted_with(&proxy_config(5));
    // One port, two hosts: the pool holds both endpoints, and the explicit
    // alias names the pool rather than any endpoint of it.
    let stored = create_upstream(&router, TENANT, pool_body("pool.test", server.port())).await;
    let upstream = Uuid::parse_str(stored["id"].as_str().expect("id")).expect("uuid");
    create_route(&router, TENANT, route_body(upstream, ROUTE_PATH)).await;

    for (expected, mock) in [("first", &first), ("second", &second)] {
        let response = proxy(
            router.clone(),
            "GET",
            &format!("{MOUNT_ROOT}/proxy/pool.test{ROUTE_PATH}"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(text(response).await, expected, "the balancer advanced once");
        assert_eq!(mock.calls_async().await, 1, "{expected}");
    }
}

#[tokio::test]
async fn a_target_host_header_selects_the_endpoint_and_bypasses_the_cursor() {
    let server = MockServer::start_async().await;
    let authority = format!("127.0.0.1:{}", server.port());
    let loopback = format!("localhost:{}", server.port());
    let first = server
        .mock_async(|when, then| {
            when.method("GET").path(ROUTE_PATH).header("host", &authority);
            then.status(200).body("first");
        })
        .await;
    let second = server
        .mock_async(|when, then| {
            when.method("GET").path(ROUTE_PATH).header("host", &loopback);
            then.status(200).body("second");
        })
        .await;
    let router = mounted_with(&proxy_config(5));
    let stored = create_upstream(&router, TENANT, pool_body("pool.test", server.port())).await;
    let upstream = Uuid::parse_str(stored["id"].as_str().expect("id")).expect("uuid");
    create_route(&router, TENANT, route_body(upstream, ROUTE_PATH)).await;

    // Two balanced requests leave the cursor on the first endpoint again, so
    // the next balanced request would dial `127.0.0.1`.
    for _ in 0..2 {
        let response = proxy(
            router.clone(),
            "GET",
            &format!("{MOUNT_ROOT}/proxy/pool.test{ROUTE_PATH}"),
        )
        .await;
        let _ = text(response).await;
    }

    let response = call(
        router,
        "GET",
        &format!("{MOUNT_ROOT}/proxy/pool.test{ROUTE_PATH}"),
        Some(context(TENANT, ALL)),
        &[(TARGET_HOST_HEADER, "localhost")],
        None,
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(text(response).await, "second", "the named endpoint was dialed");
    assert_eq!(second.calls_async().await, 2, "the named endpoint");
    assert_eq!(
        first.calls_async().await,
        1,
        "the cursor was bypassed, so the first host was not dialed again"
    );
}

#[tokio::test]
async fn a_missing_target_host_is_a_400_naming_the_pool_hosts() {
    let router = mounted_with(&proxy_config(5));
    let stored = create_upstream(&router, TENANT, suffix_pool()).await;
    let alias = stored["alias"].as_str().expect("alias").to_owned();
    let upstream = Uuid::parse_str(stored["id"].as_str().expect("id")).expect("uuid");
    create_route(&router, TENANT, route_body(upstream, ROUTE_PATH)).await;

    let path = format!("{MOUNT_ROOT}/proxy/{alias}{ROUTE_PATH}");
    let response = proxy(router, "GET", &path).await;

    let document = assert_problem(response, 400, "routing.missing_target_host.v1", &path).await;
    assert_eq!(document["alias"], json!(alias), "{document}");
    assert_eq!(
        document["valid_hosts"],
        json!(["us.vendor.com", "eu.vendor.com"]),
        "{document}"
    );
}

#[tokio::test]
async fn an_unknown_target_host_is_a_400_that_names_the_rejected_value() {
    let router = mounted_with(&proxy_config(5));
    let stored = create_upstream(&router, TENANT, suffix_pool()).await;
    let alias = stored["alias"].as_str().expect("alias").to_owned();
    let upstream = Uuid::parse_str(stored["id"].as_str().expect("id")).expect("uuid");
    create_route(&router, TENANT, route_body(upstream, ROUTE_PATH)).await;

    let path = format!("{MOUNT_ROOT}/proxy/{alias}{ROUTE_PATH}");
    let response = call(
        router,
        "GET",
        &path,
        Some(context(TENANT, ALL)),
        &[(TARGET_HOST_HEADER, "nope.vendor.com")],
        None,
    )
    .await;

    let document = assert_problem(response, 400, "routing.unknown_target_host.v1", &path).await;
    assert_eq!(document["invalid_value"], json!("nope.vendor.com"), "{document}");
    assert_eq!(document["valid_hosts"].as_array().map(Vec::len), Some(2));
}

// ---------- upstream call failures ----------

#[tokio::test]
async fn an_unreachable_upstream_is_a_502() {
    let router = mounted_with(&proxy_config(5));
    let path = dead_upstream(&router).await;

    let response = proxy(router, "GET", &path).await;

    assert_problem(response, 502, "downstream.error.v1", &path).await;
}

#[tokio::test]
async fn the_circuit_breaker_opens_after_five_failed_calls() {
    let router = mounted_with(&proxy_config(5));
    let path = dead_upstream(&router).await;

    for _ in 0..5 {
        let response = proxy(router.clone(), "GET", &path).await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY, "{path}");
    }

    // The breaker is open: the request is refused without contacting the
    // upstream, whatever the endpoint is.
    let response = proxy(router, "GET", &path).await;
    let document = assert_problem(response, 503, "circuit_breaker.open.v1", &path).await;
    assert!(
        document["retry_after_seconds"].is_number(),
        "an open breaker carries the cool-down: {document}"
    );
}

#[tokio::test]
async fn a_timed_out_upstream_call_is_a_504_request_timeout() {
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method("GET").path(ROUTE_PATH);
            then.status(200).delay(Duration::from_secs(4));
        })
        .await;
    // A one-second proxy timeout keeps the test under the CI budget while the
    // upstream needs four seconds to answer.
    let router = mounted_with(&proxy_config(1));
    let path = stubbed(&router, &server).await;

    let response = proxy(router, "GET", &path).await;

    assert_problem(response, 504, "timeout.request.v1", &path).await;
}

#[tokio::test]
async fn an_upgrade_request_is_handed_off_without_calling_the_upstream() {
    let server = MockServer::start_async().await;
    let mock = stub_ok(&server, ROUTE_PATH, 200, "ok").await;
    let router = mounted_with(&proxy_config(5));
    let path = stubbed(&router, &server).await;

    let response = call(
        router,
        "GET",
        &path,
        Some(context(TENANT, ALL)),
        &[("upgrade", "websocket"), ("connection", "Upgrade")],
        None,
    )
    .await;

    // The handoff happened before the entry-2.4 upstream call, so the upgrade
    // request reached entry 2.6 and was refused there: the request carries no
    // `Sec-WebSocket-Key`, which the upgrade path validates before it dials the
    // selected endpoint.
    let document = assert_problem(response, 400, "validation.error.v1", &path).await;
    assert!(
        document["detail"]
            .as_str()
            .expect("detail")
            .contains("Sec-WebSocket-Key"),
        "the error names the upgrade header that is missing: {document}"
    );
    assert_eq!(
        mock.calls_async().await,
        0,
        "an upgrade never reaches the upstream call"
    );
}

/// Serve one `text/event-stream` response in two chunks, `gap` apart.
///
/// Returns the port the endpoint has to name; the thread answers the first
/// request it accepts and then exits.
fn event_stream_upstream(gap: u64) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("the listener binds");
    let port = listener
        .local_addr()
        .expect("the listener has an address")
        .port();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("the gateway connects");
        let mut buffer = [0_u8; 4096];
        let mut read = 0_usize;
        loop {
            let taken = stream.read(&mut buffer[read..]).expect("the request is read");
            read += taken;
            if buffer[..read].windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                  transfer-encoding: chunked\r\n\r\n",
            )
            .expect("the head is written");
        for event in ["data: first\n\n", "data: second\n\n"] {
            let chunk = format!("{:x}\r\n{event}\r\n", event.len());
            stream
                .write_all(chunk.as_bytes())
                .expect("the chunk is written");
            stream.flush().expect("the chunk is flushed");
            std::thread::sleep(Duration::from_millis(gap));
        }
        stream.write_all(b"0\r\n\r\n").expect("the stream ends");
    });
    port
}

#[tokio::test]
async fn a_streamed_response_is_relayed_without_waiting_for_the_upstream() {
    // Two events, 400 milliseconds apart: a gateway that buffers the body
    // would deliver the first frame only after the upstream closed the stream.
    let port = event_stream_upstream(400);
    let router = mounted_with(&proxy_config(10));
    let stored = create_upstream(&router, TENANT, stub_upstream(Some(ALIAS), port)).await;
    let upstream = Uuid::parse_str(stored["id"].as_str().expect("id")).expect("uuid");
    create_route(&router, TENANT, route_body(upstream, ROUTE_PATH)).await;
    let path = format!("{MOUNT_ROOT}/proxy/{ALIAS}{ROUTE_PATH}");

    let response = proxy(router, "GET", &path).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header_of(&response, "content-type").as_deref(),
        Some("text/event-stream")
    );
    assert_eq!(error_source(&response).as_deref(), Some(ERROR_SOURCE_UPSTREAM));

    let started = Instant::now();
    let mut events = response.into_body().into_data_stream();
    let first = events
        .next()
        .await
        .expect("the first frame arrives")
        .expect("the frame is readable");
    let first_after = started.elapsed();
    assert!(
        first_after < Duration::from_millis(250),
        "the first event arrived after {first_after:?}, so the gateway buffered"
    );
    let drained = async {
        while let Some(frame) = events.next().await {
            let _ = frame.expect("the frame is readable");
        }
    };
    tokio::time::timeout(Duration::from_secs(5), drained)
        .await
        .expect("the stream ends");
    assert!(
        started.elapsed() > Duration::from_millis(350),
        "the second event came from the open upstream exchange"
    );
    assert_eq!(
        String::from_utf8(first.to_vec()).expect("the frame is utf-8"),
        "data: first\n\n",
        "the chunk boundary the upstream wrote is preserved"
    );
}

// ---------- mount surface ----------

#[tokio::test]
async fn the_proxy_path_adds_no_management_endpoint() {
    let router = mounted_with(&proxy_config(5));

    // `/proxy` without an alias is the canonical fallback, not a collection.
    let response = proxy(router.clone(), "GET", &format!("{MOUNT_ROOT}/proxy")).await;
    assert_problem(response, 404, "route.not_found.v1", &format!("{MOUNT_ROOT}/proxy")).await;

    // A `POST` to a proxy path is a forwarded call, so it is the proxy pipeline
    // that answers — with the `401` of the invoke permission and not a
    // management create.
    let response = call(
        router,
        "POST",
        &format!("{MOUNT_ROOT}/proxy/{ALIAS}"),
        None,
        &[("content-type", "application/json")],
        Some("{}"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        header_of(&response, ERROR_SOURCE_HEADER).as_deref(),
        Some(ERROR_SOURCE_GATEWAY)
    );
}
// @cpt-end:cpt-cf-oagw-dod-proxy-test-coverage:p2:inst-full
