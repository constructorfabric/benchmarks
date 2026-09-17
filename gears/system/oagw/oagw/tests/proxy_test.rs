//! Data-plane (proxy) integration tests against real mock upstreams.
//!
//! Exercises `/oagw/v1/proxy/{alias}/...` end to end: alias resolution
//! along the tenant chain, route matching, auth/guard/transform plugins,
//! rate limiting, CORS, target-host selection, and RFC 9457 error
//! semantics.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::doc_markdown)]

mod common;

use std::sync::Arc;

use axum::http::StatusCode;
use httpmock::prelude::*;
use uuid::Uuid;

use common::{PROTOCOL_HTTP, admin_ctx, expect_problem, response_text, send};

const PROXY_SCOPE: &str = "gts.cf.core.oagw.proxy.v1~:invoke";

fn proxy_ctx(tenant: Uuid) -> toolkit_security::SecurityContext {
    common::make_ctx(tenant, &[PROXY_SCOPE])
}

/// Endpoints array for a mock upstream on 127.0.0.1 (IP endpoints need
/// an explicit alias).
fn mock_endpoints(port: u16) -> serde_json::Value {
    serde_json::json!([{ "scheme": "http", "host": "127.0.0.1", "port": port }])
}

// =====================================================================
//                        Happy path & resolution
// =====================================================================

#[tokio::test]
async fn proxies_request_to_upstream_with_suffix() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/hello");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"ok":true}"#);
    });

    let app = common::TestApp::new().await;
    let tenant = Uuid::new_v4();
    common::seed_upstream_and_route(
        &app,
        tenant,
        Some("svc"),
        mock_endpoints(server.port()),
        serde_json::json!({}),
    )
    .await;

    let ctx = proxy_ctx(tenant);
    let resp = send(
        &app.router,
        "GET",
        "/oagw/v1/proxy/svc/hello",
        &ctx,
        None,
        &[],
    )
    .await;
    let status = resp.status();
    let error_source = resp
        .headers()
        .get("x-oagw-error-source")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = response_text(resp).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(error_source.as_deref(), Some("upstream"));
    let parsed: serde_json::Value =
        serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
    assert_eq!(parsed["ok"], true);
    mock.assert();
}

#[tokio::test]
async fn proxy_resolves_upstream_along_tenant_chain() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/");
        then.status(200).body("from-parent");
    });

    // `tenant` sits under `parent`; the upstream lives in `parent`.
    let parent = Uuid::new_v4();
    let tenant = Uuid::new_v4();
    let app = common::TestApp::with_tenant_chain(vec![parent]).await;
    common::seed_upstream_and_route(
        &app,
        parent,
        Some("shared-svc"),
        mock_endpoints(server.port()),
        serde_json::json!({}),
    )
    .await;

    let ctx = proxy_ctx(tenant);
    let resp = send(
        &app.router,
        "GET",
        "/oagw/v1/proxy/shared-svc",
        &ctx,
        None,
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "body: {}", response_text(resp).await);
}

#[tokio::test]
async fn proxy_unknown_alias_returns_404() {
    let app = common::TestApp::new().await;
    let tenant = Uuid::new_v4();

    let ctx = proxy_ctx(tenant);
    let resp = send(
        &app.router,
        "GET",
        "/oagw/v1/proxy/does-not-exist",
        &ctx,
        None,
        &[],
    )
    .await;
    let body = expect_problem(resp, StatusCode::NOT_FOUND).await;
    assert_eq!(body["type"], oagw::gts::ERR_ROUTE_NOT_FOUND);
    assert!(body["detail"]
        .as_str()
        .unwrap()
        .contains("does-not-exist"));
}

#[tokio::test]
async fn proxy_without_invoke_scope_returns_403() {
    let app = common::TestApp::new().await;
    let tenant = Uuid::new_v4();

    let ctx = common::make_ctx(tenant, &[]);
    let resp = send(
        &app.router,
        "GET",
        "/oagw/v1/proxy/anything",
        &ctx,
        None,
        &[],
    )
    .await;
    let body = expect_problem(resp, StatusCode::FORBIDDEN).await;
    assert_eq!(body["type"], oagw::gts::ERR_PERMISSION_DENIED);
}

#[tokio::test]
async fn proxy_disabled_upstream_returns_503() {
    let app = common::TestApp::new().await;
    let tenant = Uuid::new_v4();
    let upstream = common::seed_upstream_and_route(
        &app,
        tenant,
        Some("off"),
        mock_endpoints(1),
        serde_json::json!({}),
    )
    .await;

    // Disable the upstream via PUT (same alias → allowed).
    let ctx = admin_ctx(tenant);
    let resp = send(
        &app.router,
        "PUT",
        &format!("/oagw/v1/upstreams/{upstream}"),
        &ctx,
        Some(serde_json::json!({
            "enabled": false,
            "alias": "off",
            "server": { "endpoints": [{ "scheme": "http", "host": "127.0.0.1", "port": 1 }] },
            "protocol": PROTOCOL_HTTP
        })),
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let ctx = proxy_ctx(tenant);
    let resp = send(
        &app.router,
        "GET",
        "/oagw/v1/proxy/off",
        &ctx,
        None,
        &[],
    )
    .await;
    let body = expect_problem(resp, StatusCode::SERVICE_UNAVAILABLE).await;
    assert_eq!(body["type"], oagw::gts::ERR_LINK_UNAVAILABLE);
}

// =====================================================================
//                             Route matching
// =====================================================================

#[tokio::test]
async fn proxy_method_not_in_route_returns_404() {
    let server = MockServer::start();
    let app = common::TestApp::new().await;
    let tenant = Uuid::new_v4();
    common::seed_upstream_and_route(
        &app,
        tenant,
        Some("svc"),
        mock_endpoints(server.port()),
        serde_json::json!({}),
    )
    .await;

    // The seed route allows GET/POST/PUT/PATCH/DELETE — OPTIONS without a
    // preflight-shaped request falls through to the data plane and must
    // not match.
    let ctx = proxy_ctx(tenant);
    let resp = send(
        &app.router,
        "OPTIONS",
        "/oagw/v1/proxy/svc",
        &ctx,
        None,
        &[],
    )
    .await;
    expect_problem(resp, StatusCode::NOT_FOUND).await;
}

#[tokio::test]
async fn proxy_path_suffix_disabled_is_400() {
    let server = MockServer::start();
    let app = common::TestApp::new().await;
    let tenant = Uuid::new_v4();

    let upstream = common::create_upstream(
        &app,
        tenant,
        serde_json::json!({
            "enabled": true,
            "alias": "exact",
            "server": { "endpoints": mock_endpoints(server.port()) },
            "protocol": PROTOCOL_HTTP
        }),
    )
    .await;
    common::create_route(
        &app,
        tenant,
        serde_json::json!({
            "enabled": true,
            "upstream_id": upstream["id"].as_str().unwrap(),
            "match": {
                "http": { "methods": ["GET"], "path": "/exact", "path_suffix_mode": "disabled" }
            }
        }),
    )
    .await;

    let ctx = proxy_ctx(tenant);
    let resp = send(
        &app.router,
        "GET",
        "/oagw/v1/proxy/exact/extra",
        &ctx,
        None,
        &[],
    )
    .await;
    let body = expect_problem(resp, StatusCode::BAD_REQUEST).await;
    assert_eq!(body["type"], oagw::gts::ERR_VALIDATION);
    assert!(body["detail"].as_str().unwrap().contains("path suffix is disabled"));

    // Suffix-free request reaches the upstream.
    server.mock(|when, then| {
        when.method(GET).path("/exact");
        then.status(200).body("ok");
    });
    let resp = send(
        &app.router,
        "GET",
        "/oagw/v1/proxy/exact",
        &ctx,
        None,
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

// =====================================================================
//                      Auth / guard / transform plugins
// =====================================================================

#[tokio::test]
async fn apikey_auth_plugin_injects_secret_header() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET)
            .path("/secured")
            .header("x-api-key", "sekret-123");
        then.status(200).body("authorized");
    });

    let credstore: Arc<dyn credstore_sdk::CredStoreClientV1> = Arc::new(
        credstore_sdk::test_util::MockCredStoreClient::with_secrets(vec![(
            "oagw-test-key".to_owned(),
            "sekret-123".to_owned(),
        )]),
    );
    let app = common::TestApp::with_credstore(credstore).await;
    let tenant = Uuid::new_v4();
    common::seed_upstream_and_route(
        &app,
        tenant,
        Some("svc"),
        mock_endpoints(server.port()),
        serde_json::json!({
            "auth": {
                "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
                "config": { "api_key_ref": "oagw-test-key", "header_name": "X-API-Key" }
            }
        }),
    )
    .await;

    let ctx = proxy_ctx(tenant);
    let resp = send(
        &app.router,
        "GET",
        "/oagw/v1/proxy/svc/secured",
        &ctx,
        None,
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "body: {}", response_text(resp).await);
    mock.assert();
}

#[tokio::test]
async fn apikey_auth_missing_secret_returns_500() {
    let server = MockServer::start();
    let app = common::TestApp::new().await; // empty credstore
    let tenant = Uuid::new_v4();
    common::seed_upstream_and_route(
        &app,
        tenant,
        Some("svc"),
        mock_endpoints(server.port()),
        serde_json::json!({
            "auth": {
                "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
                "config": { "api_key_ref": "nope", "header_name": "X-API-Key" }
            }
        }),
    )
    .await;

    let ctx = proxy_ctx(tenant);
    let resp = send(
        &app.router,
        "GET",
        "/oagw/v1/proxy/svc",
        &ctx,
        None,
        &[],
    )
    .await;
    let body = expect_problem(resp, StatusCode::INTERNAL_SERVER_ERROR).await;
    assert_eq!(body["type"], oagw::gts::ERR_SECRET_NOT_FOUND);
}

#[tokio::test]
async fn catalog_only_auth_plugin_fails_with_plugin_not_found() {
    let server = MockServer::start();
    let app = common::TestApp::new().await;
    let tenant = Uuid::new_v4();
    common::seed_upstream_and_route(
        &app,
        tenant,
        Some("svc"),
        mock_endpoints(server.port()),
        serde_json::json!({
            "auth": {
                "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1",
                "config": {}
            }
        }),
    )
    .await;

    let ctx = proxy_ctx(tenant);
    let resp = send(
        &app.router,
        "GET",
        "/oagw/v1/proxy/svc",
        &ctx,
        None,
        &[],
    )
    .await;
    let body = expect_problem(resp, StatusCode::SERVICE_UNAVAILABLE).await;
    assert_eq!(body["type"], oagw::gts::ERR_PLUGIN_NOT_FOUND);
}

#[tokio::test]
async fn required_headers_guard_rejects_missing_header() {
    let server = MockServer::start();
    let app = common::TestApp::new().await;
    let tenant = Uuid::new_v4();
    common::seed_upstream_and_route(
        &app,
        tenant,
        Some("svc"),
        mock_endpoints(server.port()),
        serde_json::json!({
            "plugins": {
                "items": [{
                    "plugin_ref": "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
                    "config": { "required_request_headers": "x-tenant, x-api-key" }
                }]
            }
        }),
    )
    .await;

    let ctx = proxy_ctx(tenant);
    let resp = send(
        &app.router,
        "GET",
        "/oagw/v1/proxy/svc",
        &ctx,
        None,
        &[("x-tenant", "acme")], // x-api-key missing
    )
    .await;
    let body = expect_problem(resp, StatusCode::BAD_REQUEST).await;
    assert_eq!(body["type"], oagw::gts::ERR_VALIDATION);
    assert!(body["detail"].as_str().unwrap().contains("x-api-key"));

    // With both headers present the request is forwarded.
    let _guard_mock = server.mock(|when, then| {
        when.method(GET).path("/");
        then.status(200).body("guard ok");
    });
    let resp = send(
        &app.router,
        "GET",
        "/oagw/v1/proxy/svc",
        &ctx,
        None,
        &[("x-tenant", "acme"), ("x-api-key", "k")],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "body: {}", response_text(resp).await);
}

#[tokio::test]
async fn request_id_transform_echoes_header() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/");
        then.status(200).body("traced");
    });
    let app = common::TestApp::new().await;
    let tenant = Uuid::new_v4();
    common::seed_upstream_and_route(
        &app,
        tenant,
        Some("svc"),
        mock_endpoints(server.port()),
        serde_json::json!({
            "plugins": {
                "items": ["gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1"]
            },
            // Default passthrough is "none": allow the inbound header so
            // the request_id transform propagates the client's value.
            "headers": { "request": { "passthrough": "all" } }
        }),
    )
    .await;

    let ctx = proxy_ctx(tenant);
    let resp = send(
        &app.router,
        "GET",
        "/oagw/v1/proxy/svc",
        &ctx,
        None,
        &[("x-request-id", "client-provided-id")],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok()),
        Some("client-provided-id")
    );

    // Without a client id, a fresh UUID is allocated and echoed.
    let resp = send(&app.router, "GET", "/oagw/v1/proxy/svc", &ctx, None, &[]).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let generated = resp
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .expect("x-request-id echoed");
    assert!(Uuid::parse_str(generated).is_ok());
}

// =====================================================================
//                            Rate limiting
// =====================================================================

#[tokio::test]
async fn rate_limit_rejects_when_budget_exhausted() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/");
        then.status(200).body("limited");
    });
    let app = common::TestApp::new().await;
    let tenant = Uuid::new_v4();
    common::seed_upstream_and_route(
        &app,
        tenant,
        Some("svc"),
        mock_endpoints(server.port()),
        serde_json::json!({
            "rate_limit": {
                "sustained": { "rate": 1, "window": "second" },
                "scope": "tenant"
            }
        }),
    )
    .await;

    let ctx = proxy_ctx(tenant);
    let first = send(&app.router, "GET", "/oagw/v1/proxy/svc", &ctx, None, &[]).await;
    assert_eq!(first.status(), StatusCode::OK);

    // Second request within the same window: capacity is 1 → rejected.
    let resp = send(&app.router, "GET", "/oagw/v1/proxy/svc", &ctx, None, &[]).await;
    let body = expect_problem(resp, StatusCode::TOO_MANY_REQUESTS).await;
    assert_eq!(body["type"], oagw::gts::ERR_RATE_LIMIT_EXCEEDED);
    assert!(body["retry_after_seconds"].as_u64().unwrap() >= 1);
}

// =====================================================================
//                               CORS
// =====================================================================

#[tokio::test]
async fn cors_actual_request_rejects_disallowed_origin() {
    let server = MockServer::start();
    let app = common::TestApp::new().await;
    let tenant = Uuid::new_v4();
    common::seed_upstream_and_route(
        &app,
        tenant,
        Some("svc"),
        mock_endpoints(server.port()),
        serde_json::json!({
            "cors": {
                "enabled": true,
                "allowed_origins": ["https://good.example"],
                "allowed_methods": ["GET"]
            }
        }),
    )
    .await;

    let ctx = proxy_ctx(tenant);
    let resp = send(
        &app.router,
        "GET",
        "/oagw/v1/proxy/svc",
        &ctx,
        None,
        &[("origin", "https://evil.example")],
    )
    .await;
    let body = expect_problem(resp, StatusCode::FORBIDDEN).await;
    assert_eq!(body["type"], oagw::gts::ERR_CORS_ORIGIN_NOT_ALLOWED);

    // Allowed origin passes through and gets CORS response headers.
    server.mock(|when, then| {
        when.method(GET).path("/");
        then.status(200).body("cors ok");
    });
    let resp = send(
        &app.router,
        "GET",
        "/oagw/v1/proxy/svc",
        &ctx,
        None,
        &[("origin", "https://good.example")],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "body: {}", response_text(resp).await);
    assert_eq!(
        resp.headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("https://good.example")
    );
}

#[tokio::test]
async fn cors_preflight_is_answered_permissively() {
    let app = common::TestApp::new().await;
    let tenant = Uuid::new_v4();

    // Preflight short-circuits before any scope check or upstream
    // resolution: even a token with no scopes gets the 204.
    let ctx = common::make_ctx(tenant, &[]);
    let resp = send(
        &app.router,
        "OPTIONS",
        "/oagw/v1/proxy/svc/anything",
        &ctx,
        None,
        &[
            ("origin", "https://client.example"),
            ("access-control-request-method", "POST"),
            ("access-control-request-headers", "x-tenant"),
        ],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        resp.headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("https://client.example")
    );
    assert_eq!(
        resp.headers()
            .get("access-control-allow-methods")
            .and_then(|v| v.to_str().ok()),
        Some("POST")
    );
}

// =====================================================================
//                     Endpoint selection (target host)
// =====================================================================

#[tokio::test]
async fn target_host_selects_endpoint_in_explicit_alias_pool() {
    let server_a = MockServer::start();
    let server_b = MockServer::start();
    server_a.mock(|when, then| {
        when.method(GET).path("/");
        then.status(200).body("from-a");
    });
    server_b.mock(|when, then| {
        when.method(GET).path("/");
        then.status(200).body("from-b");
    });

    let app = common::TestApp::new().await;
    let tenant = Uuid::new_v4();
    // IP pool → explicit alias. Both endpoints share host 127.0.0.1; the
    // distinguishing dimension is the port carried by each endpoint.
    let upstream = common::create_upstream(
        &app,
        tenant,
        serde_json::json!({
            "enabled": true,
            "alias": "pool",
            "server": { "endpoints": [
                { "scheme": "http", "host": "127.0.0.1", "port": server_a.port() },
                { "scheme": "http", "host": "127.0.0.1", "port": server_b.port() }
            ]},
            "protocol": PROTOCOL_HTTP
        }),
    )
    .await;
    common::create_route(
        &app,
        tenant,
        serde_json::json!({
            "enabled": true,
            "upstream_id": upstream["id"].as_str().unwrap(),
            "match": { "http": { "methods": ["GET"], "path": "/" } }
        }),
    )
    .await;

    let ctx = proxy_ctx(tenant);
    // Round-robin hits endpoint[0] (server_a) first.
    let resp = send(&app.router, "GET", "/oagw/v1/proxy/pool", &ctx, None, &[]).await;
    assert_eq!(resp.status(), StatusCode::OK, "first: {}", response_text(resp).await);

    // Explicit target host selects the first matching endpoint.
    let resp = send(
        &app.router,
        "GET",
        "/oagw/v1/proxy/pool",
        &ctx,
        None,
        &[("x-oagw-target-host", "127.0.0.1")],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "body: {}", response_text(resp).await);

    // Unknown target host → 400 unknown_target_host.
    let resp = send(
        &app.router,
        "GET",
        "/oagw/v1/proxy/pool",
        &ctx,
        None,
        &[("x-oagw-target-host", "other.example")],
    )
    .await;
    let body = expect_problem(resp, StatusCode::BAD_REQUEST).await;
    assert_eq!(body["type"], oagw::gts::ERR_UNKNOWN_TARGET_HOST);
}

#[tokio::test]
async fn common_suffix_pool_requires_target_host_header() {
    let app = common::TestApp::new().await;
    let tenant = Uuid::new_v4();

    // Hostname pool sharing the registrable suffix `test-a.com` derives
    // the pooled alias `test-a.com`; the header is then mandatory.
    let upstream = common::create_upstream(
        &app,
        tenant,
        serde_json::json!({
            "enabled": true,
            "server": { "endpoints": [
                { "scheme": "https", "host": "us.test-a.com" },
                { "scheme": "https", "host": "eu.test-a.com" }
            ]},
            "protocol": PROTOCOL_HTTP
        }),
    )
    .await;
    assert_eq!(upstream["alias"], "test-a.com");
    common::create_route(
        &app,
        tenant,
        serde_json::json!({
            "enabled": true,
            "upstream_id": upstream["id"].as_str().unwrap(),
            "match": { "http": { "methods": ["GET"], "path": "/" } }
        }),
    )
    .await;

    let ctx = proxy_ctx(tenant);
    let resp = send(
        &app.router,
        "GET",
        "/oagw/v1/proxy/test-a.com",
        &ctx,
        None,
        &[],
    )
    .await;
    let body = expect_problem(resp, StatusCode::BAD_REQUEST).await;
    assert_eq!(body["type"], oagw::gts::ERR_MISSING_TARGET_HOST);

    // A target host gets past endpoint selection; the connection to
    // `us.test-a.com:443` then fails (DNS/transport) → 502/503, never a
    // 400/404 (proving endpoint selection ran).
    let resp = send(
        &app.router,
        "GET",
        "/oagw/v1/proxy/test-a.com",
        &ctx,
        None,
        &[("x-oagw-target-host", "us.test-a.com")],
    )
    .await;
    assert!(
        matches!(
            resp.status(),
            StatusCode::BAD_GATEWAY | StatusCode::SERVICE_UNAVAILABLE
        ),
        "expected 502/503 after endpoint selection, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn target_host_with_port_is_invalid() {
    let app = common::TestApp::new().await;
    let tenant = Uuid::new_v4();
    common::seed_upstream_and_route(
        &app,
        tenant,
        Some("svc"),
        mock_endpoints(1),
        serde_json::json!({}),
    )
    .await;

    let ctx = proxy_ctx(tenant);
    let resp = send(
        &app.router,
        "GET",
        "/oagw/v1/proxy/svc",
        &ctx,
        None,
        &[("x-oagw-target-host", "127.0.0.1:8443")],
    )
    .await;
    let body = expect_problem(resp, StatusCode::BAD_REQUEST).await;
    assert_eq!(body["type"], oagw::gts::ERR_INVALID_TARGET_HOST);
}

// =====================================================================
//                        Body / payload rules
// =====================================================================

#[tokio::test]
async fn payload_above_limit_is_413() {
    let app = common::TestApp::new().await;
    let tenant = Uuid::new_v4();
    common::seed_upstream_and_route(
        &app,
        tenant,
        Some("svc"),
        mock_endpoints(1),
        serde_json::json!({}),
    )
    .await;

    // A Content-Length header above the 100 MB ceiling is rejected
    // before any upstream traffic.
    let ctx = proxy_ctx(tenant);
    let resp = send(
        &app.router,
        "POST",
        "/oagw/v1/proxy/svc",
        &ctx,
        None,
        &[("content-length", "999999999")],
    )
    .await;
    let body = expect_problem(resp, StatusCode::PAYLOAD_TOO_LARGE).await;
    assert_eq!(body["type"], oagw::gts::ERR_PAYLOAD_TOO_LARGE);
}
