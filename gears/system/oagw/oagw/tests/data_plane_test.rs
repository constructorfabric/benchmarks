#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Data-plane (proxy) integration tests.
//!
//! Exercises `/oagw/v1/proxy/{alias}/{*path}` end to end against a real
//! plain-HTTP axum upstream on `127.0.0.1`, with the frozen e2e configuration
//! (`allow_http_upstream: true`, SSRF disabled). Covers path/query/header
//! forwarding, route matching, CORS, rate limiting, endpoint selection and
//! fail-closed plugin behaviour.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use common::{
    TENANT_A, anonymous_request, build_router, control_with_topology, e2e_config, echo_upstream,
    json_request, request_with_headers, response_json, start_upstream,
};

use credstore_sdk::test_util::MockCredStoreClient;
use oagw::gts;
use oagw::infra::data_plane::DataPlaneService;

/// Build the proxy-ready router: control plane (root/A/B topology), an empty
/// credstore and the frozen e2e data-plane config.
fn router() -> axum::Router {
    let control = control_with_topology();
    let credstore: Arc<dyn credstore_sdk::CredStoreClientV1> =
        Arc::new(MockCredStoreClient::empty());
    let data_plane = Arc::new(
        DataPlaneService::new(control.clone(), credstore, None, e2e_config())
            .expect("data plane builds"),
    );
    build_router(control, data_plane)
}

/// Start the echo upstream and create `alias` -> `127.0.0.1:{port}` under
/// `TENANT_A` (explicit alias: IP endpoints never auto-derive). Returns the
/// upstream uuid and the echo shutdown handle.
async fn setup_echo_upstream(
    router: &axum::Router,
    alias: &str,
) -> (uuid::Uuid, tokio::task::JoinHandle<()>) {
    let (port, handle) = start_upstream(echo_upstream()).await;
    let payload = serde_json::json!({
        "alias": alias,
        "server": { "endpoints": [ { "host": "127.0.0.1", "port": port } ] },
        "protocol": gts::PROTOCOL_HTTP_ID,
    });
    let resp = router
        .clone()
        .oneshot(json_request(
            "POST",
            "/oagw/v1/upstreams",
            Some(payload),
            TENANT_A,
        ))
        .await
        .expect("create upstream");
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = response_json(resp).await;
    let gid = body["id"].as_str().expect("id").to_owned();
    let uuid = gts::parse_resource_id(&gid).expect("parse id");
    (uuid, handle)
}

/// Create a route on `upstream` under `TENANT_A`. `route_body` is the full
/// route payload (`match` plus any optional top-level sections).
async fn create_route(router: &axum::Router, upstream: uuid::Uuid, route_body: serde_json::Value) {
    let mut payload = serde_json::json!({ "upstream_id": upstream.to_string() });
    if let serde_json::Value::Object(map) = &mut payload
        && let serde_json::Value::Object(extra) = route_body
    {
        for (key, value) in extra {
            map.insert(key, value);
        }
    }
    let resp = router
        .clone()
        .oneshot(json_request(
            "POST",
            "/oagw/v1/routes",
            Some(payload),
            TENANT_A,
        ))
        .await
        .expect("create route");
    if resp.status() != StatusCode::CREATED {
        let bytes = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .expect("collect")
            .to_bytes();
        eprintln!("create route rejected: {}", String::from_utf8_lossy(&bytes));
        panic!("create route failed");
    }
}

#[tokio::test]
async fn proxy_forwards_path_query_and_always_headers() {
    let router = router();
    let (upstream, handle) = setup_echo_upstream(&router, "echo").await;
    create_route(
        &router,
        upstream,
        serde_json::json!({
            "match": { "http": { "methods": ["GET"], "path": "/v1", "query_allowlist": ["id"] } }
        }),
    )
    .await;

    // GET /v1/charges with a query: only allowlisted `id` survives.
    let resp = router
        .clone()
        .oneshot(request_with_headers(
            "GET",
            "/oagw/v1/proxy/echo/v1/charges?id=5&utm_source=x",
            TENANT_A,
            &[("accept", "application/json"), ("x-skip-me", "1")],
        ))
        .await
        .expect("proxied request");
    if resp.status() != StatusCode::OK {
        let bytes = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .expect("collect")
            .to_bytes();
        eprintln!("proxy reject: {}", String::from_utf8_lossy(&bytes));
        panic!("proxy failed");
    }
    assert_eq!(resp.status(), StatusCode::OK, "echo upstream answered");
    assert_eq!(
        resp.headers()
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some("upstream"),
        "relayed (success) responses are marked upstream (ADR-0007)"
    );

    let body = response_json(resp).await;
    assert_eq!(body["method"], "GET");
    assert_eq!(body["path"], "/v1/charges");
    assert_eq!(body["query"], "id=5");
    assert_eq!(body["headers"]["accept"], "application/json");
    // Default passthrough is `none`: only always-forward headers survive.
    assert!(body["headers"].get("x-skip-me").is_none());
    // Host is rewritten to the target endpoint.
    let host = body["headers"]["host"].as_str().expect("host header");
    assert!(host.starts_with("127.0.0.1:"), "host rewritten, got {host}");

    handle.abort();
}

#[tokio::test]
async fn unknown_alias_is_route_not_found() {
    let router = router();
    let resp = router
        .clone()
        .oneshot(request_with_headers(
            "GET",
            "/oagw/v1/proxy/nope/v1/x",
            TENANT_A,
            &[],
        ))
        .await
        .expect("proxied request");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let err = response_json(resp).await;
    assert_eq!(err["type"], gts::ERR_ROUTE_NOT_FOUND);
}

#[tokio::test]
async fn method_mismatch_is_route_not_found() {
    let router = router();
    let (upstream, handle) = setup_echo_upstream(&router, "echo").await;
    create_route(
        &router,
        upstream,
        serde_json::json!({ "match": { "http": { "methods": ["POST"], "path": "/v1" } } }),
    )
    .await;

    let resp = router
        .clone()
        .oneshot(request_with_headers(
            "GET",
            "/oagw/v1/proxy/echo/v1/x",
            TENANT_A,
            &[],
        ))
        .await
        .expect("proxied request");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let error_source = resp
        .headers()
        .get("x-oagw-error-source")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let err = response_json(resp).await;
    assert_eq!(err["type"], gts::ERR_ROUTE_NOT_FOUND);
    assert_eq!(error_source.as_deref(), Some("gateway"));

    handle.abort();
}

#[tokio::test]
async fn disabled_upstream_is_link_unavailable() {
    let router = router();
    let (port, handle) = start_upstream(echo_upstream()).await;
    let payload = serde_json::json!({
        "alias": "off",
        "enabled": false,
        "server": { "endpoints": [ { "host": "127.0.0.1", "port": port } ] },
        "protocol": gts::PROTOCOL_HTTP_ID,
    });
    let resp = router
        .clone()
        .oneshot(json_request(
            "POST",
            "/oagw/v1/upstreams",
            Some(payload),
            TENANT_A,
        ))
        .await
        .expect("create upstream");
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = response_json(resp).await;
    let gid = body["id"].as_str().expect("id").to_owned();
    let uuid = gts::parse_resource_id(&gid).expect("parse id");
    create_route(
        &router,
        uuid,
        serde_json::json!({ "match": { "http": { "methods": ["GET"], "path": "/" } } }),
    )
    .await;

    let resp = router
        .clone()
        .oneshot(request_with_headers(
            "GET",
            "/oagw/v1/proxy/off/x",
            TENANT_A,
            &[],
        ))
        .await
        .expect("proxied request");
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let err = response_json(resp).await;
    assert_eq!(err["type"], gts::ERR_LINK_UNAVAILABLE);

    handle.abort();
}

#[tokio::test]
async fn cors_disallowed_origin_is_forbidden() {
    let router = router();
    let (upstream, handle) = setup_echo_upstream(&router, "echo").await;
    // CORS configured on the route (route overrides upstream).
    create_route(
        &router,
        upstream,
        serde_json::json!({
            "match": { "http": { "methods": ["GET"], "path": "/v1" } },
            "cors": {
                "enabled": true,
                "allowed_origins": ["https://good.example"],
                "allowed_methods": ["GET"],
            }
        }),
    )
    .await;

    let resp = router
        .clone()
        .oneshot(request_with_headers(
            "GET",
            "/oagw/v1/proxy/echo/v1/x",
            TENANT_A,
            &[("origin", "https://evil.example")],
        ))
        .await
        .expect("proxied request");
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let err = response_json(resp).await;
    assert_eq!(err["type"], gts::ERR_CORS_ORIGIN);

    // A request without an Origin header is not a CORS request: it passes.
    let resp = router
        .clone()
        .oneshot(request_with_headers(
            "GET",
            "/oagw/v1/proxy/echo/v1/x",
            TENANT_A,
            &[],
        ))
        .await
        .expect("proxied request");
    assert_eq!(resp.status(), StatusCode::OK);

    handle.abort();
}

#[tokio::test]
async fn rate_limit_rejects_after_burst() {
    let router = router();
    let (upstream, _handle) = setup_echo_upstream(&router, "echo").await;
    create_route(
        &router,
        upstream,
        serde_json::json!({
            "match": { "http": { "methods": ["GET"], "path": "/" } },
            "rate_limit": {
                "sustained": { "rate": 1, "window": "second" },
                "burst": { "capacity": 2 },
            }
        }),
    )
    .await;

    let uri = "/oagw/v1/proxy/echo/x";
    for _ in 0..2 {
        let resp = router
            .clone()
            .oneshot(request_with_headers("GET", uri, TENANT_A, &[]))
            .await
            .expect("proxied request");
        assert_eq!(resp.status(), StatusCode::OK, "burst allowed");
        assert!(resp.headers().contains_key("x-ratelimit-limit"));
        assert!(resp.headers().contains_key("x-ratelimit-remaining"));
    }

    // Third request exhausts the burst -> 429.
    let resp = router
        .clone()
        .oneshot(request_with_headers("GET", uri, TENANT_A, &[]))
        .await
        .expect("proxied request");
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(resp.headers().get("retry-after").is_some());
    assert!(resp.headers().get("x-ratelimit-limit").is_some());
    assert!(resp.headers().get("x-ratelimit-remaining").is_some());
    let error_source = resp
        .headers()
        .get("x-oagw-error-source")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let err = response_json(resp).await;
    assert_eq!(err["type"], gts::ERR_RATE_LIMIT);
    assert_eq!(err["status"], 429);
    assert_eq!(error_source.as_deref(), Some("gateway"));
    // The RFC 9457 problem carries the readiness data too.
    assert_eq!(err["rate_limit_limit"], 2, "burst capacity surfaced");
    assert_eq!(err["rate_limit_remaining"], 0);
}

#[tokio::test]
async fn multi_endpoint_hostname_pool_requires_target_host() {
    let router = router();
    // Two hostname endpoints share a registrable suffix -> alias "vendor.com",
    // but without `X-OAGW-Target-Host` the gateway refuses (ADR-0001).
    let payload = serde_json::json!({
        "server": { "endpoints": [
            { "host": "us.vendor.com", "port": 443 },
            { "host": "eu.vendor.com", "port": 443 },
        ]},
        "protocol": gts::PROTOCOL_HTTP_ID,
    });
    let resp = router
        .clone()
        .oneshot(json_request(
            "POST",
            "/oagw/v1/upstreams",
            Some(payload),
            TENANT_A,
        ))
        .await
        .expect("create upstream");
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = response_json(resp).await;
    assert_eq!(body["alias"], "vendor.com");
    let gid = body["id"].as_str().expect("id").to_owned();
    let uuid = gts::parse_resource_id(&gid).expect("parse id");
    create_route(
        &router,
        uuid,
        serde_json::json!({ "match": { "http": { "methods": ["GET"], "path": "/v1" } } }),
    )
    .await;

    let resp = router
        .clone()
        .oneshot(request_with_headers(
            "GET",
            "/oagw/v1/proxy/vendor.com/v1/ping",
            TENANT_A,
            &[],
        ))
        .await
        .expect("proxied request");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let err = response_json(resp).await;
    assert_eq!(err["type"], gts::ERR_MISSING_TARGET_HOST);
}

#[tokio::test]
async fn header_mutations_apply_both_directions() {
    let router = router();
    let (port, handle) = start_upstream(echo_upstream()).await;

    // Header rules live on the upstream, so create it with the rules inline.
    let payload = serde_json::json!({
        "alias": "echo",
        "server": { "endpoints": [ { "host": "127.0.0.1", "port": port } ] },
        "protocol": gts::PROTOCOL_HTTP_ID,
        "headers": {
            "request": {
                "passthrough": "all",
                "set": { "x-oagw-forwarded": "yes" },
                "remove": ["x-drop-me"],
            },
            "response": {
                "set": { "x-oagw-response": "set" },
            }
        }
    });
    let resp = router
        .clone()
        .oneshot(json_request(
            "POST",
            "/oagw/v1/upstreams",
            Some(payload),
            TENANT_A,
        ))
        .await
        .expect("create upstream");
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = response_json(resp).await;
    let gid = body["id"].as_str().expect("id").to_owned();
    let upstream = gts::parse_resource_id(&gid).expect("parse id");

    create_route(
        &router,
        upstream,
        serde_json::json!({ "match": { "http": { "methods": ["GET"], "path": "/" } } }),
    )
    .await;

    let resp = router
        .clone()
        .oneshot(request_with_headers(
            "GET",
            "/oagw/v1/proxy/echo/ping",
            TENANT_A,
            &[("x-drop-me", "gone"), ("x-keep-me", "kept")],
        ))
        .await
        .expect("proxied request");
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("x-oagw-response")
            .and_then(|v| v.to_str().ok()),
        Some("set")
    );

    let body = response_json(resp).await;
    // Passthrough "all": arbitrary headers forward.
    assert_eq!(body["headers"]["x-keep-me"], "kept");
    // Remove list drops inbound headers.
    assert!(body["headers"].get("x-drop-me").is_none());
    // Set overwrites on the outbound request.
    assert_eq!(body["headers"]["x-oagw-forwarded"], "yes");
    // Host always rewritten to the target endpoint.
    let host = body["headers"]["host"].as_str().expect("host header");
    assert!(host.starts_with("127.0.0.1:"), "host rewritten, got {host}");

    handle.abort();
}

#[tokio::test]
async fn request_id_transform_injects_headers() {
    let router = router();
    let (upstream, handle) = setup_echo_upstream(&router, "echo").await;
    // Bind the built-in request-id transform on the route.
    create_route(
        &router,
        upstream,
        serde_json::json!({
            "match": { "http": { "methods": ["GET"], "path": "/" } },
            "plugins": { "items": [ gts::TRANSFORM_REQUEST_ID_ID ] },
        }),
    )
    .await;

    let resp = router
        .clone()
        .oneshot(request_with_headers(
            "GET",
            "/oagw/v1/proxy/echo/ping",
            TENANT_A,
            &[],
        ))
        .await
        .expect("proxied request");
    assert_eq!(resp.status(), StatusCode::OK);
    // Response phase transformed the response headers.
    let rid = resp
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .expect("response x-request-id");
    assert!(!rid.is_empty());

    // Request phase transformed the outbound request headers.
    let body = response_json(resp).await;
    let out_rid = body["headers"]["x-request-id"]
        .as_str()
        .expect("outbound x-request-id");
    assert!(!out_rid.is_empty());

    // One stable id per request: the outbound hop and the returned response
    // share the exact same value (seeded once in the data plane).
    assert_eq!(
        out_rid,
        rid.as_str(),
        "request and response share one stable x-request-id"
    );

    handle.abort();
}

#[tokio::test]
async fn proxy_preflight_is_anonymous_and_permissive() {
    let router = router();

    // A bare anonymous OPTIONS (no tenant, no origin) is answered 204 — the
    // preflight route carries no auth (ADR-0004).
    let resp = router
        .clone()
        .oneshot(anonymous_request("OPTIONS", "/oagw/v1/proxy/echo/v1/x"))
        .await
        .expect("preflight");
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // A real preflight echoes the requested origin/method/headers permissively.
    let req = Request::builder()
        .method("OPTIONS")
        .uri("/oagw/v1/proxy/echo/v1/x")
        .header("origin", "https://app.example")
        .header("access-control-request-method", "POST")
        .header("access-control-request-headers", "x-requested-with")
        .body(Body::empty())
        .expect("valid preflight");
    let resp = router.clone().oneshot(req).await.expect("preflight");
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        resp.headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("https://app.example")
    );
    assert_eq!(
        resp.headers()
            .get("access-control-allow-methods")
            .and_then(|v| v.to_str().ok()),
        Some("POST")
    );
    assert_eq!(
        resp.headers()
            .get("access-control-allow-headers")
            .and_then(|v| v.to_str().ok()),
        Some("x-requested-with")
    );
}

/// Build the router with the e2e config but with the SSRF policy enabled, so
/// the proxy-time target check is exercised against a loopback upstream.
fn router_with_ssrf_enabled() -> axum::Router {
    let control = control_with_topology();
    let credstore: Arc<dyn credstore_sdk::CredStoreClientV1> =
        Arc::new(MockCredStoreClient::empty());
    let mut config = e2e_config();
    config.ssrf_policy.enabled = true;
    let data_plane = Arc::new(
        DataPlaneService::new(control.clone(), credstore, None, config).expect("data plane builds"),
    );
    build_router(control, data_plane)
}

#[tokio::test]
async fn target_host_matrix_invalid_and_unknown() {
    let router = router();
    let payload = serde_json::json!({
        "server": { "endpoints": [
            { "host": "us.vendor.com", "port": 443 },
            { "host": "eu.vendor.com", "port": 443 },
        ]},
        "protocol": gts::PROTOCOL_HTTP_ID,
    });
    let resp = router
        .clone()
        .oneshot(json_request(
            "POST",
            "/oagw/v1/upstreams",
            Some(payload),
            TENANT_A,
        ))
        .await
        .expect("create upstream");
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = response_json(resp).await;
    let upstream = gts::parse_resource_id(body["id"].as_str().expect("id")).expect("parse id");
    create_route(
        &router,
        upstream,
        serde_json::json!({ "match": { "http": { "methods": ["GET"], "path": "/v1" } } }),
    )
    .await;

    // A syntactically invalid target host is rejected up front.
    let resp = router
        .clone()
        .oneshot(request_with_headers(
            "GET",
            "/oagw/v1/proxy/vendor.com/v1/ping",
            TENANT_A,
            &[("x-oagw-target-host", "not a host!")],
        ))
        .await
        .expect("proxied request");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let err = response_json(resp).await;
    assert_eq!(err["type"], gts::ERR_INVALID_TARGET_HOST);

    // A well-formed host outside the pool is unknown for this upstream.
    let resp = router
        .clone()
        .oneshot(request_with_headers(
            "GET",
            "/oagw/v1/proxy/vendor.com/v1/ping",
            TENANT_A,
            &[("x-oagw-target-host", "other.example.org")],
        ))
        .await
        .expect("proxied request");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let err = response_json(resp).await;
    assert_eq!(err["type"], gts::ERR_UNKNOWN_TARGET_HOST);
}

#[tokio::test]
async fn proxy_alias_is_tenant_scoped() {
    let router = router();
    // `echo` is owned by TENANT_A (records an endpoint; never reached).
    let payload = serde_json::json!({
        "alias": "echo",
        "server": { "endpoints": [ { "host": "127.0.0.1", "port": 9 } ] },
        "protocol": gts::PROTOCOL_HTTP_ID,
    });
    let resp = router
        .clone()
        .oneshot(json_request(
            "POST",
            "/oagw/v1/upstreams",
            Some(payload),
            TENANT_A,
        ))
        .await
        .expect("create upstream");
    assert_eq!(resp.status(), StatusCode::CREATED);

    // TENANT_B shares TENANT_A's ancestors but not its upstreams: the alias
    // must not resolve through a sibling.
    let resp = router
        .clone()
        .oneshot(request_with_headers(
            "GET",
            "/oagw/v1/proxy/echo/v1/x",
            common::TENANT_B,
            &[],
        ))
        .await
        .expect("proxied request");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let err = response_json(resp).await;
    assert_eq!(err["type"], gts::ERR_ROUTE_NOT_FOUND);
}

#[tokio::test]
async fn ancestor_enforced_rate_limit_shadows_descendant() {
    let router = router();
    // Root enforces a tenant-scoped limit (burst 2) on alias `shared`.
    let (port, handle) = start_upstream(echo_upstream()).await;
    let root_payload = serde_json::json!({
        "alias": "shared",
        "server": { "endpoints": [ { "host": "127.0.0.1", "port": port } ] },
        "protocol": gts::PROTOCOL_HTTP_ID,
        "rate_limit": {
            "sharing": "enforce",
            "scope": "tenant",
            "sustained": { "rate": 1, "window": "second" },
            "burst": { "capacity": 2 },
        }
    });
    let resp = router
        .clone()
        .oneshot(json_request(
            "POST",
            "/oagw/v1/upstreams",
            Some(root_payload),
            common::TENANT_ROOT,
        ))
        .await
        .expect("create root upstream");
    assert_eq!(resp.status(), StatusCode::CREATED);

    // TENANT_A shadows the root alias with its own upstream that sets no
    // rate limit — the enforced ancestor policy must still apply.
    let payload = serde_json::json!({
        "alias": "shared",
        "server": { "endpoints": [ { "host": "127.0.0.1", "port": port } ] },
        "protocol": gts::PROTOCOL_HTTP_ID,
    });
    let resp = router
        .clone()
        .oneshot(json_request(
            "POST",
            "/oagw/v1/upstreams",
            Some(payload),
            TENANT_A,
        ))
        .await
        .expect("create descendant upstream");
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = response_json(resp).await;
    let upstream = gts::parse_resource_id(body["id"].as_str().expect("id")).expect("parse id");
    create_route(
        &router,
        upstream,
        serde_json::json!({ "match": { "http": { "methods": ["GET"], "path": "/" } } }),
    )
    .await;

    let uri = "/oagw/v1/proxy/shared/x";
    for _ in 0..2 {
        let resp = router
            .clone()
            .oneshot(request_with_headers("GET", uri, TENANT_A, &[]))
            .await
            .expect("proxied request");
        assert_eq!(resp.status(), StatusCode::OK, "ancestor burst allows");
    }
    let resp = router
        .clone()
        .oneshot(request_with_headers("GET", uri, TENANT_A, &[]))
        .await
        .expect("proxied request");
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    let err = response_json(resp).await;
    assert_eq!(err["type"], gts::ERR_RATE_LIMIT);
    assert_eq!(err["rate_limit_limit"], 2, "enforced ancestor capacity");
    assert_eq!(err["rate_limit_remaining"], 0);

    handle.abort();
}

#[tokio::test]
async fn cors_enriched_response_for_allowed_origin() {
    let router = router();
    let (upstream, handle) = setup_echo_upstream(&router, "echo").await;
    create_route(
        &router,
        upstream,
        serde_json::json!({
            "match": { "http": { "methods": ["GET"], "path": "/" } },
            "cors": {
                "enabled": true,
                "allowed_origins": ["https://app.example"],
                "allowed_methods": ["GET"],
                "expose_headers": ["x-trace-id"],
            }
        }),
    )
    .await;

    let resp = router
        .clone()
        .oneshot(request_with_headers(
            "GET",
            "/oagw/v1/proxy/echo/v1/x",
            TENANT_A,
            &[("origin", "https://app.example")],
        ))
        .await
        .expect("proxied request");
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("https://app.example")
    );
    assert_eq!(
        resp.headers()
            .get("access-control-expose-headers")
            .and_then(|v| v.to_str().ok()),
        Some("x-trace-id")
    );

    handle.abort();
}

#[tokio::test]
async fn stalled_upstream_becomes_request_timeout() {
    let router = router();
    // Accept connections but never answer: the proxy budget (2s in the e2e
    // config) must fire and surface ERR_REQUEST_TIMEOUT.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let hold = tokio::spawn(async move {
        let mut open: Vec<tokio::net::TcpStream> = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            open.push(stream); // hold open, stay silent
        }
    });

    let payload = serde_json::json!({
        "alias": "echo",
        "server": { "endpoints": [ { "host": "127.0.0.1", "port": addr.port() } ] },
        "protocol": gts::PROTOCOL_HTTP_ID,
    });
    let resp = router
        .clone()
        .oneshot(json_request(
            "POST",
            "/oagw/v1/upstreams",
            Some(payload),
            TENANT_A,
        ))
        .await
        .expect("create upstream");
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = response_json(resp).await;
    let upstream = gts::parse_resource_id(body["id"].as_str().expect("id")).expect("parse id");
    create_route(
        &router,
        upstream,
        serde_json::json!({ "match": { "http": { "methods": ["GET"], "path": "/" } } }),
    )
    .await;

    let resp = router
        .clone()
        .oneshot(request_with_headers(
            "GET",
            "/oagw/v1/proxy/echo/x",
            TENANT_A,
            &[],
        ))
        .await
        .expect("proxied request");
    assert_eq!(resp.status(), StatusCode::GATEWAY_TIMEOUT);
    let error_source = resp
        .headers()
        .get("x-oagw-error-source")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let err = response_json(resp).await;
    assert_eq!(err["type"], gts::ERR_REQUEST_TIMEOUT);
    assert_eq!(error_source.as_deref(), Some("gateway"));

    hold.abort();
}

#[tokio::test]
async fn grpc_upstream_is_rejected_not_proxied() {
    let router = router();
    let payload = serde_json::json!({
        "alias": "grpc",
        "server": { "endpoints": [ { "host": "127.0.0.1", "port": 443, "scheme": "grpc" } ] },
        "protocol": gts::PROTOCOL_GRPC_ID,
    });
    let resp = router
        .clone()
        .oneshot(json_request(
            "POST",
            "/oagw/v1/upstreams",
            Some(payload),
            TENANT_A,
        ))
        .await
        .expect("create upstream");
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = router
        .clone()
        .oneshot(request_with_headers(
            "GET",
            "/oagw/v1/proxy/grpc/service/method",
            TENANT_A,
            &[],
        ))
        .await
        .expect("proxied request");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let error_source = resp
        .headers()
        .get("x-oagw-error-source")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let err = response_json(resp).await;
    assert_eq!(err["type"], gts::ERR_VALIDATION);
    assert!(
        err["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("not implemented"),
        "detail: {err}"
    );
    assert_eq!(error_source.as_deref(), Some("gateway"));
}

#[tokio::test]
async fn non_get_method_and_body_are_proxied() {
    let router = router();
    let (upstream, handle) = setup_echo_upstream(&router, "echo").await;
    create_route(
        &router,
        upstream,
        serde_json::json!({
            "match": { "http": { "methods": ["GET", "POST"], "path": "/" } }
        }),
    )
    .await;

    let req = Request::builder()
        .method("POST")
        .uri("/oagw/v1/proxy/echo/orders")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"items":2}"#))
        .expect("valid request");
    let mut req = req;
    req.extensions_mut().insert(common::make_security(TENANT_A));
    let resp = router.clone().oneshot(req).await.expect("proxied request");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = response_json(resp).await;
    assert_eq!(body["method"], "POST");
    assert_eq!(body["path"], "/orders");
    assert_eq!(body["body"], r#"{"items":2}"#);

    handle.abort();
}

#[tokio::test]
async fn disabled_route_is_skipped_in_favour_of_enabled() {
    let router = router();
    let (upstream, handle) = setup_echo_upstream(&router, "echo").await;
    // A disabled route with a higher priority must lose to the enabled one.
    let disabled = serde_json::json!({
        "match": { "http": { "methods": ["GET"], "path": "/" } },
        "enabled": false,
        "priority": 100,
    });
    create_route(&router, upstream, disabled).await;
    let enabled = serde_json::json!({
        "match": { "http": { "methods": ["GET"], "path": "/" } },
        "enabled": true,
        "priority": 1,
        "rate_limit": {
            "sustained": { "rate": 1, "window": "second" },
            "burst": { "capacity": 7 },
        }
    });
    create_route(&router, upstream, enabled).await;

    let resp = router
        .clone()
        .oneshot(request_with_headers(
            "GET",
            "/oagw/v1/proxy/echo/ping",
            TENANT_A,
            &[],
        ))
        .await
        .expect("proxied request");
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("x-ratelimit-limit")
            .and_then(|v| v.to_str().ok()),
        Some("7"),
        "the enabled route's policy won the match"
    );

    handle.abort();
}

#[tokio::test]
async fn higher_priority_route_wins() {
    let router = router();
    let (upstream, handle) = setup_echo_upstream(&router, "echo").await;
    // Two enabled routes with the same match: the explicit priority decides.
    let low = serde_json::json!({
        "match": { "http": { "methods": ["GET"], "path": "/" } },
        "priority": 1,
        "rate_limit": {
            "sustained": { "rate": 1, "window": "second" },
            "burst": { "capacity": 3 },
        }
    });
    create_route(&router, upstream, low).await;
    let high = serde_json::json!({
        "match": { "http": { "methods": ["GET"], "path": "/" } },
        "priority": 5,
        "rate_limit": {
            "sustained": { "rate": 1, "window": "second" },
            "burst": { "capacity": 9 },
        }
    });
    create_route(&router, upstream, high).await;

    let resp = router
        .clone()
        .oneshot(request_with_headers(
            "GET",
            "/oagw/v1/proxy/echo/ping",
            TENANT_A,
            &[],
        ))
        .await
        .expect("proxied request");
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("x-ratelimit-limit")
            .and_then(|v| v.to_str().ok()),
        Some("9"),
        "the higher-priority route's policy was applied"
    );

    handle.abort();
}

#[tokio::test]
async fn ssrf_policy_blocks_loopback_upstream_at_proxy() {
    let router = router_with_ssrf_enabled();
    // The target is 127.0.0.1 — blocked by policy before any connection is
    // attempted, so no upstream server is needed.
    let payload = serde_json::json!({
        "alias": "echo",
        "server": { "endpoints": [ { "host": "127.0.0.1", "port": 9 } ] },
        "protocol": gts::PROTOCOL_HTTP_ID,
    });
    let resp = router
        .clone()
        .oneshot(json_request(
            "POST",
            "/oagw/v1/upstreams",
            Some(payload),
            TENANT_A,
        ))
        .await
        .expect("create upstream");
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = response_json(resp).await;
    let upstream = gts::parse_resource_id(body["id"].as_str().expect("id")).expect("parse id");
    create_route(
        &router,
        upstream,
        serde_json::json!({ "match": { "http": { "methods": ["GET"], "path": "/" } } }),
    )
    .await;

    let resp = router
        .clone()
        .oneshot(request_with_headers(
            "GET",
            "/oagw/v1/proxy/echo/x",
            TENANT_A,
            &[],
        ))
        .await
        .expect("proxied request");
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    let error_source = resp
        .headers()
        .get("x-oagw-error-source")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let err = response_json(resp).await;
    assert_eq!(err["type"], gts::ERR_PROTOCOL);
    assert!(
        err["detail"].as_str().unwrap_or_default().contains("SSRF"),
        "detail: {err}"
    );
    assert_eq!(error_source.as_deref(), Some("gateway"));
}

/// Test-only middleware that materializes the `SecurityContext` extension
/// from an `x-test-tenant` header, mirroring what the edge injects in
/// production. Needed because a real hyper server (which installs the
/// server-side `OnUpgrade` into the request extensions) cannot otherwise
/// authenticate the proxy route.
async fn inject_security_from_header(
    req: Request<Body>,
    next: axum::middleware::Next,
) -> axum::response::Response<Body> {
    let tenant = req
        .headers()
        .get("x-test-tenant")
        .and_then(|v| v.to_str().ok())
        .and_then(|t| uuid::Uuid::parse_str(t).ok());
    let mut req = req;
    if let Some(tenant) = tenant {
        req.extensions_mut().insert(common::make_security(tenant));
    }
    next.run(req).await
}

#[tokio::test]
async fn websocket_declined_by_upstream_is_relayed() {
    let router = router();
    let (upstream, handle) = setup_echo_upstream(&router, "echo").await;
    create_route(
        &router,
        upstream,
        serde_json::json!({ "match": { "http": { "methods": ["GET"], "path": "/" } } }),
    )
    .await;

    // A browser-style upgrade handshake must reach a real server so hyper
    // installs the `OnUpgrade` handle; serve the OAGW router on a socket.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind gateway");
    let addr = listener.local_addr().expect("gateway addr");
    let app = router.layer(axum::middleware::from_fn(inject_security_from_header));
    let served = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("gateway serves");
    });

    let mut stream = tokio::net::TcpStream::connect(addr)
        .await
        .expect("connect gateway");
    let raw = format!(
        "GET /oagw/v1/proxy/echo/socket HTTP/1.1\r\n\
         host: {addr}\r\n\
         x-test-tenant: {TENANT_A}\r\n\
         connection: Upgrade\r\n\
         upgrade: websocket\r\n\
         sec-websocket-key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         sec-websocket-version: 13\r\n\r\n"
    );
    tokio::io::AsyncWriteExt::write_all(&mut stream, raw.as_bytes())
        .await
        .expect("send handshake");

    // Read the relayed response: headers, then the content-length body.
    let mut buf = Vec::new();
    let mut chunk = [0_u8; 2048];
    let headers_end = b"\r\n\r\n";
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let n = tokio::io::AsyncReadExt::read(&mut stream, &mut chunk)
                .await
                .expect("read response");
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(head_end) = find_bytes(&buf, headers_end) {
                let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
                let need = content_length_after(&head);
                if let Some(need) = need
                    && buf.len() >= head_end + 4 + need
                {
                    break;
                }
                if need.is_none() {
                    break;
                }
            }
        }
    })
    .await
    .expect("response within deadline");

    let text = String::from_utf8_lossy(&buf);
    assert!(
        text.starts_with("HTTP/1.1 200 OK"),
        "declined upgrade is relayed as the upstream 200, got: {text}"
    );
    assert!(
        text.contains("x-oagw-error-source: upstream"),
        "relayed responses are flagged upstream, got: {text}"
    );
    assert!(
        text.contains("upgrade\":\"websocket"),
        "the handshake headers reached the upstream, got: {text}"
    );

    served.abort();
    handle.abort();
}

/// Index of the first occurrence of `needle` in `haystack`, or None.
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Parse a `Content-Length` header value from a response head.
fn content_length_after(head: &str) -> Option<usize> {
    for line in head.lines() {
        if let Some(value) = line.strip_prefix("content-length:") {
            return value.trim().parse().ok();
        }
    }
    None
}

#[tokio::test]
async fn bound_custom_plugin_fails_closed() {
    let router = router();
    let (upstream, handle) = setup_echo_upstream(&router, "echo").await;

    // A custom (Starlark) plugin is stored and validatable but not executable:
    // binding one fails the proxy closed with 503 PluginNotFound.
    let plugin = serde_json::json!({
        "name": "transform-marker",
        "plugin_type": "transform",
        "config_schema": { "type": "object" },
        "source_code": "def handle(ctx):\n    return None\n",
    });
    let resp = router
        .clone()
        .oneshot(json_request(
            "POST",
            "/oagw/v1/plugins",
            Some(plugin),
            TENANT_A,
        ))
        .await
        .expect("create plugin");
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = response_json(resp).await;
    let plugin_gid = body["id"].as_str().expect("id").to_owned();

    create_route(
        &router,
        upstream,
        serde_json::json!({
            "match": { "http": { "methods": ["GET"], "path": "/" } },
            "plugins": { "items": [ plugin_gid ] },
        }),
    )
    .await;

    let resp = router
        .clone()
        .oneshot(request_with_headers(
            "GET",
            "/oagw/v1/proxy/echo/ping",
            TENANT_A,
            &[],
        ))
        .await
        .expect("proxied request");
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let error_source = resp
        .headers()
        .get("x-oagw-error-source")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let err = response_json(resp).await;
    assert_eq!(err["type"], gts::ERR_PLUGIN_NOT_FOUND);
    assert_eq!(error_source.as_deref(), Some("gateway"));

    handle.abort();
}
