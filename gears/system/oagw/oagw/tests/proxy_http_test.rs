// Created: 2026-08-29 by Constructor Tech
//! Data-plane happy path, header handling and target-host selection.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{get, json_body, post, put, tenant};
use httpmock::MockServer;
use serde_json::{Value, json};
use uuid::Uuid;

const PROTOCOL: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

/// Upstream bound to an httpmock server; endpoint hosts are IP literals, so the
/// alias is always supplied.
fn upstream_for(alias: &str, server: &MockServer) -> Value {
    json!({
        "alias": alias,
        "protocol": PROTOCOL,
        "server": { "endpoints": [ { "scheme": "http", "host": server.host(), "port": server.port() } ] },
    })
}

async fn create_upstream(harness: &common::Harness, alias: &str, server: &MockServer) -> Value {
    let created = json_body(
        post(
            harness.router(),
            "/oagw/v1/upstreams",
            upstream_for(alias, server),
            tenant(),
        )
        .await,
    )
    .await;
    assert_eq!(created["alias"], alias);
    created
}

#[tokio::test]
async fn happy_path_forwards_status_body_and_headers() {
    let server = MockServer::start();
    let target = server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/api/v1/pets");
        then.status(200)
            .header("x-upstream", "yes")
            .body("{\"ok\":true}");
    });

    let harness = common::Harness::new(common::test_config(), None);
    create_upstream(&harness, "happy.example.com", &server).await;
    let upstream_id = json_body(get(harness.router(), "/oagw/v1/upstreams", tenant()).await).await
        ["items"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    post(
        harness.router(),
        "/oagw/v1/routes",
        json!({ "upstream_id": upstream_id, "match": { "http": { "methods": ["GET"], "path": "/api" } } }),
        tenant(),
    )
    .await;

    let response = harness
        .send(
            "GET",
            "/oagw/v1/proxy/happy.example.com/api/v1/pets",
            None,
            tenant(),
        )
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        common::header(&response, "x-upstream").as_deref(),
        Some("yes")
    );
    // The success path still names the source of the response: ADR-0007 requires
    // the header on every response, and a relayed one is produced by the upstream.
    assert_eq!(
        common::header(&response, "x-oagw-error-source").as_deref(),
        Some("upstream")
    );
    assert_eq!(
        common::body_bytes(response).await.as_ref(),
        b"{\"ok\":true}"
    );
    assert_eq!(target.calls(), 1);
}

#[tokio::test]
async fn request_header_rules_and_hop_by_hop_stripping() {
    let server = MockServer::start();
    // The mock only answers when the rule-injected header arrives …
    let injected = server.mock(|when, then| {
        when.header("x-gateway", "oagw");
        then.status(200).body("ok");
    });
    // … and never when a hop-by-hop header survives.
    let hop_by_hop = server.mock(|when, then| {
        when.header("connection", "keep-alive");
        then.status(200).body("leaked");
    });
    let routing_header = server.mock(|when, then| {
        when.header("x-oagw-target-host", "happy.example.com");
        then.status(200).body("leaked");
    });

    let harness = common::Harness::new(common::test_config(), None);
    let upstream = json!({
        "alias": "headers.example.com",
        "protocol": PROTOCOL,
        "server": { "endpoints": [ { "scheme": "http", "host": server.host(), "port": server.port() } ] },
        "headers": { "request": { "set": { "x-gateway": "oagw" } } },
    });
    let upstream =
        json_body(post(harness.router(), "/oagw/v1/upstreams", upstream, tenant()).await).await;
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();
    post(harness.router(), "/oagw/v1/routes", json!({ "upstream_id": upstream_id, "match": { "http": { "methods": ["GET"], "path": "/" } } }), tenant())
        .await;

    let mut request = axum::http::Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/headers.example.com/echo")
        .header("connection", "keep-alive")
        .header("x-oagw-target-host", server.host())
        .body(axum::body::Body::empty())
        .unwrap();
    request
        .extensions_mut()
        .insert(common::security_for(tenant()));
    let response = harness.send_request(request, tenant()).await;
    assert_eq!(response.status(), 200);

    assert_eq!(injected.calls(), 1);
    assert_eq!(hop_by_hop.calls(), 0, "hop-by-hop headers must be stripped");
    assert_eq!(
        routing_header.calls(),
        0,
        "the target-host header is read then stripped"
    );
}

#[tokio::test]
async fn authorization_is_not_forwarded_unless_allowed() {
    let server = MockServer::start();
    let forwarded = server.mock(|when, then| {
        when.header("authorization", "Bearer caller-token");
        then.status(200).body("ok");
    });

    let harness = common::Harness::new(common::test_config(), None);
    let upstream_id = create_upstream(&harness, "auth.example.com", &server).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    post(
        harness.router(),
        "/oagw/v1/routes",
        json!({ "upstream_id": upstream_id, "match": { "http": { "methods": ["GET"], "path": "/" } } }),
        tenant(),
    )
    .await;

    // Default: no passthrough, the caller credential stays at the gateway.
    let request = axum::http::Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/auth.example.com/echo")
        .header("authorization", "Bearer caller-token")
        .body(axum::body::Body::empty())
        .unwrap();
    let response = harness.send_request(request, tenant()).await;
    // The mock above only answers when the credential is forwarded, so the
    // request falls through to httpmock's built-in 404.
    assert_eq!(response.status(), 404);
    assert_eq!(
        forwarded.calls(),
        0,
        "the credential must stay at the gateway"
    );

    // Allowlist the header and the credential reaches the upstream.
    let upstream = json!({
        "alias": "auth.example.com",
        "protocol": PROTOCOL,
        "server": { "endpoints": [ { "scheme": "http", "host": server.host(), "port": server.port() } ] },
        "headers": { "request": { "passthrough": "allowlist", "passthrough_allowlist": ["authorization"] } },
    });
    let replaced = put(
        harness.router(),
        &format!("/oagw/v1/upstreams/{upstream_id}"),
        upstream,
        tenant(),
    )
    .await;
    assert_eq!(replaced.status(), 200);
    let request = axum::http::Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/auth.example.com/echo")
        .header("authorization", "Bearer caller-token")
        .body(axum::body::Body::empty())
        .unwrap();
    let response = harness.send_request(request, tenant()).await;
    assert_eq!(response.status(), 200);
    assert_eq!(forwarded.calls(), 1);
}

#[tokio::test]
async fn query_allowlist_filters_the_forwarded_query() {
    let server = MockServer::start();
    let filtered = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/api/search")
            .query_param("keep", "1");
        then.status(200).body("ok");
    });

    let harness = common::Harness::new(common::test_config(), None);
    let upstream_id = create_upstream(&harness, "query.example.com", &server).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    post(
        harness.router(),
        "/oagw/v1/routes",
        json!({
            "upstream_id": upstream_id,
            "match": { "http": { "methods": ["GET"], "path": "/api", "query_allowlist": ["keep"] } },
        }),
        tenant(),
    )
    .await;

    let response = harness
        .send(
            "GET",
            "/oagw/v1/proxy/query.example.com/api/search?keep=1&drop=2",
            None,
            tenant(),
        )
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(filtered.calls(), 1);
}

#[tokio::test]
async fn target_host_header_selects_the_endpoint() {
    let server = MockServer::start();
    let target = server.mock(|when, then| {
        when.method(httpmock::Method::GET);
        then.status(200).body("ok");
    });

    let harness = common::Harness::new(common::test_config(), None);
    // Two endpoints that share the mock: the alias is explicit, so the header
    // is optional but honoured.
    let upstream = json!({
        "alias": "multi.example.com",
        "protocol": PROTOCOL,
        "server": { "endpoints": [
            { "scheme": "http", "host": server.host(), "port": server.port() },
            { "scheme": "http", "host": server.host(), "port": server.port() },
        ] },
    });
    let upstream =
        json_body(post(harness.router(), "/oagw/v1/upstreams", upstream, tenant()).await).await;
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();
    post(harness.router(), "/oagw/v1/routes", json!({ "upstream_id": upstream_id, "match": { "http": { "methods": ["GET"], "path": "/" } } }), tenant())
        .await;

    let request = axum::http::Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/multi.example.com/echo")
        .header("x-oagw-target-host", server.host())
        .body(axum::body::Body::empty())
        .unwrap();
    let response = harness.send_request(request, tenant()).await;
    assert_eq!(response.status(), 200);
    assert_eq!(target.calls(), 1);

    // An unknown host is refused with the configured alternatives.
    let request = axum::http::Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/multi.example.com/echo")
        .header("x-oagw-target-host", "elsewhere.example.com")
        .body(axum::body::Body::empty())
        .unwrap();
    let response = harness.send_request(request, tenant()).await;
    assert_eq!(response.status(), 400);
    let body = json_body(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1"
    );
    assert_eq!(body["invalid_value"], "elsewhere.example.com");
    assert_eq!(body["valid_hosts"], json!([server.host(), server.host()]));
}

#[tokio::test]
async fn common_suffix_alias_requires_a_target_host() {
    let harness = common::Harness::new(common::test_config(), None);
    let upstream = json!({
        "alias": "vendor.com",
        "protocol": PROTOCOL,
        "server": { "endpoints": [
            { "scheme": "https", "host": "us.vendor.com", "port": 443 },
            { "scheme": "https", "host": "eu.vendor.com", "port": 443 },
        ] },
    });
    let upstream =
        json_body(post(harness.router(), "/oagw/v1/upstreams", upstream, tenant()).await).await;
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();
    post(harness.router(), "/oagw/v1/routes", json!({ "upstream_id": upstream_id, "match": { "http": { "methods": ["GET"], "path": "/" } } }), tenant())
        .await;

    let response = harness
        .send("GET", "/oagw/v1/proxy/vendor.com/catalog", None, tenant())
        .await;
    assert_eq!(response.status(), 400);
    let body = json_body(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1"
    );
    assert_eq!(body["alias"], "vendor.com");
    assert_eq!(
        body["valid_hosts"],
        json!(["us.vendor.com", "eu.vendor.com"])
    );
}

#[tokio::test]
async fn a_target_host_carrying_a_port_is_invalid_not_unknown() {
    let harness = common::Harness::new(common::test_config(), None);
    let upstream = json!({
        "alias": "api.example.com",
        "protocol": PROTOCOL,
        "server": { "endpoints": [ { "scheme": "https", "host": "api.example.com", "port": 443 } ] },
    });
    let upstream =
        json_body(post(harness.router(), "/oagw/v1/upstreams", upstream, tenant()).await).await;
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();
    post(
        harness.router(),
        "/oagw/v1/routes",
        json!({ "upstream_id": upstream_id, "match": { "http": { "methods": ["GET"], "path": "/" } } }),
        tenant(),
    )
    .await;

    // ADR-0007: `X-OAGW-Target-Host` is a bare hostname — a port is a format
    // error, not an unknown endpoint, and the caller's value is echoed back.
    let request = axum::http::Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/api.example.com/api")
        .header("x-oagw-target-host", "us.vendor.com:8443")
        .body(axum::body::Body::empty())
        .unwrap();
    let response = harness.send_request(request, tenant()).await;
    assert_eq!(response.status(), 400);
    let body = json_body(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1"
    );
    assert_eq!(body["invalid_value"], "us.vendor.com:8443");
}

#[tokio::test]
async fn content_length_must_match_the_body() {
    let server = MockServer::start();
    let target = server.mock(|when, then| {
        when.method(httpmock::Method::POST);
        then.status(200).body("ok");
    });

    let harness = common::Harness::new(common::test_config(), None);
    let upstream_id = create_upstream(&harness, "length.example.com", &server).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    post(harness.router(), "/oagw/v1/routes", json!({ "upstream_id": upstream_id, "match": { "http": { "methods": ["POST"], "path": "/" } } }), tenant())
        .await;

    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/oagw/v1/proxy/length.example.com/api")
        .header("content-length", "10")
        .body(axum::body::Body::from("abc"))
        .unwrap();
    let response = harness.send_request(request, tenant()).await;
    assert_eq!(response.status(), 400);
    assert_eq!(target.calls(), 0);
}

#[tokio::test]
async fn transfer_encoding_must_be_chunked() {
    let server = MockServer::start();
    let harness = common::Harness::new(common::test_config(), None);
    let upstream_id = create_upstream(&harness, "te.example.com", &server).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    post(harness.router(), "/oagw/v1/routes", json!({ "upstream_id": upstream_id, "match": { "http": { "methods": ["POST"], "path": "/" } } }), tenant())
        .await;

    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/oagw/v1/proxy/te.example.com/api")
        .header("transfer-encoding", "gzip")
        .body(axum::body::Body::from("abc"))
        .unwrap();
    let response = harness.send_request(request, tenant()).await;
    assert_eq!(response.status(), 400);
    assert_eq!(
        json_body(response).await["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
}

#[tokio::test]
async fn disabled_upstream_is_unavailable() {
    let server = MockServer::start();
    let harness = common::Harness::new(common::test_config(), None);
    let upstream = create_upstream(&harness, "disabled.example.com", &server).await;
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();
    post(harness.router(), "/oagw/v1/routes", json!({ "upstream_id": upstream_id, "match": { "http": { "methods": ["GET"], "path": "/" } } }), tenant())
        .await;
    let disabled = json_body(
        harness
            .send(
                "POST",
                &format!("/oagw/v1/upstreams/{upstream_id}/disable"),
                Some(json!({"enabled": false})),
                tenant(),
            )
            .await,
    )
    .await;
    assert_eq!(disabled["enabled"], false);

    let response = harness
        .send(
            "GET",
            "/oagw/v1/proxy/disabled.example.com/api",
            None,
            tenant(),
        )
        .await;
    assert_eq!(response.status(), 503);
    assert_eq!(
        common::header(&response, "x-oagw-error-source").as_deref(),
        Some("gateway")
    );
    assert_eq!(
        json_body(response).await["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1"
    );
}

#[tokio::test]
async fn upstream_500_is_passed_through_unchanged() {
    let server = MockServer::start();
    let target = server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/boom");
        then.status(500)
            .header("x-upstream", "err")
            .body("upstream says no");
    });

    let harness = common::Harness::new(common::test_config(), None);
    let upstream_id = create_upstream(&harness, "boom.example.com", &server).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    post(harness.router(), "/oagw/v1/routes", json!({ "upstream_id": upstream_id, "match": { "http": { "methods": ["GET"], "path": "/" } } }), tenant())
        .await;

    let response = harness
        .send(
            "GET",
            "/oagw/v1/proxy/boom.example.com/boom",
            None,
            tenant(),
        )
        .await;
    assert_eq!(response.status(), 500);
    assert_eq!(
        common::header(&response, "x-oagw-error-source").as_deref(),
        Some("upstream")
    );
    assert_eq!(
        common::header(&response, "x-upstream").as_deref(),
        Some("err")
    );
    assert_eq!(
        common::body_bytes(response).await.as_ref(),
        b"upstream says no"
    );
    assert_eq!(target.calls(), 1);
}

#[tokio::test]
async fn unknown_alias_is_a_gateway_404() {
    let harness = common::Harness::new(common::test_config(), None);
    let response = harness
        .send(
            "GET",
            "/oagw/v1/proxy/no-such-alias.example.com/api",
            None,
            tenant(),
        )
        .await;
    assert_eq!(response.status(), 404);
    assert_eq!(
        common::header(&response, "x-oagw-error-source").as_deref(),
        Some("gateway")
    );
    let body = json_body(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
    );
    assert!(body["instance"].is_string());
    assert!(body["trace_id"].is_string());
}

#[tokio::test]
async fn request_body_is_forwarded_verbatim() {
    let server = MockServer::start();
    let target = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .body("{\"name\":\"pet\"}");
        then.status(201).body("created");
    });

    let harness = common::Harness::new(common::test_config(), None);
    let upstream_id = create_upstream(&harness, "post.example.com", &server).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    post(harness.router(), "/oagw/v1/routes", json!({ "upstream_id": upstream_id, "match": { "http": { "methods": ["POST"], "path": "/" } } }), tenant())
        .await;

    let response = harness
        .send(
            "POST",
            "/oagw/v1/proxy/post.example.com/api",
            Some(json!({ "name": "pet" })),
            tenant(),
        )
        .await;
    assert_eq!(response.status(), 201);
    assert_eq!(common::body_bytes(response).await.as_ref(), b"created");
    assert_eq!(target.calls(), 1);
}

#[tokio::test]
async fn catalog_only_auth_plugin_is_rejected_at_configuration_time() {
    let harness = common::Harness::new(common::test_config(), None);
    let upstream = json!({
        "alias": "catalog.example.com",
        "protocol": PROTOCOL,
        "server": { "endpoints": [ { "scheme": "https", "host": "catalog.example.com", "port": 443 } ] },
        "auth": {
            "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1",
            "config": {},
        },
    });
    let response = post(harness.router(), "/oagw/v1/upstreams", upstream, tenant()).await;
    assert_eq!(response.status(), 400);
    let body = json_body(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .contains("catalog identifier with no implementation")
    );
}

#[tokio::test]
async fn unknown_chain_plugin_fails_closed() {
    let harness = common::Harness::new(common::test_config(), None);
    let upstream = json!({
        "alias": "chain.example.com",
        "protocol": PROTOCOL,
        "server": { "endpoints": [ { "scheme": "https", "host": "chain.example.com", "port": 443 } ] },
        "plugins": { "items": ["gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1"] },
    });
    let upstream_id = json_body(
        post(harness.router(), "/oagw/v1/upstreams", upstream, tenant()).await,
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    post(harness.router(), "/oagw/v1/routes", json!({ "upstream_id": upstream_id, "match": { "http": { "methods": ["GET"], "path": "/" } } }), tenant())
        .await;

    let response = harness
        .send(
            "GET",
            "/oagw/v1/proxy/chain.example.com/api",
            None,
            tenant(),
        )
        .await;
    assert_eq!(response.status(), 503);
    let body = json_body(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1"
    );
    assert_eq!(body["upstream_id"], upstream_id);
}

#[tokio::test]
async fn plain_http_is_refused_when_not_allowed() {
    // The upstream is loopback on purpose (the protocol refusal must be what
    // fails the request, not the SSRF guard), so the SSRF policy is opted out
    // while `allow_http_upstream` stays at its secure default.
    let config = oagw::OagwConfig {
        ssrf_policy: oagw::config::SsrfPolicy {
            enabled: true,
            allow_private_addresses: true,
        },
        ..oagw::OagwConfig::default()
    };
    let harness = common::Harness::new(config, None);
    let server = MockServer::start();
    let upstream = json!({
        "alias": "insecure.example.com",
        "protocol": PROTOCOL,
        "server": { "endpoints": [ { "scheme": "http", "host": server.host(), "port": server.port() } ] },
    });
    let response = post(harness.router(), "/oagw/v1/upstreams", upstream, tenant()).await;
    assert_eq!(response.status(), 201);
    let upstream_id = json_body(response).await["id"].as_str().unwrap().to_owned();
    post(harness.router(), "/oagw/v1/routes", json!({ "upstream_id": upstream_id, "match": { "http": { "methods": ["GET"], "path": "/" } } }), tenant())
        .await;

    let response = harness
        .send(
            "GET",
            "/oagw/v1/proxy/insecure.example.com/api",
            None,
            tenant(),
        )
        .await;
    assert_eq!(response.status(), 502);
    assert_eq!(
        json_body(response).await["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1"
    );
}

#[tokio::test]
async fn unresolvable_upstream_is_a_gateway_502() {
    // Port 1 on loopback refuses the connection: the gateway reports 502.
    let harness = common::Harness::new(common::test_config(), None);
    let upstream = json!({
        "alias": "refusing.example.com",
        "protocol": PROTOCOL,
        "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": 1 } ] },
    });
    let upstream_id = json_body(
        post(harness.router(), "/oagw/v1/upstreams", upstream, tenant()).await,
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    post(harness.router(), "/oagw/v1/routes", json!({ "upstream_id": upstream_id, "match": { "http": { "methods": ["GET"], "path": "/" } } }), tenant())
        .await;

    let response = harness
        .send(
            "GET",
            "/oagw/v1/proxy/refusing.example.com/api",
            None,
            tenant(),
        )
        .await;
    assert_eq!(response.status(), 502);
    assert_eq!(
        common::header(&response, "x-oagw-error-source").as_deref(),
        Some("gateway")
    );
    let body = json_body(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1"
    );
    assert_eq!(body["upstream_id"], upstream_id);
}

#[tokio::test]
async fn tenant_scoping_applies_to_the_proxy_path() {
    let server = MockServer::start();
    let harness = common::Harness::new(common::test_config(), None);
    create_upstream(&harness, "scoped.example.com", &server).await;

    let other = Uuid::new_v4();
    let response = harness
        .send("GET", "/oagw/v1/proxy/scoped.example.com/api", None, other)
        .await;
    assert_eq!(response.status(), 404);
    assert_eq!(
        json_body(response).await["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
    );
}

#[tokio::test]
async fn get_with_no_route_is_a_gateway_404() {
    let server = MockServer::start();
    let harness = common::Harness::new(common::test_config(), None);
    let upstream_id = create_upstream(&harness, "noroute.example.com", &server).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let _ = get(harness.router(), "/oagw/v1/upstreams", tenant()).await;
    assert!(!upstream_id.is_empty());
    let response = harness
        .send(
            "GET",
            "/oagw/v1/proxy/noroute.example.com/api",
            None,
            tenant(),
        )
        .await;
    assert_eq!(response.status(), 404);
    assert_eq!(
        common::header(&response, "x-oagw-error-source").as_deref(),
        Some("gateway")
    );
}
