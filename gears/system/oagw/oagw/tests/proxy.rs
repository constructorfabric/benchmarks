//! Integration tests for the data plane.
//!
//! Each test builds a real gateway, points it at a real local HTTP upstream and
//! drives it over a socket, so streaming, upgrades, headers and problem
//! documents are exercised exactly as a client would see them. The request-body
//! limits are the exception: the test HTTP client would reject those before the
//! gateway ever sees them, so they call the engine directly.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use http_body_util::BodyExt as _;
use serde_json::{Value, json};
use uuid::Uuid;

use common::{
    Recorder, aliased_upstream_body, get, post, route_body, spawn, spawn_on, spawn_on_port,
    upstream_body,
};
use oagw::domain::ids;
use oagw::domain::service::ControlPlaneService as _;

mod common;

const HTTP_PROTOCOL: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
const AUTH_PLUGIN: &str = "gts.cf.core.oagw.auth_plugin.v1~";
const GUARD_PLUGIN: &str = "gts.cf.core.oagw.guard_plugin.v1~";
const TRANSFORM_PLUGIN: &str = "gts.cf.core.oagw.transform_plugin.v1~";

/// The `type` member of a problem document.
fn problem_type(response: &common::TestResponse) -> String {
    response.json()["type"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

/// Renders an engine response the way the wire helpers render a wire response.
async fn render(response: axum::response::Response) -> common::TestResponse {
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.expect("read the body").to_bytes();
    common::TestResponse {
        status: parts.status.as_u16(),
        headers: parts
            .headers
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_ascii_lowercase(),
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
            .collect(),
        body: bytes.to_vec(),
    }
}

/// A `ProxiedRequest` addressed to `alias` with the given headers and body.
fn engine_request(
    alias: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> oagw::infra::proxy::ProxiedRequest {
    oagw::infra::proxy::ProxiedRequest {
        alias: alias.to_owned(),
        method: axum::http::Method::POST,
        path_suffix: "/v1/models".to_owned(),
        query: Vec::new(),
        headers: headers
            .iter()
            .map(|(name, value)| ((*name).to_ascii_lowercase(), (*value).to_owned()))
            .collect(),
        body: bytes::Bytes::copy_from_slice(body),
        client_ip: String::new(),
        is_websocket: false,
    }
}

/// Runs one engine call with the harness tenant's security context.
async fn call_engine(
    harness: &Arc<common::Harness>,
    request: oagw::infra::proxy::ProxiedRequest,
) -> common::TestResponse {
    let context = harness.context();
    render(
        harness
            .state
            .engine
            .handle(&context, request, None)
            .await,
    )
    .await
}

// ---------------------------------------------------------------------------------------
// Publishing helpers
// ---------------------------------------------------------------------------------------

/// Creates an upstream and returns `(id, alias)`.
async fn register_upstream(harness: &Arc<common::Harness>, mut upstream: Value) -> (String, String) {
    upstream["protocol"] = json!(HTTP_PROTOCOL);
    let response = post(harness.serve_once().await, "/oagw/v1/upstreams", upstream).await;
    assert_eq!(response.status, 201, "{}", response.text());
    let body = response.json();
    (
        body["id"].as_str().unwrap_or_default().to_owned(),
        body["alias"].as_str().unwrap_or_default().to_owned(),
    )
}

/// Creates a route for `upstream_id`.
async fn register_route(harness: &Arc<common::Harness>, upstream_id: &str, mut route: Value) {
    route["upstream_id"] = json!(upstream_id);
    let response = post(harness.serve_once().await, "/oagw/v1/routes", route).await;
    assert_eq!(response.status, 201, "{}", response.text());
}

/// A route body for a fresh upstream.
fn route_for(upstream_id: &str, methods: &[&str], path: &str) -> Value {
    route_body(upstream_id, methods, path)
}

/// Publishes an upstream plus a `GET`/`POST` route and returns the alias.
async fn publish(harness: &Arc<common::Harness>, upstream: Value, route: Value) -> String {
    let (id, alias) = register_upstream(harness, upstream).await;
    let mut route = route;
    route["upstream_id"] = json!(id);
    register_route(harness, &id, route).await;
    alias
}

/// Publishes an upstream over `router` with a `GET {path}` route.
async fn serve_behind_gateway(
    harness: &Arc<common::Harness>,
    router: axum::Router,
    methods: &[&str],
    path: &str,
) -> (String, String) {
    let addr = spawn(router).await;
    let upstream = aliased_upstream_body(
        &addr.ip().to_string(),
        addr.port(),
        "http",
        &format!("{}:{}", addr.ip(), addr.port()),
    );
    let (id, alias) = register_upstream(harness, upstream).await;
    register_route(harness, &id, route_for(&id, methods, path)).await;
    (id, alias)
}

// ---------------------------------------------------------------------------------------
// Forwarding
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn a_proxied_request_reaches_the_upstream_and_returns_its_response() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let (_, alias) = serve_behind_gateway(&harness, recorder.router(), &["GET"], "/v1").await;

    let response = get(
        harness.serve_once().await,
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.text());
    assert_eq!(response.json(), json!({"ok": true}));
    assert_eq!(
        recorder.first().expect("the upstream was called").path,
        "/v1/models"
    );
}

#[tokio::test]
async fn the_path_suffix_and_query_are_forwarded() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let addr = spawn(recorder.router()).await;
    let upstream = aliased_upstream_body(
        &addr.ip().to_string(),
        addr.port(),
        "http",
        &format!("{}:{}", addr.ip(), addr.port()),
    );
    let (id, alias) = register_upstream(&harness, upstream).await;
    let mut route = route_for(&id, &["GET"], "/v1");
    route["match"]["http"]["query_allowlist"] = json!(["limit"]);
    register_route(&harness, &id, route).await;

    let response = get(
        harness.serve_once().await,
        &format!("/oagw/v1/proxy/{alias}/v1/models?limit=1"),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.text());
    let seen = recorder.first().expect("the upstream was called");
    assert_eq!(seen.path, "/v1/models");
    assert_eq!(seen.query, "limit=1");
}

#[tokio::test]
async fn request_bodies_are_forwarded() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let (_, alias) =
        serve_behind_gateway(&harness, recorder.router(), &["POST"], "/v1/echo").await;

    let response = post(
        harness.serve_once().await,
        &format!("/oagw/v1/proxy/{alias}/v1/echo"),
        json!({"a": 1}),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.text());
    let seen = recorder.first().expect("the upstream was called");
    assert_eq!(seen.method, "POST");
    assert_eq!(seen.text(), "{\"a\":1}");
}

#[tokio::test]
async fn an_unknown_alias_returns_404() {
    let harness = common::Harness::allowing_http();
    let response = get(
        harness.serve_once().await,
        "/oagw/v1/proxy/no-such-alias/anything",
    )
    .await;
    assert_eq!(response.status, 404);
    assert_eq!(response.header("X-OAGW-Error-Source"), Some("gateway"));
    assert_eq!(problem_type(&response), ids::ERR_ROUTE_NOT_FOUND);
}

#[tokio::test]
async fn no_matching_route_returns_404() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let (_, alias) = serve_behind_gateway(&harness, recorder.router(), &["GET"], "/v1").await;

    let response = get(
        harness.serve_once().await,
        &format!("/oagw/v1/proxy/{alias}/v9/none"),
    )
    .await;
    assert_eq!(response.status, 404);
    assert_eq!(problem_type(&response), ids::ERR_ROUTE_NOT_FOUND);
    assert_eq!(recorder.count(), 0, "the upstream must not be called");
}

#[tokio::test]
async fn a_request_method_outside_the_allowlist_is_rejected() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let (_, alias) = serve_behind_gateway(&harness, recorder.router(), &["GET"], "/v1").await;

    let response = post(
        harness.serve_once().await,
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
        json!({}),
    )
    .await;
    assert_eq!(response.status, 404);
    assert_eq!(problem_type(&response), ids::ERR_ROUTE_NOT_FOUND);
    assert_eq!(recorder.count(), 0);
}

#[tokio::test]
async fn alias_resolution_is_case_insensitive() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let addr = spawn(recorder.router()).await;
    let upstream = aliased_upstream_body(
        &addr.ip().to_string(),
        addr.port(),
        "http",
        &format!("mixed-case.{}", Uuid::new_v4().simple()),
    );
    let (id, alias) = register_upstream(&harness, upstream).await;
    register_route(&harness, &id, route_for("placeholder", &["GET"], "/v1")).await;
    let mixed: String = alias
        .chars()
        .map(|c| if c.is_ascii_alphabetic() { c.to_ascii_uppercase() } else { c })
        .collect();
    assert_ne!(mixed, alias, "the test needs a mixed-case alias");

    let response = get(
        harness.serve_once().await,
        &format!("/oagw/v1/proxy/{mixed}/v1/models"),
    )
    .await;
    assert_eq!(response.status, 200, "{mixed} -> {}", response.text());
}

// ---------------------------------------------------------------------------------------
// Tenant hierarchy
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn a_descendant_shadows_an_ancestor_upstream() {
    let root = Uuid::new_v4();
    let middle = Uuid::new_v4();
    let child = Uuid::new_v4();
    let mut parents = std::collections::HashMap::new();
    parents.insert(middle, root);
    parents.insert(child, middle);
    let harness = common::Harness::new(
        oagw::config::OagwConfig {
            allow_http_upstream: true,
            ..oagw::config::OagwConfig::default()
        },
        parents,
    );
    let ancestor = harness.with_tenant(middle);
    let descendant = harness.with_tenant(child);

    let recorder = Recorder::new();
    let ancestor_addr = spawn(
        recorder.clone().responding_with(
            axum::http::StatusCode::OK,
            "application/json",
            "{\"side\":\"ancestor\"}",
        ),
    )
    .await;
    let descendant_addr = spawn(
        recorder.clone().responding_with(
            axum::http::StatusCode::OK,
            "application/json",
            "{\"side\":\"descendant\"}",
        ),
    )
    .await;

    // Both tenants register the same alias; the descendant must win for the
    // descendant's callers and must not disturb the ancestor's own.
    for (owner, addr) in [(&ancestor, ancestor_addr), (&descendant, descendant_addr)] {
        let upstream = aliased_upstream_body(
            &addr.ip().to_string(),
            addr.port(),
            "http",
            "shadowed.example.com",
        );
        let (id, _) = register_upstream(owner, upstream).await;
        register_route(owner, &id, route_for(&id, &["GET"], "/v1")).await;
    }

    let from_child = get(
        descendant.serve_once().await,
        "/oagw/v1/proxy/shadowed.example.com/v1/models",
    )
    .await;
    assert_eq!(from_child.status, 200, "{}", from_child.text());
    assert_eq!(from_child.json()["side"], json!("descendant"));

    let from_parent = get(
        ancestor.serve_once().await,
        "/oagw/v1/proxy/shadowed.example.com/v1/models",
    )
    .await;
    assert_eq!(from_parent.json()["side"], json!("ancestor"));
}

// ---------------------------------------------------------------------------------------
// X-OAGW-Target-Host
// ---------------------------------------------------------------------------------------

/// Two live mock upstreams on distinct loopback hosts behind one alias.
///
/// The endpoints must share a port, so the first listener is bound first and
/// the second reuses its port on a different loopback address.
async fn two_endpoint_upstream(harness: &Arc<common::Harness>) -> String {
    let first = spawn_on(IpAddr::from([127, 0, 0, 1]), Recorder::new().router()).await;
    let second = spawn_on_port(IpAddr::from([127, 0, 0, 2]), first.port(), Recorder::new().router()).await;
    assert_eq!(first.port(), second.port());
    let upstream = json!({
        "server": {"endpoints": [
            {"scheme": "http", "host": "127.0.0.1", "port": first.port()},
            {"scheme": "http", "host": "127.0.0.2", "port": second.port()}
        ]},
        "alias": "pooled.example.com"
    });
    publish(harness, upstream, route_for("placeholder", &["GET"], "/v1")).await
}

/// Two `https` endpoints on a shared domain suffix, which the alias then
/// collapses to.
async fn suffixed_upstream(harness: &Arc<common::Harness>) -> String {
    let upstream = json!({
        "server": {"endpoints": [
            {"scheme": "https", "host": "us.vendor.com", "port": 443},
            {"scheme": "https", "host": "eu.vendor.com", "port": 443}
        ]}
    });
    publish(harness, upstream, route_for("placeholder", &["GET"], "/v1")).await
}

#[tokio::test]
async fn a_single_endpoint_routes_without_the_header() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let (_, alias) = serve_behind_gateway(&harness, recorder.router(), &["GET"], "/v1").await;

    let response = get(
        harness.serve_once().await,
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.text());
    assert_eq!(recorder.count(), 1);
}

#[tokio::test]
async fn a_single_endpoint_validates_a_supplied_header() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let (_, alias) = serve_behind_gateway(&harness, recorder.router(), &["GET"], "/v1").await;

    let response = common::request(
        harness.serve_once().await,
        "GET",
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
        &[("X-OAGW-Target-Host", "not-configured.example.com")],
        Vec::new(),
    )
    .await;
    assert_eq!(response.status, 400, "{}", response.text());
    assert_eq!(problem_type(&response), ids::ERR_UNKNOWN_TARGET_HOST);
    assert_eq!(recorder.count(), 0);
}

#[tokio::test]
async fn a_multi_endpoint_upstream_serves_every_endpoint() {
    let harness = common::Harness::allowing_http();
    let alias = two_endpoint_upstream(&harness).await;

    for _ in 0..4 {
        let response = get(
            harness.serve_once().await,
            &format!("/oagw/v1/proxy/{alias}/v1/models"),
        )
        .await;
        assert_eq!(response.status, 200, "{}", response.text());
    }
}

#[tokio::test]
async fn a_multi_endpoint_upstream_honours_the_header() {
    let harness = common::Harness::allowing_http();
    let alias = two_endpoint_upstream(&harness).await;

    let response = common::request(
        harness.serve_once().await,
        "GET",
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
        &[("X-OAGW-Target-Host", "127.0.0.2")],
        Vec::new(),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.text());
}

#[tokio::test]
async fn a_common_suffix_alias_requires_the_header() {
    let harness = common::Harness::allowing_http();
    let alias = suffixed_upstream(&harness).await;
    assert_eq!(alias, "vendor.com");

    let response = get(
        harness.serve_once().await,
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
    )
    .await;
    assert_eq!(response.status, 400, "{}", response.text());
    assert_eq!(problem_type(&response), ids::ERR_MISSING_TARGET_HOST);
    let hosts = response.json()["valid_hosts"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(hosts.len(), 2, "{hosts:?}");
}

#[tokio::test]
async fn a_malformed_header_value_is_rejected() {
    let harness = common::Harness::allowing_http();
    let alias = suffixed_upstream(&harness).await;

    let response = common::request(
        harness.serve_once().await,
        "GET",
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
        &[("X-OAGW-Target-Host", "us.vendor.com:8443")],
        Vec::new(),
    )
    .await;
    assert_eq!(response.status, 400, "{}", response.text());
    assert_eq!(problem_type(&response), ids::ERR_INVALID_TARGET_HOST);
}

#[tokio::test]
async fn a_header_naming_an_unconfigured_endpoint_is_rejected() {
    let harness = common::Harness::allowing_http();
    let alias = suffixed_upstream(&harness).await;

    let response = common::request(
        harness.serve_once().await,
        "GET",
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
        &[("X-OAGW-Target-Host", "apac.vendor.com")],
        Vec::new(),
    )
    .await;
    assert_eq!(response.status, 400, "{}", response.text());
    assert_eq!(problem_type(&response), ids::ERR_UNKNOWN_TARGET_HOST);
    assert!(response.json()["valid_hosts"].is_array());
}

#[tokio::test]
async fn the_target_host_header_does_not_reach_the_upstream() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let (_, alias) = serve_behind_gateway(&harness, recorder.router(), &["GET"], "/v1").await;

    let response = common::request(
        harness.serve_once().await,
        "GET",
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
        &[("X-OAGW-Target-Host", "127.0.0.1")],
        Vec::new(),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.text());
    assert!(
        recorder
            .first()
            .expect("called")
            .has_no_header("x-oagw-target-host")
    );
}

// ---------------------------------------------------------------------------------------
// Header transformation
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn hop_by_hop_headers_do_not_reach_the_upstream() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let (_, alias) = serve_behind_gateway(&harness, recorder.router(), &["GET"], "/v1").await;

    let response = common::request(
        harness.serve_once().await,
        "GET",
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
        &[("Connection", "keep-alive"), ("Upgrade", "h2c")],
        Vec::new(),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.text());
    let seen = recorder.first().expect("called");
    assert!(seen.has_no_header("connection"));
    assert!(seen.has_no_header("upgrade"));
}

#[tokio::test]
async fn the_upstream_sees_its_own_host() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let (_, alias) = serve_behind_gateway(&harness, recorder.router(), &["GET"], "/v1").await;
    let port = alias.split(':').nth(1).unwrap_or_default().to_owned();

    let response = common::request(
        harness.serve_once().await,
        "GET",
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
        &[("Host", "oagw.internal")],
        Vec::new(),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.text());
    let host = recorder
        .first()
        .expect("called")
        .header("host")
        .unwrap_or_default()
        .to_owned();
    assert!(
        host == format!("127.0.0.1:{port}") || host == "127.0.0.1",
        "the upstream host, got {host}"
    );
}

/// Publishes an upstream with a `headers` policy over `recorder`.
async fn upstream_with_headers(
    harness: &Arc<common::Harness>,
    recorder: &Recorder,
    headers: Value,
) -> String {
    let addr = spawn(recorder.router()).await;
    let upstream = json!({
        "server": {"endpoints": [{"scheme": "http", "host": addr.ip().to_string(), "port": addr.port()}]},
        "alias": format!("headers.{}", Uuid::new_v4().simple()),
        "headers": headers
    });
    publish(harness, upstream, route_for("placeholder", &["GET"], "/v1")).await
}

#[tokio::test]
async fn passthrough_none_forwards_no_inbound_headers() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let alias =
        upstream_with_headers(&harness, &recorder, json!({"request": {"passthrough": "none"}})).await;

    let response = common::request(
        harness.serve_once().await,
        "GET",
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
        &[("X-Custom", "yes")],
        Vec::new(),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.text());
    assert!(recorder.first().expect("called").has_no_header("x-custom"));
}

#[tokio::test]
async fn an_allowlist_forwards_only_listed_headers() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let alias = upstream_with_headers(
        &harness,
        &recorder,
        json!({"request": {"passthrough": "allowlist", "passthrough_allowlist": ["X-Trace-Id"]}}),
    )
    .await;

    let response = common::request(
        harness.serve_once().await,
        "GET",
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
        &[("X-Trace-Id", "t"), ("X-Other", "o")],
        Vec::new(),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.text());
    let seen = recorder.first().expect("called");
    assert_eq!(seen.header("x-trace-id"), Some("t"));
    assert!(seen.has_no_header("x-other"));
}

#[tokio::test]
async fn passthrough_all_forwards_the_rest() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let alias =
        upstream_with_headers(&harness, &recorder, json!({"request": {"passthrough": "all"}})).await;

    let response = common::request(
        harness.serve_once().await,
        "GET",
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
        &[("X-Trace-Id", "t"), ("X-Secret", "s")],
        Vec::new(),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.text());
    let seen = recorder.first().expect("called");
    assert_eq!(seen.header("x-trace-id"), Some("t"));
    assert_eq!(seen.header("x-secret"), Some("s"));
}

#[tokio::test]
async fn set_overwrites_and_add_appends() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let alias = upstream_with_headers(
        &harness,
        &recorder,
        json!({"request": {"passthrough": "all", "set": {"X-Foo": "set"}, "add": {"X-Bar": "added"}}}),
    )
    .await;

    let response = common::request(
        harness.serve_once().await,
        "GET",
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
        &[("X-Foo", "original")],
        Vec::new(),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.text());
    let seen = recorder.first().expect("called");
    assert_eq!(seen.header("x-foo"), Some("set"));
    assert_eq!(seen.header("x-bar"), Some("added"));
}

#[tokio::test]
async fn remove_strips_a_request_header() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let alias = upstream_with_headers(
        &harness,
        &recorder,
        json!({"request": {"passthrough": "all", "remove": ["X-Secret"]}}),
    )
    .await;

    let response = common::request(
        harness.serve_once().await,
        "GET",
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
        &[("X-Secret", "s")],
        Vec::new(),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.text());
    assert!(recorder.first().expect("called").has_no_header("x-secret"));
}

#[tokio::test]
async fn response_header_rules_apply_to_the_client_response() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let addr = spawn(recorder.router()).await;
    let upstream = json!({
        "server": {"endpoints": [{"scheme": "http", "host": addr.ip().to_string(), "port": addr.port()}]},
        "alias": format!("resp.{}", Uuid::new_v4().simple()),
        "headers": {"response": {"set": {"X-Got": "1"}, "remove": ["X-Drop"]}}
    });
    let alias = publish(&harness, upstream, route_for("placeholder", &["GET"], "/v1")).await;

    let response = get(
        harness.serve_once().await,
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.text());
    assert_eq!(response.header("x-got"), Some("1"));
    assert_eq!(response.header("x-drop"), None);
}

#[tokio::test]
async fn an_invalid_configured_header_value_is_rejected_at_create_time() {
    let harness = common::Harness::allowing_http();
    let upstream = json!({
        "server": {"endpoints": [{"scheme": "https", "host": "api.vendor.com", "port": 443}]},
        "alias": "crlf.example.com",
        "headers": {"request": {"set": {"X-Bad": "value\r\nInjected: yes"}}}
    });
    let response = post(harness.serve_once().await, "/oagw/v1/upstreams", upstream).await;
    assert_eq!(response.status, 400, "{}", response.text());
    assert_eq!(problem_type(&response), ids::ERR_VALIDATION);
}

// ---------------------------------------------------------------------------------------
// Body validation
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn a_mismatched_content_length_is_rejected() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let (_, alias) =
        serve_behind_gateway(&harness, recorder.router(), &["POST"], "/v1").await;

    let request = engine_request(&alias, &[("content-length", "5")], b"abc");
    let response = call_engine(&harness, request).await;
    assert_eq!(response.status, 400, "{}", response.text());
    assert_eq!(problem_type(&response), ids::ERR_VALIDATION);
    assert_eq!(recorder.count(), 0);
}

#[tokio::test]
async fn an_unsupported_transfer_encoding_is_rejected() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let (_, alias) =
        serve_behind_gateway(&harness, recorder.router(), &["POST"], "/v1").await;

    let request = engine_request(&alias, &[("transfer-encoding", "gzip")], b"{}");
    let response = call_engine(&harness, request).await;
    assert_eq!(response.status, 400, "{}", response.text());
    assert_eq!(problem_type(&response), ids::ERR_VALIDATION);
    assert_eq!(recorder.count(), 0);
}

#[tokio::test]
async fn a_chunked_transfer_encoding_is_accepted() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let (_, alias) =
        serve_behind_gateway(&harness, recorder.router(), &["POST"], "/v1").await;

    let request = engine_request(&alias, &[("transfer-encoding", "chunked")], b"{}");
    let response = call_engine(&harness, request).await;
    assert_eq!(response.status, 200, "{}", response.text());
    assert_eq!(recorder.count(), 1);
}

#[tokio::test]
async fn an_oversized_body_is_rejected() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let (_, alias) =
        serve_behind_gateway(&harness, recorder.router(), &["POST"], "/v1").await;

    let limit = oagw::config::MAX_BODY_BYTES;
    let request = oagw::infra::proxy::ProxiedRequest {
        body: bytes::Bytes::from(vec![0u8; limit + 1]),
        ..engine_request(&alias, &[], b"")
    };
    let response = call_engine(&harness, request).await;
    assert_eq!(response.status, 413, "{}", response.text());
    assert_eq!(problem_type(&response), ids::ERR_PAYLOAD_TOO_LARGE);
    assert_eq!(recorder.count(), 0);
}

// ---------------------------------------------------------------------------------------
// Query allowlist and path suffix mode
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn a_non_allowlisted_query_parameter_is_rejected() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let addr = spawn(recorder.router()).await;
    let upstream = aliased_upstream_body(
        &addr.ip().to_string(),
        addr.port(),
        "http",
        &format!("query.{}", Uuid::new_v4().simple()),
    );
    let (id, alias) = register_upstream(&harness, upstream).await;
    let mut route = route_for(&id, &["GET"], "/v1");
    route["match"]["http"]["query_allowlist"] = json!(["limit"]);
    register_route(&harness, &id, route).await;

    let rejected = get(
        harness.serve_once().await,
        &format!("/oagw/v1/proxy/{alias}/v1/models?foo=1"),
    )
    .await;
    assert_eq!(rejected.status, 400, "{}", rejected.text());
    assert_eq!(problem_type(&rejected), ids::ERR_VALIDATION);
    assert_eq!(recorder.count(), 0);

    let allowed = get(
        harness.serve_once().await,
        &format!("/oagw/v1/proxy/{alias}/v1/models?limit=2"),
    )
    .await;
    assert_eq!(allowed.status, 200, "{}", allowed.text());
    assert_eq!(recorder.first().expect("called").query, "limit=2");
}

#[tokio::test]
async fn an_empty_allowlist_admits_no_query_parameters() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let (_, alias) = serve_behind_gateway(&harness, recorder.router(), &["GET"], "/v1").await;

    let response = get(
        harness.serve_once().await,
        &format!("/oagw/v1/proxy/{alias}/v1/models?limit=2"),
    )
    .await;
    assert_eq!(response.status, 400, "{}", response.text());
    assert_eq!(recorder.count(), 0);
}

#[tokio::test]
async fn path_suffix_mode_disabled_rejects_a_suffix() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let addr = spawn(recorder.router()).await;
    let upstream = aliased_upstream_body(
        &addr.ip().to_string(),
        addr.port(),
        "http",
        &format!("exact.{}", Uuid::new_v4().simple()),
    );
    let (id, alias) = register_upstream(&harness, upstream).await;
    let mut route = route_for(&id, &["GET"], "/v1/only");
    route["match"]["http"]["path_suffix_mode"] = json!("disabled");
    register_route(&harness, &id, route).await;

    let rejected = get(
        harness.serve_once().await,
        &format!("/oagw/v1/proxy/{alias}/v1/only/extra"),
    )
    .await;
    assert_eq!(rejected.status, 400, "{}", rejected.text());
    assert_eq!(problem_type(&rejected), ids::ERR_VALIDATION);
    assert_eq!(recorder.count(), 0);

    let exact = get(
        harness.serve_once().await,
        &format!("/oagw/v1/proxy/{alias}/v1/only"),
    )
    .await;
    assert_eq!(exact.status, 200, "{}", exact.text());
}

// ---------------------------------------------------------------------------------------
// CORS
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn preflight_returns_204_with_echoed_headers() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let (_, alias) = serve_behind_gateway(&harness, recorder.router(), &["GET"], "/v1").await;

    let response = common::request(
        harness.serve_once().await,
        "OPTIONS",
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
        &[
            ("Origin", "https://app.example.com"),
            ("Access-Control-Request-Method", "POST"),
            ("Access-Control-Request-Headers", "Content-Type"),
        ],
        Vec::new(),
    )
    .await;
    assert_eq!(response.status, 204, "{}", response.text());
    assert_eq!(
        response.header("access-control-allow-origin"),
        Some("https://app.example.com")
    );
    assert_eq!(response.header("access-control-allow-methods"), Some("POST"));
    assert_eq!(
        response.header("access-control-allow-headers"),
        Some("Content-Type")
    );
    assert_eq!(response.header("access-control-max-age"), Some("600"));
    let vary = response.header("vary").unwrap_or_default();
    assert!(vary.contains("Origin"), "{vary}");
    assert_eq!(recorder.count(), 0, "a preflight never reaches the upstream");
}

#[tokio::test]
async fn preflight_does_not_require_a_matching_route() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let (_, alias) = serve_behind_gateway(&harness, recorder.router(), &["GET"], "/v1").await;

    let response = common::request(
        harness.serve_once().await,
        "OPTIONS",
        &format!("/oagw/v1/proxy/{alias}/unrouted/path"),
        &[
            ("Origin", "https://app.example.com"),
            ("Access-Control-Request-Method", "POST"),
        ],
        Vec::new(),
    )
    .await;
    assert_eq!(response.status, 204, "{}", response.text());
}

/// Publishes a CORS-enabled upstream over `recorder`.
async fn cors_upstream(harness: &Arc<common::Harness>, recorder: &Recorder, cors: Value) -> String {
    let addr = spawn(recorder.router()).await;
    let upstream = json!({
        "server": {"endpoints": [{"scheme": "http", "host": addr.ip().to_string(), "port": addr.port()}]},
        "alias": format!("cors.{}", Uuid::new_v4().simple()),
        "cors": cors
    });
    publish(harness, upstream, route_for("placeholder", &["GET"], "/v1")).await
}

#[tokio::test]
async fn a_disallowed_origin_is_rejected_on_an_actual_request() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let alias = cors_upstream(
        &harness,
        &recorder,
        json!({"enabled": true, "allowed_origins": ["https://app.example.com"]}),
    )
    .await;

    let response = common::request(
        harness.serve_once().await,
        "GET",
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
        &[("Origin", "https://evil.com")],
        Vec::new(),
    )
    .await;
    assert_eq!(response.status, 403, "{}", response.text());
    assert_eq!(problem_type(&response), ids::ERR_CORS_ORIGIN);
    assert_eq!(recorder.count(), 0);
}

#[tokio::test]
async fn a_disallowed_method_is_rejected_on_an_actual_request() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let alias = cors_upstream(
        &harness,
        &recorder,
        json!({"enabled": true, "allowed_origins": ["https://app.example.com"], "allowed_methods": ["GET"]}),
    )
    .await;
    let route = harness
        .control_plane
        .list_routes(harness.tenant_id)
        .await
        .into_iter()
        .next()
        .expect("the created route");
    let mut replaced = route_body(&route.upstream_id, &["GET", "DELETE"], "/v1");
    replaced["enabled"] = json!(true);
    let replaced = common::put(
        harness.serve_once().await,
        &format!("/oagw/v1/routes/{}", route.id.unwrap_or_default()),
        replaced,
    )
    .await;
    assert_eq!(replaced.status, 200, "{}", replaced.text());

    let response = common::request(
        harness.serve_once().await,
        "DELETE",
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
        &[("Origin", "https://app.example.com")],
        Vec::new(),
    )
    .await;
    assert_eq!(response.status, 403, "{}", response.text());
    assert_eq!(problem_type(&response), ids::ERR_CORS_METHOD);
}

#[tokio::test]
async fn an_allowed_origin_receives_cors_response_headers() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let alias = cors_upstream(
        &harness,
        &recorder,
        json!({"enabled": true, "allowed_origins": ["https://app.example.com"], "expose_headers": ["X-Request-ID"]}),
    )
    .await;

    let response = common::request(
        harness.serve_once().await,
        "GET",
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
        &[("Origin", "https://app.example.com")],
        Vec::new(),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.text());
    assert_eq!(
        response.header("access-control-allow-origin"),
        Some("https://app.example.com")
    );
    assert_eq!(
        response.header("access-control-expose-headers"),
        Some("X-Request-ID")
    );
    let vary = response.header("vary").unwrap_or_default();
    assert!(vary.contains("Origin"), "{vary}");
}

#[tokio::test]
async fn a_non_cross_origin_request_is_unaffected_by_cors() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let alias = cors_upstream(
        &harness,
        &recorder,
        json!({"enabled": true, "allowed_origins": ["https://app.example.com"]}),
    )
    .await;

    let response = get(
        harness.serve_once().await,
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.text());
    assert_eq!(response.header("access-control-allow-origin"), None);
}

#[tokio::test]
async fn cors_is_disabled_by_default() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let (_, alias) = serve_behind_gateway(&harness, recorder.router(), &["GET"], "/v1").await;

    let response = common::request(
        harness.serve_once().await,
        "GET",
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
        &[("Origin", "https://app.example.com")],
        Vec::new(),
    )
    .await;
    assert_eq!(response.status, 403, "{}", response.text());
    assert_eq!(problem_type(&response), ids::ERR_CORS_ORIGIN);
}

// ---------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn a_gateway_error_body_is_problem_json_with_the_documented_fields() {
    let harness = common::Harness::allowing_http();
    let response = get(
        harness.serve_once().await,
        "/oagw/v1/proxy/no-such-alias/v1/models",
    )
    .await;
    assert_eq!(response.header("content-type"), Some("application/problem+json"));
    assert_eq!(response.header("X-OAGW-Error-Source"), Some("gateway"));
    let body = response.json();
    assert!(body["type"].as_str().is_some(), "{body}");
    assert!(body["title"].as_str().is_some());
    assert_eq!(body["status"], json!(404));
    assert!(body["detail"].as_str().is_some());
    assert_eq!(
        body["instance"],
        json!("/oagw/v1/proxy/no-such-alias/v1/models")
    );
}

#[tokio::test]
async fn an_upstream_5xx_is_passed_through_unchanged() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let router = recorder.responding_with(
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        "application/json",
        "{\"err\":\"boom\"}",
    );
    let (_, alias) = serve_behind_gateway(&harness, router, &["GET"], "/v1").await;

    let response = get(
        harness.serve_once().await,
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
    )
    .await;
    assert_eq!(response.status, 500, "{}", response.text());
    assert_eq!(response.json(), json!({"err": "boom"}));
    assert_eq!(response.header("content-type"), Some("application/json"));
    assert_eq!(response.header("X-OAGW-Error-Source"), Some("upstream"));
}

#[tokio::test]
async fn an_upstream_4xx_is_passed_through_unchanged() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let router =
        recorder.responding_with(axum::http::StatusCode::NOT_FOUND, "application/json", "{}");
    let (_, alias) = serve_behind_gateway(&harness, router, &["GET"], "/v1").await;

    let response = get(
        harness.serve_once().await,
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
    )
    .await;
    assert_eq!(response.status, 404);
    assert_eq!(response.header("X-OAGW-Error-Source"), Some("upstream"));
}

#[tokio::test]
async fn a_timeout_is_reported_as_504() {
    let harness = common::Harness::new(
        oagw::config::OagwConfig {
            proxy_timeout_secs: 1,
            allow_http_upstream: true,
            ..oagw::config::OagwConfig::default()
        },
        std::collections::HashMap::new(),
    );
    let slow = axum::Router::new().route(
        "/v1/models",
        axum::routing::get(|| async {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            axum::http::StatusCode::OK
        }),
    );
    let (_, alias) = serve_behind_gateway(&harness, slow, &["GET"], "/v1").await;

    let response = get(
        harness.serve_once().await,
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
    )
    .await;
    assert_eq!(response.status, 504, "{}", response.text());
    assert_eq!(problem_type(&response), ids::ERR_TIMEOUT);
    assert_eq!(response.header("X-OAGW-Error-Source"), Some("gateway"));
}

#[tokio::test]
async fn a_refused_upstream_connection_is_reported_as_502() {
    let harness = common::Harness::allowing_http();
    // A port with nothing listening on it.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let closed = listener.local_addr().expect("addr");
    drop(listener);

    let upstream = aliased_upstream_body(
        "127.0.0.1",
        closed.port(),
        "http",
        &format!("refused.{}", Uuid::new_v4().simple()),
    );
    let alias = publish(&harness, upstream, route_for("placeholder", &["GET"], "/v1")).await;

    let response = get(
        harness.serve_once().await,
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
    )
    .await;
    assert_eq!(response.status, 502, "{}", response.text());
    assert_eq!(problem_type(&response), ids::ERR_DOWNSTREAM);
    assert_eq!(response.header("X-OAGW-Error-Source"), Some("gateway"));
}

// ---------------------------------------------------------------------------------------
// Rate limiting
// ---------------------------------------------------------------------------------------

/// An upstream with a per-second limit of `rate`.
fn limited_upstream(host: &str, port: u16, rate: u32) -> Value {
    let mut body = aliased_upstream_body(
        host,
        port,
        "http",
        &format!("limited.{}", Uuid::new_v4().simple()),
    );
    body["rate_limit"] = json!({
        "sustained": {"rate": rate, "window": "second"},
        "burst": {"capacity": rate}
    });
    body
}

#[tokio::test]
async fn a_burst_up_to_capacity_is_allowed_and_beyond_is_rejected() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let addr = spawn(recorder.router()).await;
    let alias = publish(
        &harness,
        limited_upstream(&addr.ip().to_string(), addr.port(), 3),
        route_for("placeholder", &["GET"], "/v1"),
    )
    .await;

    for index in 0..3 {
        let response = get(
            harness.serve_once().await,
            &format!("/oagw/v1/proxy/{alias}/v1/models"),
        )
        .await;
        assert_eq!(response.status, 200, "request {index}: {}", response.text());
    }
    let rejected = get(
        harness.serve_once().await,
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
    )
    .await;
    assert_eq!(rejected.status, 429, "{}", rejected.text());
    assert_eq!(problem_type(&rejected), ids::ERR_RATE_LIMIT);
    assert_eq!(recorder.count(), 3, "rejected requests never reach the upstream");
}

#[tokio::test]
async fn a_429_carries_the_rate_limit_headers() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let addr = spawn(recorder.router()).await;
    let alias = publish(
        &harness,
        limited_upstream(&addr.ip().to_string(), addr.port(), 1),
        route_for("placeholder", &["GET"], "/v1"),
    )
    .await;

    assert_eq!(
        get(
            harness.serve_once().await,
            &format!("/oagw/v1/proxy/{alias}/v1/models")
        )
        .await
        .status,
        200
    );
    let rejected = get(
        harness.serve_once().await,
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
    )
    .await;
    assert_eq!(rejected.status, 429);
    assert_eq!(rejected.header("retry-after"), Some("1"));
    assert_eq!(rejected.header("x-ratelimit-limit"), Some("1"));
    assert_eq!(rejected.header("x-ratelimit-remaining"), Some("0"));
    assert!(rejected.header("x-ratelimit-reset").is_some());
    assert_eq!(rejected.header("content-type"), Some("application/problem+json"));
    assert_eq!(rejected.header("X-OAGW-Error-Source"), Some("gateway"));
}

/// Replaces the created route with `rate_limit` applied.
async fn with_route_limit(harness: &Arc<common::Harness>, rate_limit: Value) {
    let route = harness
        .control_plane
        .list_routes(harness.tenant_id)
        .await
        .into_iter()
        .find(|route| route.match_rules.http.as_ref().is_some_and(|http| http.path == "/v1"))
        .expect("the created route");
    let mut replaced = route_body(&route.upstream_id, &["GET"], "/v1");
    replaced["rate_limit"] = rate_limit;
    let response = common::put(
        harness.serve_once().await,
        &format!("/oagw/v1/routes/{}", route.id.unwrap_or_default()),
        replaced,
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.text());
}

#[tokio::test]
async fn a_stricter_route_limit_wins() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let addr = spawn(recorder.router()).await;
    let mut upstream = limited_upstream(&addr.ip().to_string(), addr.port(), 5);
    upstream["alias"] = json!(format!("route-strict.{}", Uuid::new_v4().simple()));
    let alias = publish(&harness, upstream, route_for("placeholder", &["GET"], "/v1")).await;
    with_route_limit(
        &harness,
        json!({"sustained": {"rate": 1, "window": "second"}, "burst": {"capacity": 1}}),
    )
    .await;

    assert_eq!(
        get(
            harness.serve_once().await,
            &format!("/oagw/v1/proxy/{alias}/v1/models")
        )
        .await
        .status,
        200
    );
    let rejected = get(
        harness.serve_once().await,
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
    )
    .await;
    assert_eq!(rejected.status, 429, "{}", rejected.text());
}

#[tokio::test]
async fn a_route_limit_cannot_exceed_the_upstream_limit() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let addr = spawn(recorder.router()).await;
    let mut upstream = limited_upstream(&addr.ip().to_string(), addr.port(), 1);
    upstream["alias"] = json!(format!("route-loose.{}", Uuid::new_v4().simple()));
    let alias = publish(&harness, upstream, route_for("placeholder", &["GET"], "/v1")).await;
    with_route_limit(&harness, json!({"sustained": {"rate": 100, "window": "second"}})).await;

    assert_eq!(
        get(
            harness.serve_once().await,
            &format!("/oagw/v1/proxy/{alias}/v1/models")
        )
        .await
        .status,
        200
    );
    let rejected = get(
        harness.serve_once().await,
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
    )
    .await;
    assert_eq!(rejected.status, 429, "{}", rejected.text());
    assert_eq!(rejected.header("x-ratelimit-limit"), Some("1"));
}

#[tokio::test]
async fn a_route_without_its_own_limit_inherits_the_upstream_limit() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let addr = spawn(recorder.router()).await;
    let alias = publish(
        &harness,
        limited_upstream(&addr.ip().to_string(), addr.port(), 2),
        route_for("placeholder", &["GET"], "/v1"),
    )
    .await;

    for _ in 0..2 {
        assert_eq!(
            get(
                harness.serve_once().await,
                &format!("/oagw/v1/proxy/{alias}/v1/models")
            )
            .await
            .status,
            200
        );
    }
    let rejected = get(
        harness.serve_once().await,
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
    )
    .await;
    assert_eq!(rejected.status, 429, "{}", rejected.text());
}

#[tokio::test]
async fn a_cost_consumes_multiple_tokens() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let addr = spawn(recorder.router()).await;
    let mut upstream = limited_upstream(&addr.ip().to_string(), addr.port(), 4);
    upstream["alias"] = json!(format!("cost.{}", Uuid::new_v4().simple()));
    let alias = publish(&harness, upstream, route_for("placeholder", &["GET"], "/v1")).await;
    with_route_limit(
        &harness,
        json!({"sustained": {"rate": 2, "window": "second"}, "cost": 3}),
    )
    .await;

    let rejected = get(
        harness.serve_once().await,
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
    )
    .await;
    assert_eq!(rejected.status, 429, "{}", rejected.text());
}

#[tokio::test]
async fn two_tenants_do_not_share_a_counter() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let addr = spawn(recorder.router()).await;
    let other = harness.with_tenant(Uuid::new_v4());
    let alias = format!("scoped.{}", Uuid::new_v4().simple());

    for owner in [&harness, &other] {
        let mut upstream = limited_upstream(&addr.ip().to_string(), addr.port(), 1);
        upstream["alias"] = json!(alias.clone());
        publish(owner, upstream, route_for("placeholder", &["GET"], "/v1")).await;
    }

    assert_eq!(
        get(
            harness.serve_once().await,
            &format!("/oagw/v1/proxy/{alias}/v1/models")
        )
        .await
        .status,
        200
    );
    let rejected = get(
        harness.serve_once().await,
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
    )
    .await;
    assert_eq!(rejected.status, 429, "{}", rejected.text());
    let response = get(
        other.serve_once().await,
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.text());
}

#[tokio::test]
async fn ip_scope_counts_per_client_address() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let addr = spawn(recorder.router()).await;
    let mut upstream = limited_upstream(&addr.ip().to_string(), addr.port(), 1);
    upstream["alias"] = json!(format!("ip-scope.{}", Uuid::new_v4().simple()));
    upstream["rate_limit"]["scope"] = json!("ip");
    let alias = publish(&harness, upstream, route_for("placeholder", &["GET"], "/v1")).await;

    let gateway = harness.serve_once().await;
    let path = format!("/oagw/v1/proxy/{alias}/v1/models");
    let as_client = common::request(gateway, "GET", &path, &[("X-Forwarded-For", "203.0.113.7")], Vec::new()).await;
    assert_eq!(as_client.status, 200);
    let other_client = common::request(gateway, "GET", &path, &[("X-Forwarded-For", "198.51.100.9")], Vec::new()).await;
    assert_eq!(other_client.status, 200);
    let again = common::request(gateway, "GET", &path, &[("X-Forwarded-For", "203.0.113.7")], Vec::new()).await;
    assert_eq!(again.status, 429);
}

// ---------------------------------------------------------------------------------------
// Enablement
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn a_disabled_upstream_rejects_proxy_requests() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let addr = spawn(recorder.router()).await;
    let mut body = upstream_body(&addr.ip().to_string(), addr.port(), "http");
    body["alias"] = json!(format!("enabled.{}", Uuid::new_v4().simple()));
    let alias = publish(&harness, body, route_for("placeholder", &["GET"], "/v1")).await;

    let id = harness
        .control_plane
        .list_upstreams(harness.tenant_id)
        .await
        .into_iter()
        .find(|upstream| upstream.alias.as_deref() == Some(alias.as_str()))
        .expect("the created upstream")
        .id
        .unwrap_or_default();
    let mut replaced = upstream_body(&addr.ip().to_string(), addr.port(), "http");
    replaced["alias"] = json!(alias);
    replaced["enabled"] = json!(false);
    let response = common::put(harness.serve_once().await, &format!("/oagw/v1/upstreams/{id}"), replaced).await;
    assert_eq!(response.status, 200, "{}", response.text());

    let rejected = get(
        harness.serve_once().await,
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
    )
    .await;
    assert_eq!(rejected.status, 503, "{}", rejected.text());
    assert_eq!(problem_type(&rejected), ids::ERR_LINK_UNAVAILABLE);
    assert_eq!(rejected.header("X-OAGW-Error-Source"), Some("gateway"));
    assert_eq!(rejected.header("content-type"), Some("application/problem+json"));
}

#[tokio::test]
async fn a_disabled_route_is_not_matched() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let addr = spawn(recorder.router()).await;
    let mut body = upstream_body(&addr.ip().to_string(), addr.port(), "http");
    body["alias"] = json!(format!("enabled.{}", Uuid::new_v4().simple()));
    let alias = publish(&harness, body, route_for("placeholder", &["GET"], "/v1")).await;

    let route = harness
        .control_plane
        .list_routes(harness.tenant_id)
        .await
        .into_iter()
        .find(|route| route.match_rules.http.as_ref().is_some_and(|http| http.path == "/v1"))
        .expect("the created route");
    let mut replaced = route_body(&route.upstream_id, &["GET"], "/v1");
    replaced["enabled"] = json!(false);
    let response = common::put(
        harness.serve_once().await,
        &format!("/oagw/v1/routes/{}", route.id.unwrap_or_default()),
        replaced,
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.text());

    let rejected = get(
        harness.serve_once().await,
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
    )
    .await;
    assert_eq!(rejected.status, 404, "{}", rejected.text());
    assert_eq!(problem_type(&rejected), ids::ERR_ROUTE_NOT_FOUND);
}

// ---------------------------------------------------------------------------------------
// Built-in plugins at runtime
// ---------------------------------------------------------------------------------------

/// Publishes an upstream over `recorder` with `extra` merged into its body.
async fn bound_upstream(harness: &Arc<common::Harness>, router: axum::Router, extra: Value) -> String {
    let addr = spawn(router).await;
    let mut upstream = json!({
        "server": {"endpoints": [{"scheme": "http", "host": addr.ip().to_string(), "port": addr.port()}]},
        "alias": format!("bound.{}", Uuid::new_v4().simple())
    });
    if let Some(members) = extra.as_object() {
        for (name, value) in members {
            upstream[name.as_str()] = value.clone();
        }
    }
    publish(harness, upstream, route_for("placeholder", &["GET"], "/v1")).await
}

#[tokio::test]
async fn the_noop_auth_plugin_leaves_the_request_untouched() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let alias = bound_upstream(
        &harness,
        recorder.router(),
        json!({"auth": {"type": format!("{AUTH_PLUGIN}cf.core.oagw.noop.v1")}}),
    )
    .await;

    let response = get(
        harness.serve_once().await,
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.text());
    let seen = recorder.first().expect("called");
    assert!(seen.has_no_header("authorization"));
    assert_eq!(seen.query, "");
}

#[tokio::test]
async fn the_apikey_auth_plugin_injects_a_header() {
    let harness = common::Harness::with_credentials();
    let recorder = Recorder::new();
    let alias = bound_upstream(
        &harness,
        recorder.router(),
        json!({"auth": {"type": format!("{AUTH_PLUGIN}cf.core.oagw.apikey.v1"),
            "config": {"header": "X-Api-Key", "secret_ref": "cred://test-key"}}}),
    )
    .await;

    let response = get(
        harness.serve_once().await,
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.text());
    assert_eq!(
        recorder.first().expect("called").header("x-api-key"),
        Some("super-secret")
    );
}

#[tokio::test]
async fn the_apikey_auth_plugin_can_inject_into_the_query_string() {
    let harness = common::Harness::with_credentials();
    let recorder = Recorder::new();
    let alias = bound_upstream(
        &harness,
        recorder.router(),
        json!({"auth": {"type": format!("{AUTH_PLUGIN}cf.core.oagw.apikey.v1"),
            "config": {"query": "api_key", "secret_ref": "cred://test-key"}}}),
    )
    .await;

    let response = get(
        harness.serve_once().await,
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.text());
    assert_eq!(
        recorder.first().expect("called").query,
        "api_key=super-secret"
    );
}

#[tokio::test]
async fn a_missing_secret_yields_500_secret_not_found() {
    let harness = common::Harness::with_credentials();
    let recorder = Recorder::new();
    let alias = bound_upstream(
        &harness,
        recorder.router(),
        json!({"auth": {"type": format!("{AUTH_PLUGIN}cf.core.oagw.apikey.v1"),
            "config": {"header": "X-Api-Key", "secret_ref": "cred://absent-key"}}}),
    )
    .await;

    let response = get(
        harness.serve_once().await,
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
    )
    .await;
    assert_eq!(response.status, 500, "{}", response.text());
    assert_eq!(problem_type(&response), ids::ERR_SECRET_NOT_FOUND);
    assert_eq!(recorder.count(), 0);
    assert!(
        !response.text().contains("super-secret"),
        "no secret material: {}",
        response.text()
    );
}

#[tokio::test]
async fn the_oauth2_plugin_injects_a_bearer_token_and_caches_it() {
    use std::sync::atomic::Ordering;

    let harness = common::Harness::with_credentials();
    let recorder = Recorder::new();
    let tokens = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&tokens);
    let token_server = axum::Router::new().route(
        "/token",
        axum::routing::post(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            async { axum::Json(json!({"access_token": "tok123", "expires_in": 3600})) }
        }),
    );
    let token_addr = spawn(token_server).await;

    let alias = bound_upstream(
        &harness,
        recorder.router(),
        json!({"auth": {"type": format!("{AUTH_PLUGIN}cf.core.oagw.oauth2_client_cred.v1"),
            "config": {
                "token_endpoint": format!("http://127.0.0.1:{}/token", token_addr.port()),
                "client_id_ref": "cred://client",
                "client_secret_ref": "cred://secret"
            }}}),
    )
    .await;

    for _ in 0..3 {
        let response = get(
            harness.serve_once().await,
            &format!("/oagw/v1/proxy/{alias}/v1/models"),
        )
        .await;
        assert_eq!(response.status, 200, "{}", response.text());
        assert_eq!(
            recorder.first().expect("called").header("authorization"),
            Some("Bearer tok123")
        );
    }
    assert_eq!(tokens.load(Ordering::SeqCst), 1, "the token is cached");
}

#[tokio::test]
async fn an_oauth2_token_fetch_failure_yields_401() {
    let harness = common::Harness::with_credentials();
    let recorder = Recorder::new();
    let token_server = axum::Router::new().route(
        "/token",
        axum::routing::post(|| async {
            (
                axum::http::StatusCode::UNAUTHORIZED,
                axum::Json(json!({"error": "invalid_client"})),
            )
        }),
    );
    let token_addr = spawn(token_server).await;

    let alias = bound_upstream(
        &harness,
        recorder.router(),
        json!({"auth": {"type": format!("{AUTH_PLUGIN}cf.core.oagw.oauth2_client_cred.v1"),
            "config": {
                "token_endpoint": format!("http://127.0.0.1:{}/token", token_addr.port()),
                "client_id_ref": "cred://client",
                "client_secret_ref": "cred://secret"
            }}}),
    )
    .await;

    let response = get(
        harness.serve_once().await,
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
    )
    .await;
    assert_eq!(response.status, 401, "{}", response.text());
    assert_eq!(problem_type(&response), ids::ERR_AUTH_FAILED);
    assert_eq!(recorder.count(), 0);
}

#[tokio::test]
async fn a_missing_required_request_header_is_rejected_with_400() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let alias = bound_upstream(
        &harness,
        recorder.router(),
        json!({"plugins": {"items": [{"plugin_ref": format!("{GUARD_PLUGIN}cf.core.oagw.required_headers.v1"),
            "config": {"required_request_headers": "x-correlation-id,accept"}}]}}),
    )
    .await;

    let response = get(
        harness.serve_once().await,
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
    )
    .await;
    assert_eq!(response.status, 400, "{}", response.text());
    assert_eq!(response.json()["error_code"], json!("REQUIRED_HEADER_MISSING"));
    assert_eq!(recorder.count(), 0, "the request is not forwarded");

    let present = common::request(
        harness.serve_once().await,
        "GET",
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
        &[("X-Correlation-Id", "c1"), ("Accept", "application/json")],
        Vec::new(),
    )
    .await;
    assert_eq!(present.status, 200, "{}", present.text());
    assert_eq!(recorder.count(), 1, "present required headers pass");
}

#[tokio::test]
async fn a_missing_required_response_header_is_rejected_with_502() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let router = recorder.responding_with(axum::http::StatusCode::OK, "text/plain", "hi");
    let alias = bound_upstream(
        &harness,
        router,
        json!({"plugins": {"items": [{"plugin_ref": format!("{GUARD_PLUGIN}cf.core.oagw.required_headers.v1"),
            "config": {"required_response_headers": "x-must-be-present"}}]}}),
    )
    .await;

    let response = get(
        harness.serve_once().await,
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
    )
    .await;
    assert_eq!(response.status, 502, "{}", response.text());
    assert_eq!(response.json()["error_code"], json!("REQUIRED_HEADER_MISSING"));
}

#[tokio::test]
async fn blank_guard_configuration_fails_open() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let alias = bound_upstream(
        &harness,
        recorder.router(),
        json!({"plugins": {"items": [{"plugin_ref": format!("{GUARD_PLUGIN}cf.core.oagw.required_headers.v1"),
            "config": {"required_request_headers": " , , "}}]}}),
    )
    .await;

    let response = get(
        harness.serve_once().await,
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.text());
}

#[tokio::test]
async fn an_unconfigured_guard_fails_open() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let alias = bound_upstream(
        &harness,
        recorder.router(),
        json!({"plugins": {"items": [{"plugin_ref": format!("{GUARD_PLUGIN}cf.core.oagw.required_headers.v1")}]}}),
    )
    .await;

    let response = get(
        harness.serve_once().await,
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.text());
}

#[tokio::test]
async fn upstream_plugins_run_before_route_plugins() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let addr = spawn(recorder.router()).await;
    let upstream = json!({
        "server": {"endpoints": [{"scheme": "http", "host": addr.ip().to_string(), "port": addr.port()}]},
        "alias": format!("chain.{}", Uuid::new_v4().simple()),
        // The upstream's transform writes the header the route's guard checks,
        // so the guard only passes if the upstream chain ran first.
        "plugins": {"items": [{"plugin_ref": format!("{TRANSFORM_PLUGIN}cf.core.oagw.request_id.v1")}]}
    });
    let (id, alias) = register_upstream(&harness, upstream).await;
    let mut route = route_for(&id, &["GET"], "/v1");
    route["plugins"] = json!({"items": [{
        "plugin_ref": format!("{GUARD_PLUGIN}cf.core.oagw.required_headers.v1"),
        "config": {"required_request_headers": "x-request-id"}
    }]});
    register_route(&harness, &id, route).await;

    let response = get(
        harness.serve_once().await,
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.text());
    assert_eq!(
        response.header("x-request-id"),
        recorder.first().expect("called").header("x-request-id"),
        "the id the transform wrote is the id the guard admitted"
    );
}

#[tokio::test]
async fn auth_runs_before_the_plugin_chain() {
    let harness = common::Harness::with_credentials();
    let recorder = Recorder::new();
    let alias = bound_upstream(
        &harness,
        recorder.router(),
        json!({
            "auth": {"type": format!("{AUTH_PLUGIN}cf.core.oagw.apikey.v1"),
                     "config": {"header": "X-Api-Key", "secret_ref": "cred://test-key"}},
            "plugins": {"items": [{"plugin_ref": format!("{TRANSFORM_PLUGIN}cf.core.oagw.request_id.v1")}]}
        }),
    )
    .await;

    let response = common::request(
        harness.serve_once().await,
        "GET",
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
        &[("X-Request-ID", "abc")],
        Vec::new(),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.text());
    let seen = recorder.first().expect("called");
    assert_eq!(seen.header("x-request-id"), Some("abc"));
    assert_eq!(seen.header("x-api-key"), Some("super-secret"));
    assert_eq!(response.header("x-request-id"), Some("abc"));
}

#[tokio::test]
async fn an_inbound_request_id_is_propagated() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let alias = bound_upstream(
        &harness,
        recorder.router(),
        json!({"plugins": {"items": [{"plugin_ref": format!("{TRANSFORM_PLUGIN}cf.core.oagw.request_id.v1")}]}}),
    )
    .await;

    let response = common::request(
        harness.serve_once().await,
        "GET",
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
        &[("X-Request-ID", "abc")],
        Vec::new(),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.text());
    assert_eq!(
        recorder.first().expect("called").header("x-request-id"),
        Some("abc")
    );
    assert_eq!(response.header("x-request-id"), Some("abc"));
}

#[tokio::test]
async fn a_generated_request_id_is_used() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let alias = bound_upstream(
        &harness,
        recorder.router(),
        json!({"plugins": {"items": [{"plugin_ref": format!("{TRANSFORM_PLUGIN}cf.core.oagw.request_id.v1")}]}}),
    )
    .await;

    let response = get(
        harness.serve_once().await,
        &format!("/oagw/v1/proxy/{alias}/v1/models"),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.text());
    let generated = recorder
        .first()
        .expect("called")
        .header("x-request-id")
        .unwrap_or_default()
        .to_owned();
    assert!(!generated.is_empty());
    assert_eq!(response.header("x-request-id"), Some(generated.as_str()));
}

// ---------------------------------------------------------------------------------------
// Streaming, upgrades and idempotence
// ---------------------------------------------------------------------------------------

/// Publishes the two-event SSE mock behind a `/v1/events` route.
async fn streaming_upstream(harness: &Arc<common::Harness>, active: Arc<AtomicUsize>) -> String {
    let addr = spawn(common::sse_router(active)).await;
    let upstream = json!({
        "server": {"endpoints": [{"scheme": "http", "host": addr.ip().to_string(), "port": addr.port()}]},
        "alias": format!("sse.{}", Uuid::new_v4().simple())
    });
    let (id, alias) = register_upstream(harness, upstream).await;
    register_route(harness, &id, route_for(&id, &["GET"], "/v1/events")).await;
    alias
}

#[tokio::test]
async fn sse_events_are_streamed() {
    let harness = common::Harness::allowing_http();
    let alias = streaming_upstream(&harness, Arc::new(AtomicUsize::new(0))).await;

    let mut stream = common::request_stream(
        harness.serve_once().await,
        "GET",
        &format!("/oagw/v1/proxy/{alias}/v1/events"),
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(stream.status, 200, "{:?}", stream.headers);
    assert_eq!(stream.header("content-type"), Some("text/event-stream"));

    let started = std::time::Instant::now();
    let first = stream.next_chunk().await.expect("the first event");
    assert!(
        started.elapsed() < std::time::Duration::from_millis(110),
        "the first event must arrive before the second is emitted"
    );
    assert!(String::from_utf8_lossy(&first).contains("one"));
    let second = stream.next_chunk().await.expect("the second event");
    assert!(String::from_utf8_lossy(&second).contains("two"));
    assert!(
        stream.next_chunk().await.is_none(),
        "the stream closes with the upstream"
    );
}

#[tokio::test]
async fn a_client_disconnect_closes_the_upstream_connection() {
    use std::sync::atomic::Ordering;

    let harness = common::Harness::allowing_http();
    let active = Arc::new(AtomicUsize::new(0));
    let alias = streaming_upstream(&harness, Arc::clone(&active)).await;

    let mut stream = common::request_stream(
        harness.serve_once().await,
        "GET",
        &format!("/oagw/v1/proxy/{alias}/v1/events"),
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(stream.status, 200);
    let first = stream.next_chunk().await.expect("the first event");
    assert!(String::from_utf8_lossy(&first).contains("one"));
    drop(stream);

    for _ in 0..200 {
        if active.load(Ordering::SeqCst) == 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("the upstream connection was never closed");
}

/// Publishes the echoing `WebSocket` mock behind a `/v1/ws` route.
async fn ws_upstream(harness: &Arc<common::Harness>) -> String {
    let addr = spawn(common::echo_ws_router()).await;
    let upstream = json!({
        "server": {"endpoints": [{"scheme": "http", "host": addr.ip().to_string(), "port": addr.port()}]},
        "alias": format!("ws.{}", Uuid::new_v4().simple())
    });
    let (id, alias) = register_upstream(harness, upstream).await;
    register_route(harness, &id, route_for(&id, &["GET"], "/v1/ws")).await;
    alias
}

#[tokio::test]
async fn a_websocket_upgrade_round_trips() {
    let harness = common::Harness::allowing_http();
    let alias = ws_upstream(&harness).await;

    let (status, headers, mut socket) = common::upgrade(
        harness.serve_once().await,
        &format!("/oagw/v1/proxy/{alias}/v1/ws"),
        &[],
    )
    .await
    .expect("the upgrade succeeds");
    assert_eq!(status, 101);
    assert!(
        headers
            .iter()
            .any(|(name, value)| name == "upgrade" && value == "websocket"),
        "{headers:?}"
    );

    common::ws_send(&mut socket, b"hello gateway").await;
    assert_eq!(common::ws_recv(&mut socket).await, b"hello gateway");
}

#[tokio::test]
async fn an_upstream_that_refuses_the_upgrade_produces_an_error() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let (_, alias) = serve_behind_gateway(&harness, recorder.router(), &["GET"], "/v1/ws").await;

    let outcome = common::upgrade(
        harness.serve_once().await,
        &format!("/oagw/v1/proxy/{alias}/v1/ws"),
        &[],
    )
    .await;
    match outcome {
        Err(response) => {
            assert_eq!(response.status, 502, "{}", response.text());
            assert_eq!(response.header("X-OAGW-Error-Source"), Some("gateway"));
        }
        Ok((status, _, _)) => panic!("expected a refusal, got status {status}"),
    }
}

#[tokio::test]
async fn two_identical_requests_both_reach_the_upstream() {
    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let (_, alias) = serve_behind_gateway(&harness, recorder.router(), &["GET"], "/v1").await;

    for _ in 0..2 {
        let response = get(
            harness.serve_once().await,
            &format!("/oagw/v1/proxy/{alias}/v1/models"),
        )
        .await;
        assert_eq!(response.status, 200, "{}", response.text());
    }
    assert_eq!(recorder.count(), 2, "the proxy never caches");
}

// ---------------------------------------------------------------------------------------
// Transport error classification
// ---------------------------------------------------------------------------------------

#[test]
fn a_connect_timeout_is_distinguished_from_a_plain_connect_failure() {
    // `hyper_util`'s legacy client produces a chain of the shape client error →
    // connector error → `io::Error`, so the timeout is never the top level.
    #[derive(Debug)]
    struct Wrapper(Box<dyn std::error::Error + Send + Sync>);

    impl std::fmt::Display for Wrapper {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "tcp connect error")
        }
    }

    impl std::error::Error for Wrapper {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(self.0.as_ref())
        }
    }

    fn wrap(error: std::io::Error) -> Box<dyn std::error::Error + Send + Sync> {
        Box::new(Wrapper(Box::new(error)))
    }

    let timed_out = std::io::Error::new(std::io::ErrorKind::TimedOut, "connect timed out");
    let legacy = std::io::Error::other(wrap(timed_out));
    assert!(
        oagw::infra::proxy::is_connect_timeout(&legacy),
        "a TimedOut io error in the cause chain is a connect timeout"
    );

    let refused = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
    let legacy_refused = std::io::Error::other(wrap(refused));
    assert!(
        !oagw::infra::proxy::is_connect_timeout(&legacy_refused),
        "a refused connection is not a timeout"
    );

    let direct = std::io::Error::other("nope");
    assert!(
        !oagw::infra::proxy::is_connect_timeout(&direct),
        "an unrelated error is not a connect timeout"
    );
}

// ---------------------------------------------------------------------------------------
// Logging
// ---------------------------------------------------------------------------------------

mod logging {
    use std::sync::Arc;
    use std::sync::Mutex;

    /// A `tracing` layer capturing the proxy access-log line.
    #[derive(Clone, Default)]
    pub struct LogCapture(pub Arc<Mutex<Vec<String>>>);

    impl LogCapture {
        /// The recorded lines.
        #[must_use]
        pub fn lines(&self) -> Vec<String> {
            self.0.lock().expect("lock the capture").clone()
        }
    }

    impl<S> tracing_subscriber::Layer<S> for LogCapture
    where
        S: tracing::Subscriber,
    {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut visitor = StringVisitor::default();
            event.record(&mut visitor);
            if event.metadata().target() == "oagw::proxy" {
                self.0.lock().expect("lock the capture").push(visitor.0);
            }
        }
    }

    /// Collects an event's fields into one string.
    #[derive(Default)]
    struct StringVisitor(String);

    #[allow(clippy::use_debug)] // the line renders arbitrary field values
    impl tracing::field::Visit for StringVisitor {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            use std::fmt::Write as _;

            write!(self.0, "{}={value:?} ", field.name()).expect("write a field");
        }
    }
}

#[tokio::test]
async fn a_proxied_request_is_logged_with_its_fields() {
    use tracing_subscriber::prelude::*;

    let harness = common::Harness::allowing_http();
    let recorder = Recorder::new();
    let (_, alias) = serve_behind_gateway(&harness, recorder.router(), &["GET"], "/v1").await;

    // A scoped dispatcher is not reliably visible to a served task, so the
    // capture is installed globally; this is the only test that installs one.
    let capture = logging::LogCapture::default();
    let _installed = tracing::subscriber::set_global_default(
        tracing_subscriber::registry().with(capture.clone()),
    );


    let mut request = engine_request(&alias, &[("x-request-id", "corr-1")], b"");
    request.method = axum::http::Method::GET;
    let response = call_engine(&harness, request).await;
    assert_eq!(response.status, 200, "{}", response.text());

    let lines = capture.lines();
    let line = lines
        .into_iter()
        .find(|entry| entry.contains(&format!("alias={alias}")))
        .unwrap_or_default();
    assert!(line.contains("proxied request"), "{line}");
    assert!(line.contains("path=/v1/models"), "{line}");
    assert!(line.contains("method=GET"), "{line}");
    assert!(line.contains("status=200"), "{line}");
    assert!(line.contains("duration_ms="), "{line}");
    assert!(line.contains("correlation_id=\"corr-1\""), "the correlation id is logged: {line}");
    assert!(
        !line.contains("authorization"),
        "no header material is logged: {line}"
    );
}
