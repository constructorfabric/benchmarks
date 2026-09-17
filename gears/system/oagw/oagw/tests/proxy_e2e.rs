//! End-to-end tests through the registered axum router: control-plane CRUD
//! plus the data-plane proxy against `httpmock` upstreams.
//!
//! Covers the required coverage areas PROXY END-TO-END, ERRORS, CORS,
//! RATE LIMITING, BUILT-IN PLUGINS, ALIAS rules and DTO/schema fidelity at
//! the HTTP boundary.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::header::CONTENT_TYPE;
use axum::http::{Request, StatusCode};
use common::{
    api_json, build_env, build_env_with_hierarchy, create_http_route, create_ip_upstream,
    make_ctx, proxy_request, raw, TestEnv,
};
use httpmock::prelude::*;
use oagw::config::OagwConfig;
use oagw::domain::gts;
use uuid::Uuid;

/// Config used by most tests: HTTP upstreams allowed, SSRF off.
fn cfg() -> OagwConfig {
    OagwConfig {
        allow_http_upstream: true,
        proxy_timeout_secs: 5,
        ..Default::default()
    }
}

fn problem_type(bytes: &[u8]) -> Option<String> {
    let json: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    json["type"].as_str().map(str::to_owned)
}

fn body_text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Proxy happy path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn proxy_forwards_to_upstream_and_tags_source() {
    let env = build_env(cfg());
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/v1/ping");
        then.status(200)
            .header("content-type", "text/plain")
            .body("pong");
    });

    // Bind the built-in `request_id` transform so the data-plane chain
    // exercises a resolvable built-in end to end.
    let (status, json) = api_json(
        &env,
        "POST",
        "/oagw/v1/upstreams",
        Some(serde_json::json!({
            "enabled": true,
            "alias": "mock-api",
            "server": { "endpoints": [{ "scheme": "http", "host": "127.0.0.1", "port": server.port() }] },
            "protocol": "http",
            "plugins": {
                "items": [{
                    "plugin_ref": gts::TRANSFORM_REQUEST_ID,
                    "position": 0
                }]
            }
        })),
        env.tenant,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{json}");
    let up = Uuid::parse_str(json["id"].as_str().unwrap()).unwrap();
    create_http_route(&env, env.tenant, up, "/v1", &["GET"]).await;

    let (status, headers, bytes) = raw(
        &env,
        proxy_request("GET", "/oagw/v1/proxy/mock-api/v1/ping", env.tenant),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "status={status} headers={headers:?} body={:?}",
        body_text(&bytes)
    );
    assert_eq!(body_text(&bytes), "pong");
    mock.assert();
    // Transform: the data plane propagates a generated request id.
    assert!(
        headers.contains_key("x-request-id"),
        "x-request-id missing: {headers:?}"
    );
    // ADR 0007: upstream success responses are tagged `upstream`.
    assert_eq!(headers.get("x-oagw-error-source").unwrap(), "upstream");
}

#[tokio::test]
async fn proxy_posts_body_and_preserves_status() {
    let env = build_env(cfg());
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/v1/submit").body("hello upstream");
        then.status(201).header("x-upstream", "yes").body("accepted");
    });

    let up = create_ip_upstream(&env, env.tenant, "mock-api", server.port()).await;
    create_http_route(&env, env.tenant, up, "/v1", &["POST"]).await;

    let mut req = Request::builder()
        .method("POST")
        .uri("/oagw/v1/proxy/mock-api/v1/submit")
        .header("content-type", "text/plain")
        .body(Body::from("hello upstream"))
        .unwrap();
    req.extensions_mut().insert(make_ctx(env.tenant));

    let (status, headers, bytes) = raw(&env, req).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body_text(&bytes), "accepted");
    assert_eq!(headers.get("x-upstream").unwrap(), "yes");
    assert_eq!(headers.get("x-oagw-error-source").unwrap(), "upstream");
    mock.assert();
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unknown_alias_returns_404_problem() {
    let env = build_env(cfg());
    let (status, headers, bytes) = raw(
        &env,
        proxy_request("GET", "/oagw/v1/proxy/never-existed/x", env.tenant),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        headers.get(CONTENT_TYPE).unwrap(),
        "application/problem+json"
    );
    assert_eq!(headers.get("x-oagw-error-source").unwrap(), "gateway");
    assert_eq!(problem_type(&bytes).as_deref(), Some(gts::ERR_ROUTE_NOT_FOUND));
}

#[tokio::test]
async fn disabled_upstream_returns_503() {
    let env = build_env(cfg());
    let up = create_ip_upstream(&env, env.tenant, "mock-api", 9).await;
    create_http_route(&env, env.tenant, up, "/", &["GET"]).await;

    let (status, _) = api_json(
        &env,
        "POST",
        &format!("/oagw/v1/upstreams/{up}/disable"),
        None,
        env.tenant,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, headers, bytes) = raw(
        &env,
        proxy_request("GET", "/oagw/v1/proxy/mock-api/x", env.tenant),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(headers.get("x-oagw-error-source").unwrap(), "gateway");
    assert_eq!(
        problem_type(&bytes).as_deref(),
        Some(gts::ERR_LINK_UNAVAILABLE)
    );
    assert!(headers.contains_key("retry-after"));
}

#[tokio::test]
async fn method_not_allowlisted_returns_404_route_not_found() {
    let env = build_env(cfg());
    let server = MockServer::start();
    let up = create_ip_upstream(&env, env.tenant, "mock-api", server.port()).await;
    // Only GET is allowed.
    create_http_route(&env, env.tenant, up, "/", &["GET"]).await;

    let (status, headers, bytes) = raw(
        &env,
        proxy_request("POST", "/oagw/v1/proxy/mock-api/x", env.tenant),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(headers.get("x-oagw-error-source").unwrap(), "gateway");
    assert_eq!(
        problem_type(&bytes).as_deref(),
        Some(gts::ERR_ROUTE_NOT_FOUND)
    );
}

#[tokio::test]
async fn https_upstream_returns_502_protocol_error() {
    // TLS connectors are not wired in this build — https upstreams are never
    // forwardable and must surface 502 ProtocolError before any socket opens.
    // The alias must equal the auto-derived one ("example.com").
    let env = build_env(cfg());
    let (status, json) = api_json(
        &env,
        "POST",
        "/oagw/v1/upstreams",
        Some(serde_json::json!({
            "enabled": true,
            "alias": "example.com",
            "server": { "endpoints": [{ "scheme": "https", "host": "example.com", "port": 443 }] },
            "protocol": "http",
        })),
        env.tenant,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{json}");
    let up = Uuid::parse_str(json["id"].as_str().unwrap()).unwrap();
    create_http_route(&env, env.tenant, up, "/", &["GET"]).await;

    let (status, headers, bytes) = raw(
        &env,
        proxy_request("GET", "/oagw/v1/proxy/example.com/x", env.tenant),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(headers.get("x-oagw-error-source").unwrap(), "gateway");
    assert_eq!(
        problem_type(&bytes).as_deref(),
        Some(gts::ERR_PROTOCOL_ERROR)
    );
}

#[tokio::test]
async fn unreachable_upstream_returns_502_downstream_error() {
    let env = build_env(cfg());
    // Grab an ephemeral port then release it, so nothing is listening.
    let port = {
        let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        l.local_addr().unwrap().port()
    };
    let up = create_ip_upstream(&env, env.tenant, "mock-api", port).await;
    create_http_route(&env, env.tenant, up, "/", &["GET"]).await;

    let (status, headers, bytes) = raw(
        &env,
        proxy_request("GET", "/oagw/v1/proxy/mock-api/x", env.tenant),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(headers.get("x-oagw-error-source").unwrap(), "gateway");
    assert_eq!(
        problem_type(&bytes).as_deref(),
        Some(gts::ERR_DOWNSTREAM_ERROR)
    );
    assert!(headers.contains_key("retry-after"));
}

#[tokio::test]
async fn ssrf_policy_blocks_private_and_loopback() {
    let env = build_env(OagwConfig {
        allow_http_upstream: true,
        proxy_timeout_secs: 5,
        ssrf_policy: oagw::config::SsrfPolicyConfig { enabled: true },
        ..Default::default()
    });
    let up = create_ip_upstream(&env, env.tenant, "mock-api", 8080).await;
    create_http_route(&env, env.tenant, up, "/", &["GET"]).await;

    let (status, _, bytes) = raw(
        &env,
        proxy_request("GET", "/oagw/v1/proxy/mock-api/x", env.tenant),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(problem_type(&bytes).as_deref(), Some(gts::ERR_VALIDATION));
}

#[tokio::test]
async fn request_body_over_limit_returns_413() {
    let env = build_env(OagwConfig {
        allow_http_upstream: true,
        proxy_timeout_secs: 5,
        body_limit_bytes: 8,
        ..Default::default()
    });
    let up = create_ip_upstream(&env, env.tenant, "mock-api", 9).await;
    create_http_route(&env, env.tenant, up, "/", &["POST"]).await;

    let mut req = Request::builder()
        .method("POST")
        .uri("/oagw/v1/proxy/mock-api/x")
        .header("content-length", "20")
        .body(Body::from(vec![b'x'; 20]))
        .unwrap();
    req.extensions_mut().insert(make_ctx(env.tenant));

    let (status, headers, bytes) = raw(&env, req).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(headers.get("x-oagw-error-source").unwrap(), "gateway");
    assert_eq!(
        problem_type(&bytes).as_deref(),
        Some(gts::ERR_PAYLOAD_TOO_LARGE)
    );
}

// ---------------------------------------------------------------------------
// CORS (ADR 0004)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cors_preflight_returns_permissive_204() {
    // Preflight is answered at the handler level — even for an unknown alias.
    let env = build_env(cfg());
    let mut req = Request::builder()
        .method("OPTIONS")
        .uri("/oagw/v1/proxy/whatever/x")
        .header("origin", "https://browser.example")
        .header("access-control-request-method", "GET")
        .body(Body::empty())
        .unwrap();
    req.extensions_mut().insert(make_ctx(env.tenant));

    let (status, headers, _) = raw(&env, req).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        headers.get("access-control-allow-origin").unwrap(),
        "https://browser.example"
    );
    assert!(headers.contains_key("access-control-allow-methods"));
    assert_eq!(headers.get("vary").unwrap(), "Origin");
}

#[tokio::test]
async fn cors_actual_request_disallowed_origin_returns_403() {
    let env = build_env(cfg());
    let server = MockServer::start();
    let ok = server.mock(|when, then| {
        when.method(GET).path("/v1/x");
        then.status(200).body("ok");
    });
    let (status, json) = api_json(
        &env,
        "POST",
        "/oagw/v1/upstreams",
        Some(serde_json::json!({
            "enabled": true,
            "alias": "cors-api",
            "server": { "endpoints": [{ "scheme": "http", "host": "127.0.0.1", "port": server.port() }] },
            "protocol": "http",
            "cors": {
                "enabled": true,
                "allowed_origins": ["https://good.example"],
                "allowed_methods": ["GET", "POST"]
            }
        })),
        env.tenant,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{json}");
    let up = Uuid::parse_str(json["id"].as_str().unwrap()).unwrap();
    create_http_route(&env, env.tenant, up, "/v1", &["GET"]).await;

    // Allowed origin + allowed method proceeds.
    let mut req = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/cors-api/v1/x")
        .header("origin", "https://good.example")
        .body(Body::empty())
        .unwrap();
    req.extensions_mut().insert(make_ctx(env.tenant));
    let (status, headers, _) = raw(&env, req).await;
    assert_eq!(status, StatusCode::OK, "allowed origin proceeds");
    ok.assert();
    assert_eq!(
        headers.get("access-control-allow-origin").unwrap(),
        "https://good.example"
    );
    assert_eq!(headers.get("vary").unwrap(), "Origin");

    // Disallowed origin → 403 gateway problem.
    let mut req = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/cors-api/v1/x")
        .header("origin", "https://evil.example")
        .body(Body::empty())
        .unwrap();
    req.extensions_mut().insert(make_ctx(env.tenant));
    let (status, headers, bytes) = raw(&env, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(headers.get("x-oagw-error-source").unwrap(), "gateway");
    assert_eq!(
        problem_type(&bytes).as_deref(),
        Some(gts::ERR_CORS_ORIGIN_NOT_ALLOWED)
    );
}

#[tokio::test]
async fn cors_preflight_from_any_origin_echoes_origin() {
    let env = build_env(cfg());
    let mut req = Request::builder()
        .method("OPTIONS")
        .uri("/oagw/v1/proxy/a/b")
        .header("origin", "https://other.example")
        .header("access-control-request-method", "DELETE")
        .body(Body::empty())
        .unwrap();
    req.extensions_mut().insert(make_ctx(env.tenant));
    let (status, headers, _) = raw(&env, req).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        headers.get("access-control-allow-origin").unwrap(),
        "https://other.example"
    );
}

// ---------------------------------------------------------------------------
// Rate limiting (ADR 0003)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rate_limit_rejects_after_capacity_with_retry_after() {
    let env = build_env(cfg());
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/v1/rl");
        then.status(200).body("ok");
    });

    let (status, json) = api_json(
        &env,
        "POST",
        "/oagw/v1/upstreams",
        Some(serde_json::json!({
            "enabled": true,
            "alias": "rl-api",
            "server": { "endpoints": [{ "scheme": "http", "host": "127.0.0.1", "port": server.port() }] },
            "protocol": "http",
            "rate_limit": {
                "sustained": { "rate": 1, "window": "minute" },
                "burst": 1,
                "scope": "global",
                "strategy": "reject"
            }
        })),
        env.tenant,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{json}");
    let up = Uuid::parse_str(json["id"].as_str().unwrap()).unwrap();
    create_http_route(&env, env.tenant, up, "/v1", &["GET"]).await;

    // First request consumes the single token.
    let (status, headers, _) = raw(
        &env,
        proxy_request("GET", "/oagw/v1/proxy/rl-api/v1/rl", env.tenant),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    mock.assert_calls(1);
    // Headers reflect the bucket (capacity = sustained rate = 1, window 60s).
    assert_eq!(headers.get("x-ratelimit-limit").unwrap(), "1");
    assert_eq!(headers.get("x-ratelimit-remaining").unwrap(), "1");
    assert_eq!(headers.get("x-ratelimit-reset").unwrap(), "60");

    // Second request → 429 + Retry-After, tagged gateway.
    let (status, headers, bytes) = raw(
        &env,
        proxy_request("GET", "/oagw/v1/proxy/rl-api/v1/rl", env.tenant),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(headers.get("x-oagw-error-source").unwrap(), "gateway");
    assert_eq!(headers.get("retry-after").unwrap(), "60");
    assert_eq!(
        problem_type(&bytes).as_deref(),
        Some(gts::ERR_RATE_LIMIT_EXCEEDED)
    );
    mock.assert_calls(1);
}

// ---------------------------------------------------------------------------
// Built-in plugins through the data plane
// ---------------------------------------------------------------------------

#[tokio::test]
async fn apikey_auth_injected_into_forwarded_request() {
    let env = build_env(cfg());
    let server = MockServer::start();
    let auth_mock = server.mock(|when, then| {
        when.method(GET).path("/v1/secure").header("x-api-key", "sekret");
        then.status(200).body("authed");
    });

    // Passthrough "all" is required for injected credentials to reach the
    // upstream (the default passthrough mode forwards nothing).
    let (status, json) = api_json(
        &env,
        "POST",
        "/oagw/v1/upstreams",
        Some(serde_json::json!({
            "enabled": true,
            "alias": "secure-api",
            "server": { "endpoints": [{ "scheme": "http", "host": "127.0.0.1", "port": server.port() }] },
            "protocol": "http",
            "auth": {
                "type": gts::AUTH_APIKEY,
                "sharing": "private",
                "config": { "header": "X-Api-Key", "key": "sekret" }
            },
            "headers": { "request": { "passthrough": "all" } }
        })),
        env.tenant,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{json}");
    let up = Uuid::parse_str(json["id"].as_str().unwrap()).unwrap();
    create_http_route(&env, env.tenant, up, "/v1", &["GET"]).await;

    let (status, _, bytes) = raw(
        &env,
        proxy_request("GET", "/oagw/v1/proxy/secure-api/v1/secure", env.tenant),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body_text(&bytes), "authed");
    auth_mock.assert();
}

#[tokio::test]
async fn catalog_only_plugin_binding_fails_with_503() {
    // Auth plugin `basic` is catalog-only: binding must fail at proxy time.
    let env = build_env(cfg());
    let (status, json) = api_json(
        &env,
        "POST",
        "/oagw/v1/upstreams",
        Some(serde_json::json!({
            "enabled": true,
            "alias": "basic-api",
            "server": { "endpoints": [{ "scheme": "http", "host": "127.0.0.1", "port": 9 }] },
            "protocol": "http",
            "auth": {
                "type": gts::AUTH_BASIC,
                "sharing": "private",
                "config": {}
            }
        })),
        env.tenant,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{json}");
    let up = Uuid::parse_str(json["id"].as_str().unwrap()).unwrap();
    create_http_route(&env, env.tenant, up, "/", &["GET"]).await;

    let (status, headers, bytes) = raw(
        &env,
        proxy_request("GET", "/oagw/v1/proxy/basic-api/x", env.tenant),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(headers.get("x-oagw-error-source").unwrap(), "gateway");
    assert_eq!(
        problem_type(&bytes).as_deref(),
        Some(gts::ERR_PLUGIN_NOT_FOUND)
    );
}

// ---------------------------------------------------------------------------
// Alias shadowing across the tenant hierarchy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn child_alias_shadows_ancestor() {
    // Root and child tenants host distinct upstreams under the same alias.
    let root = Uuid::new_v4();
    let child = Uuid::new_v4();
    let hierarchy = Arc::new(oagw::infra::storage::tenant_hierarchy::MemoryHierarchy::default().add_edge(child, root));
    let env = build_env_with_hierarchy(cfg(), child, hierarchy);

    let server_root = MockServer::start();
    let mock_root = server_root.mock(|when, then| {
        when.method(GET).path("/v1/who");
        then.status(200).body("root-server");
    });
    let server_child = MockServer::start();
    let mock_child = server_child.mock(|when, then| {
        when.method(GET).path("/v1/who");
        then.status(200).body("child-server");
    });

    // Root defines the shared alias under `inherit` sharing so children may
    // shadow it.
    create_ip_upstream_with_sharing(&env, root, "shared", server_root.port(), "inherit").await;
    let child_up = create_ip_upstream(&env, child, "shared", server_child.port()).await;
    create_http_route(&env, child, child_up, "/v1", &["GET"]).await;

    // The child resolves to its own closest upstream.
    let (status, _, bytes) = raw(
        &env,
        proxy_request("GET", "/oagw/v1/proxy/shared/v1/who", child),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body_text(&bytes), "child-server");
    mock_child.assert();
    mock_root.assert_calls(0);
}

async fn create_ip_upstream_with_sharing(
    env: &TestEnv,
    tenant: Uuid,
    alias: &str,
    port: u16,
    auth_sharing: &str,
) -> Uuid {
    let (status, json) = api_json(
        env,
        "POST",
        "/oagw/v1/upstreams",
        Some(serde_json::json!({
            "enabled": true,
            "alias": alias,
            "server": { "endpoints": [{ "scheme": "http", "host": "127.0.0.1", "port": port }] },
            "protocol": "http",
            "auth": { "type": gts::AUTH_NOOP, "sharing": auth_sharing }
        })),
        tenant,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{json}");
    Uuid::parse_str(json["id"].as_str().unwrap()).unwrap()
}

// ---------------------------------------------------------------------------
// Management API validation (DTO / schema fidelity at the HTTP boundary)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_upstream_ip_without_alias_returns_400_problem() {
    let env = build_env(cfg());
    let (status, json) = api_json(
        &env,
        "POST",
        "/oagw/v1/upstreams",
        Some(serde_json::json!({
            "enabled": true,
            "server": { "endpoints": [{ "scheme": "http", "host": "10.0.1.1", "port": 80 }] },
            "protocol": "http",
        })),
        env.tenant,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json["type"], gts::ERR_VALIDATION);
    assert_eq!(json["status"], 400);
}

#[tokio::test]
async fn create_upstream_user_alias_differing_from_derived_returns_400() {
    let env = build_env(cfg());
    let (status, json) = api_json(
        &env,
        "POST",
        "/oagw/v1/upstreams",
        Some(serde_json::json!({
            "enabled": true,
            "alias": "my-custom-alias",
            "server": { "endpoints": [{ "scheme": "https", "host": "api.openai.com", "port": 443 }] },
            "protocol": "http",
        })),
        env.tenant,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json["type"], gts::ERR_VALIDATION);
    assert!(json["detail"]
        .as_str()
        .unwrap_or_default()
        .contains("auto-derived"));
}

#[tokio::test]
async fn create_route_missing_upstream_id_returns_400() {
    let env = build_env(cfg());
    let (status, json) = api_json(
        &env,
        "POST",
        "/oagw/v1/routes",
        Some(serde_json::json!({
            "match": { "http": { "path": "/v1", "methods": ["GET"] } }
        })),
        env.tenant,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json["type"], gts::ERR_VALIDATION);
}

#[tokio::test]
async fn duplicate_route_conflict_returns_400() {
    let env = build_env(cfg());
    let up = create_ip_upstream(&env, env.tenant, "mock-api", 9).await;
    create_http_route(&env, env.tenant, up, "/", &["GET"]).await;
    let (status, json) = api_json(
        &env,
        "POST",
        "/oagw/v1/routes",
        Some(serde_json::json!({
            "upstream_id": up.to_string(),
            "enabled": true,
            "priority": 0,
            "match": { "http": { "path": "/", "path_suffix_mode": "append", "methods": ["GET"] } }
        })),
        env.tenant,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json["type"], gts::ERR_VALIDATION);
}

#[tokio::test]
async fn upstream_crud_round_trip_with_gts_instance_id_paths() {
    let env = build_env(cfg());
    let up = create_ip_upstream(&env, env.tenant, "mock-api", 9).await;

    // The management API accepts the anonymous GTS instance id in paths.
    let gid = gts::upstream_instance_id(up);
    let (status, json) = api_json(
        &env,
        "GET",
        &format!("/oagw/v1/upstreams/{gid}"),
        None,
        env.tenant,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["id"], up.to_string());
    assert_eq!(json["alias"], "mock-api");

    let (status, list) = api_json(&env, "GET", "/oagw/v1/upstreams", None, env.tenant).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 1);

    let (status, _) = api_json(
        &env,
        "DELETE",
        &format!("/oagw/v1/upstreams/{up}"),
        None,
        env.tenant,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, json) = api_json(
        &env,
        "GET",
        &format!("/oagw/v1/upstreams/{up}"),
        None,
        env.tenant,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json["type"], gts::ERR_VALIDATION);
}

#[tokio::test]
async fn plugin_lifecycle_and_in_use_conflict() {
    let env = build_env(cfg());
    let (status, json) = api_json(
        &env,
        "POST",
        "/oagw/v1/plugins",
        Some(serde_json::json!({
            "name": "my-guard",
            "kind": "guard",
            "source_code": "def main():\n  pass",
            "config_schema": { "type": "object" }
        })),
        env.tenant,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{json}");
    let plugin = json["id"].as_str().unwrap().to_owned();

    let (status, src) = api_json(
        &env,
        "GET",
        &format!("/oagw/v1/plugins/{plugin}/source"),
        None,
        env.tenant,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(src["source"], "def main():\n  pass");

    // Delete while unbound → 204.
    let (status, _) = api_json(
        &env,
        "DELETE",
        &format!("/oagw/v1/plugins/{plugin}"),
        None,
        env.tenant,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Re-create, bind to an upstream, then attempt delete → 409 PluginInUse.
    let (status, json) = api_json(
        &env,
        "POST",
        "/oagw/v1/plugins",
        Some(serde_json::json!({
            "name": "bound-guard",
            "kind": "guard",
            "source_code": "x",
        })),
        env.tenant,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{json}");
    let plugin = json["id"].as_str().unwrap().to_owned();
    let up = create_ip_upstream(&env, env.tenant, "mock-api", 9).await;
    let (status, _) = api_json(
        &env,
        "PUT",
        &format!("/oagw/v1/upstreams/{up}"),
        Some(serde_json::json!({
            "enabled": true,
            "alias": "mock-api",
            "server": { "endpoints": [{ "scheme": "http", "host": "127.0.0.1", "port": 9 }] },
            "protocol": "http",
            "plugins": {
                "sharing": "private",
                "items": [{
                    "plugin_ref": format!("{}{plugin}", gts::GUARD_PLUGIN_TYPE),
                    "plugin_uuid": plugin,
                    "position": 0,
                    "config": null
                }]
            }
        })),
        env.tenant,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "bind plugin to upstream");

    let (status, json) = api_json(
        &env,
        "DELETE",
        &format!("/oagw/v1/plugins/{plugin}"),
        None,
        env.tenant,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(json["type"], gts::ERR_PLUGIN_IN_USE);
}

#[tokio::test]
async fn gts_instance_id_accepted_for_plugin_resources() {
    let env = build_env(cfg());
    let (status, json) = api_json(
        &env,
        "POST",
        "/oagw/v1/plugins",
        Some(serde_json::json!({
            "name": "g",
            "kind": "transform",
            "source_code": "x",
        })),
        env.tenant,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{json}");
    let id = Uuid::parse_str(json["id"].as_str().unwrap()).unwrap();
    let gid = gts::plugin_instance_id(oagw::domain::model::PluginKind::Transform, id);
    let (status, _) = api_json(
        &env,
        "GET",
        &format!("/oagw/v1/plugins/{gid}"),
        None,
        env.tenant,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn cross_tenant_data_is_scoped() {
    let env = build_env(cfg());
    create_ip_upstream(&env, env.tenant, "mock-api", 9).await;
    let other = Uuid::new_v4();
    // The other tenant cannot read or proxy our upstream.
    let (status, json) = api_json(&env, "GET", "/oagw/v1/upstreams", None, other).await;
    assert_eq!(status, StatusCode::OK);
    assert!(json.as_array().unwrap().is_empty());
    let (status, headers, bytes) = raw(
        &env,
        proxy_request("GET", "/oagw/v1/proxy/mock-api/x", other),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(headers.get("x-oagw-error-source").unwrap(), "gateway");
    assert_eq!(
        problem_type(&bytes).as_deref(),
        Some(gts::ERR_ROUTE_NOT_FOUND)
    );
}
