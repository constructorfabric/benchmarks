//! Integration tests over the full router: the proxy data plane.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use http::{Method, StatusCode};
use httpmock::prelude::*;
use serde_json::json;

/// A gateway wired to a permissive configuration and a live mock upstream.
struct ProxyFixture {
    harness: Harness,
    upstream: MockUpstream,
    alias: &'static str,
}

impl ProxyFixture {
    /// Build the fixture and register the upstream with a route to `/echo`.
    async fn new() -> Self {
        Self::with_upstream_body(|upstream| upstream).await
    }

    /// Build the fixture with a mutated upstream document.
    async fn with_upstream_body(
        mutate: impl FnOnce(serde_json::Value) -> serde_json::Value,
    ) -> Self {
        let harness = Harness::with_config(permissive_config(), None);
        let upstream = MockUpstream::start();
        let body = mutate(upstream.upstream_body("local"));
        let created = create_upstream(&harness, body).await;
        create_route(
            &harness,
            route_body_with(created["id"].as_str().unwrap(), &["GET", "POST"], "/echo"),
        )
        .await;
        Self {
            harness,
            upstream,
            alias: "local",
        }
    }

    /// The mock upstream address.
    fn addr(&self) -> std::net::SocketAddr {
        self.upstream.addr
    }
}

/// A `GET` through the proxy reaches the upstream and the body comes back.
#[tokio::test]
async fn get_is_proxied_to_the_upstream() {
    let fixture = ProxyFixture::new().await;
    let response = fixture
        .harness
        .send(Method::GET, &proxy_path(fixture.alias, "/echo"), None)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = fixture.harness.json(response).await;
    assert_eq!(body["method"], "GET");
    assert_eq!(body["path"], "/echo");
    assert_eq!(
        body["headers"]["host"],
        format!("127.0.0.1:{}", fixture.addr().port())
    );
}

/// Query strings survive the hop.
#[tokio::test]
async fn query_strings_are_forwarded() {
    let fixture = ProxyFixture::new().await;
    let response = fixture
        .harness
        .send(
            Method::GET,
            &proxy_path(fixture.alias, "/echo?alpha=1&beta=two"),
            None,
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = fixture.harness.json(response).await;
    assert_eq!(body["query"], "alpha=1&beta=two");
}

/// `POST` bodies are forwarded byte for byte.
#[tokio::test]
async fn post_bodies_are_forwarded() {
    let fixture = ProxyFixture::new().await;
    let response = fixture
        .harness
        .send(
            Method::POST,
            &proxy_path(fixture.alias, "/echo"),
            Some("{\"hello\":\"world\"}".to_owned()),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = fixture.harness.json(response).await;
    assert_eq!(body["method"], "POST");
    assert_eq!(body["body"], "{\"hello\":\"world\"}");
}

/// A successful relay marks the response as coming from the upstream.
#[tokio::test]
async fn upstream_responses_carry_the_upstream_source() {
    let fixture = ProxyFixture::new().await;
    let response = fixture
        .harness
        .send(Method::GET, &proxy_path(fixture.alias, "/echo"), None)
        .await;
    assert_eq!(response.headers()["x-oagw-error-source"], "upstream");
}

/// An unknown alias is a 404 route-not-found problem.
#[tokio::test]
async fn unknown_alias_is_not_found() {
    let fixture = ProxyFixture::new().await;
    let response = fixture
        .harness
        .send(Method::GET, &proxy_path("nobody", "/echo"), None)
        .await;
    let problem = fixture
        .harness
        .expect_problem(response, StatusCode::NOT_FOUND)
        .await;
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
    );
    assert_eq!(problem["path"], "/oagw/v1/proxy/nobody/echo");
}

/// A path no route claims is a 404.
#[tokio::test]
async fn unmatched_paths_are_not_found() {
    let fixture = ProxyFixture::new().await;
    let response = fixture
        .harness
        .send(Method::GET, &proxy_path(fixture.alias, "/nowhere"), None)
        .await;
    fixture
        .harness
        .expect_problem(response, StatusCode::NOT_FOUND)
        .await;
}

/// A method outside the route allowlist is a 400 validation problem.
#[tokio::test]
async fn disallowed_methods_are_rejected() {
    let fixture = ProxyFixture::new().await;
    let response = fixture
        .harness
        .send(Method::DELETE, &proxy_path(fixture.alias, "/echo"), None)
        .await;
    let problem = fixture
        .harness
        .expect_problem(response, StatusCode::BAD_REQUEST)
        .await;
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
}

/// A multi-endpoint pool demands the target host header.
#[tokio::test]
async fn multi_endpoint_pools_require_the_target_host() {
    let harness = Harness::with_config(permissive_config(), None);
    let upstream = MockUpstream::start();
    let port = upstream.addr.port();
    let created = create_upstream(
        &harness,
        json!({
            "alias": "multi",
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "server": { "endpoints": [
                { "scheme": "http", "host": "127.0.0.1", "port": port },
                { "scheme": "http", "host": "localhost", "port": port }
            ] }
        }),
    )
    .await;
    create_route(
        &harness,
        route_body(created["id"].as_str().unwrap(), "/echo"),
    )
    .await;

    let response = harness
        .send(Method::GET, &proxy_path("multi", "/echo"), None)
        .await;
    let problem = harness
        .expect_problem(response, StatusCode::BAD_REQUEST)
        .await;
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1"
    );

    let headers = [("x-oagw-target-host", "localhost")];
    let response = harness
        .send_raw(Method::GET, &proxy_path("multi", "/echo"), &headers, None)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
}

/// An unknown target host is rejected.
#[tokio::test]
async fn unknown_target_hosts_are_rejected() {
    let harness = Harness::with_config(permissive_config(), None);
    let upstream = MockUpstream::start();
    let port = upstream.addr.port();
    let created = create_upstream(
        &harness,
        json!({
            "alias": "multi",
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "server": { "endpoints": [
                { "scheme": "http", "host": "127.0.0.1", "port": port },
                { "scheme": "http", "host": "localhost", "port": port }
            ] }
        }),
    )
    .await;
    create_route(
        &harness,
        route_body(created["id"].as_str().unwrap(), "/echo"),
    )
    .await;

    let headers = [("x-oagw-target-host", "203.0.113.9")];
    let response = harness
        .send_raw(Method::GET, &proxy_path("multi", "/echo"), &headers, None)
        .await;
    let problem = harness
        .expect_problem(response, StatusCode::BAD_REQUEST)
        .await;
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1"
    );
}

/// A malformed target host is rejected before endpoint selection.
#[tokio::test]
async fn malformed_target_hosts_are_rejected() {
    let fixture = ProxyFixture::new().await;
    let headers = [("x-oagw-target-host", "evil.example.com/../etc")];
    let response = fixture
        .harness
        .send_raw(
            Method::GET,
            &proxy_path(fixture.alias, "/echo"),
            &headers,
            None,
        )
        .await;
    let problem = fixture
        .harness
        .expect_problem(response, StatusCode::BAD_REQUEST)
        .await;
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1"
    );
}

/// The fourth request is rejected once the one-token bucket is drained.
#[tokio::test]
async fn rate_limits_return_429_with_headers() {
    let harness = Harness::with_config(permissive_config(), None);
    let upstream = MockUpstream::start();
    let body = json!({
        "alias": "limited",
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "server": { "endpoints": [ upstream.endpoint() ] },
        "rate_limit": {
            "sustained": { "rate": 1, "window": "second" },
            "burst": { "capacity": 1 }
        }
    });
    let created = create_upstream(&harness, body).await;
    create_route(
        &harness,
        route_body(created["id"].as_str().unwrap(), "/echo"),
    )
    .await;

    let allowed = harness
        .send(Method::GET, &proxy_path("limited", "/echo"), None)
        .await;
    assert_eq!(allowed.status(), StatusCode::OK);
    assert_eq!(allowed.headers()["x-ratelimit-limit"], "1");
    assert_eq!(allowed.headers()["x-ratelimit-remaining"], "0");

    let rejected = harness
        .send(Method::GET, &proxy_path("limited", "/echo"), None)
        .await;
    assert_eq!(rejected.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(rejected.headers()["retry-after"], "1");
    assert_eq!(rejected.headers()["x-ratelimit-limit"], "1");
    assert_eq!(rejected.headers()["x-ratelimit-remaining"], "0");
    assert_eq!(rejected.headers()["x-oagw-error-source"], "gateway");
    let problem = harness.json(rejected).await;
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1"
    );
}

/// Preflights are answered locally with a permissive 204.
#[tokio::test]
async fn preflights_are_answered_without_reaching_the_upstream() {
    let fixture = ProxyFixture::new().await;
    let headers = [
        ("origin", "https://app.example.com"),
        ("access-control-request-method", "POST"),
        ("access-control-request-headers", "content-type"),
    ];
    let response = fixture
        .harness
        .send_raw(
            Method::OPTIONS,
            &proxy_path(fixture.alias, "/echo"),
            &headers,
            None,
        )
        .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        response.headers()["access-control-allow-origin"],
        "https://app.example.com"
    );
    assert_eq!(response.headers()["access-control-allow-methods"], "POST");
    assert_eq!(
        response.headers()["access-control-allow-headers"],
        "content-type"
    );
    assert_eq!(response.headers()["access-control-max-age"], "86400");
    assert!(
        response.headers()["vary"]
            .to_str()
            .unwrap()
            .contains("Origin")
    );

    // The echo mock served nothing: preflight is answered by the gateway.
    let probe = fixture
        .harness
        .send(Method::GET, &proxy_path(fixture.alias, "/echo"), None)
        .await;
    assert_eq!(probe.status(), StatusCode::OK);
}

/// Actual cross-origin calls are validated against the configured origins.
#[tokio::test]
async fn disallowed_origins_are_forbidden() {
    let harness = Harness::with_config(permissive_config(), None);
    let upstream = MockUpstream::start();
    let body = json!({
        "alias": "web",
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "server": { "endpoints": [ upstream.endpoint() ] },
        "cors": {
            "enabled": true,
            "allowed_origins": ["https://app.example.com"],
            "allowed_methods": ["GET", "POST"],
            "expose_headers": ["x-request-id"]
        }
    });
    let created = create_upstream(&harness, body).await;
    create_route(
        &harness,
        route_body(created["id"].as_str().unwrap(), "/echo"),
    )
    .await;

    let headers = [("origin", "https://evil.com")];
    let response = harness
        .send_raw(Method::GET, &proxy_path("web", "/echo"), &headers, None)
        .await;
    let problem = harness
        .expect_problem(response, StatusCode::FORBIDDEN)
        .await;
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1"
    );

    let headers = [("origin", "https://app.example.com")];
    let response = harness
        .send_raw(Method::GET, &proxy_path("web", "/echo"), &headers, None)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["access-control-allow-origin"],
        "https://app.example.com"
    );
    assert_eq!(
        response.headers()["access-control-expose-headers"],
        "x-request-id"
    );
    assert_eq!(response.headers()["vary"], "Origin");
}

/// Methods outside the CORS allowlist are forbidden even for allowed origins.
#[tokio::test]
async fn disallowed_cors_methods_are_forbidden() {
    let harness = Harness::with_config(permissive_config(), None);
    let upstream = MockUpstream::start();
    let body = json!({
        "alias": "web",
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "server": { "endpoints": [ upstream.endpoint() ] },
        "cors": {
            "enabled": true,
            "allowed_origins": ["*"],
            "allowed_methods": ["GET"]
        }
    });
    let created = create_upstream(&harness, body).await;
    create_route(
        &harness,
        route_body_with(created["id"].as_str().unwrap(), &["GET", "DELETE"], "/echo"),
    )
    .await;

    let headers = [("origin", "https://app.example.com")];
    let response = harness
        .send_raw(Method::DELETE, &proxy_path("web", "/echo"), &headers, None)
        .await;
    let problem = harness
        .expect_problem(response, StatusCode::FORBIDDEN)
        .await;
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1"
    );
}

/// The built-in required-headers guard runs in the request phase.
#[tokio::test]
async fn guard_plugins_enforce_required_headers() {
    let harness = Harness::with_config(permissive_config(), None);
    let upstream = MockUpstream::start();
    let body = json!({
        "alias": "guarded",
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "server": { "endpoints": [ upstream.endpoint() ] },
        "headers": { "request": { "passthrough": "all" } },
        "plugins": {
            "items": [
                {
                    "plugin_ref": "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
                    "config": { "required_request_headers": "x-api-key" }
                }
            ]
        }
    });
    let created = create_upstream(&harness, body).await;
    create_route(
        &harness,
        route_body(created["id"].as_str().unwrap(), "/echo"),
    )
    .await;

    let response = harness
        .send(Method::GET, &proxy_path("guarded", "/echo"), None)
        .await;
    let problem = harness
        .expect_problem(response, StatusCode::BAD_REQUEST)
        .await;
    assert_eq!(problem["error_code"], "REQUIRED_HEADER_MISSING");
    assert_eq!(problem["path"], "/oagw/v1/proxy/guarded/echo");

    let headers = [("x-api-key", "secret")];
    let response = harness
        .send_raw(Method::GET, &proxy_path("guarded", "/echo"), &headers, None)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
}

/// The request-id transform stamps every call it forwards.
#[tokio::test]
async fn transform_plugins_stamp_the_request_id() {
    let harness = Harness::with_config(permissive_config(), None);
    let upstream = MockUpstream::start();
    let body = json!({
        "alias": "traced",
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "server": { "endpoints": [ upstream.endpoint() ] },
        "headers": { "request": { "passthrough": "all" } },
        "plugins": {
            "items": [
                {
                    "plugin_ref": "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1"
                }
            ]
        }
    });
    let created = create_upstream(&harness, body).await;
    create_route(
        &harness,
        route_body(created["id"].as_str().unwrap(), "/echo"),
    )
    .await;

    let response = harness
        .send(Method::GET, &proxy_path("traced", "/echo"), None)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = harness.json(response).await;
    let stamped = body["headers"]["x-request-id"].as_str().unwrap().to_owned();
    assert!(!stamped.is_empty());

    // An inbound request id is propagated rather than replaced.
    let headers = [("x-request-id", "client-1")];
    let response = harness
        .send_raw(Method::GET, &proxy_path("traced", "/echo"), &headers, None)
        .await;
    let body = harness.json(response).await;
    assert_eq!(body["headers"]["x-request-id"], "client-1");
}

/// Header rules rewrite what the upstream sees.
#[tokio::test]
async fn request_header_rules_are_applied() {
    let harness = Harness::with_config(permissive_config(), None);
    let upstream = MockUpstream::start();
    let body = json!({
        "alias": "rewritten",
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "server": { "endpoints": [ upstream.endpoint() ] },
        "headers": {
            "request": {
                "set": { "x-added": "by-gateway" },
                "remove": ["x-dropped"],
                "passthrough": "all"
            }
        }
    });
    let created = create_upstream(&harness, body).await;
    create_route(
        &harness,
        route_body(created["id"].as_str().unwrap(), "/echo"),
    )
    .await;

    let headers = [("x-dropped", "yes"), ("x-kept", "yes")];
    let response = harness
        .send_raw(
            Method::GET,
            &proxy_path("rewritten", "/echo"),
            &headers,
            None,
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = harness.json(response).await;
    assert_eq!(body["headers"]["x-added"], "by-gateway");
    assert_eq!(body["headers"]["x-kept"], "yes");
    assert!(body["headers"].get("x-dropped").is_none());
}

/// The OAGW routing header never reaches the upstream.
#[tokio::test]
async fn gateway_routing_headers_are_stripped() {
    let fixture = ProxyFixture::new().await;
    let headers = [("x-oagw-target-host", "127.0.0.1")];
    let response = fixture
        .harness
        .send_raw(
            Method::GET,
            &proxy_path(fixture.alias, "/echo"),
            &headers,
            None,
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = fixture.harness.json(response).await;
    assert!(body["headers"].get("x-oagw-target-host").is_none());
}

/// Response header rules rewrite what the client sees.
#[tokio::test]
async fn response_header_rules_are_applied() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/echo");
        then.status(200)
            .header("x-upstream-only", "yes")
            .header("content-type", "application/json")
            .body("{}");
    });
    let harness = Harness::with_config(permissive_config(), None);
    let port = server.address().port();
    let body = json!({
        "alias": "shaped",
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": port } ] },
        "headers": { "response": { "remove": ["x-upstream-only"] } }
    });
    let created = create_upstream(&harness, body).await;
    create_route(
        &harness,
        route_body(created["id"].as_str().unwrap(), "/echo"),
    )
    .await;

    let response = harness
        .send(Method::GET, &proxy_path("shaped", "/echo"), None)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().get("x-upstream-only").is_none());
}

/// Unreachable upstreams surface as a gateway problem document.
#[tokio::test]
async fn unreachable_upstreams_are_gateway_errors() {
    let harness = Harness::with_config(permissive_config(), None);
    // Nothing listens on the reserved documentation port.
    let created = create_upstream(
        &harness,
        json!({
            "alias": "dark",
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": 1 } ] }
        }),
    )
    .await;
    create_route(
        &harness,
        route_body(created["id"].as_str().unwrap(), "/echo"),
    )
    .await;

    let response = harness
        .send(Method::GET, &proxy_path("dark", "/echo"), None)
        .await;
    let problem = harness
        .expect_problem(response, StatusCode::SERVICE_UNAVAILABLE)
        .await;
    assert!(
        problem["type"]
            .as_str()
            .unwrap()
            .ends_with("link.unavailable.v1")
    );
    assert_eq!(problem["host"], "gateway");
    assert_eq!(problem["path"], "/oagw/v1/proxy/dark/echo");
}

/// Bodies beyond the configured ceiling are rejected before the hop.
#[tokio::test]
async fn oversized_bodies_are_rejected() {
    let harness = Harness::with_config(
        oagw::config::OagwConfig {
            max_request_body_bytes: 8,
            ..permissive_config()
        },
        None,
    );
    let upstream = MockUpstream::start();
    let created = create_upstream(&harness, upstream.upstream_body("small")).await;
    create_route(
        &harness,
        route_body_with(created["id"].as_str().unwrap(), &["GET", "POST"], "/echo"),
    )
    .await;

    let response = harness
        .send(
            Method::POST,
            &proxy_path("small", "/echo"),
            Some("{\"payload\":\"".to_owned() + &"x".repeat(64) + "\"}"),
        )
        .await;
    harness
        .expect_problem(response, StatusCode::PAYLOAD_TOO_LARGE)
        .await;
}

/// A disabled upstream is not served.
#[tokio::test]
async fn disabled_upstreams_are_unavailable() {
    let harness = Harness::with_config(permissive_config(), None);
    let upstream = MockUpstream::start();
    let body = json!({
        "alias": "off",
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "server": { "endpoints": [ upstream.endpoint() ] },
        "enabled": false
    });
    let created = create_upstream(&harness, body).await;
    assert_eq!(created["enabled"], false);
    create_route(
        &harness,
        route_body(created["id"].as_str().unwrap(), "/echo"),
    )
    .await;

    let response = harness
        .send(Method::GET, &proxy_path("off", "/echo"), None)
        .await;
    let problem = harness
        .expect_problem(response, StatusCode::SERVICE_UNAVAILABLE)
        .await;
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1"
    );
}

/// The API-key auth plugin injects the credential it resolves.
#[tokio::test]
async fn apikey_auth_injects_the_resolved_key() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET)
            .path("/echo")
            .header("x-api-key", "resolved-key");
        then.status(200).body("{}");
    });
    let port = server.address().port();
    let harness = Harness::with_config(
        permissive_config(),
        Some(std::sync::Arc::new(FakeCredStore::new(
            "vendor-key",
            "resolved-key",
        ))),
    );
    let body = json!({
        "alias": "keyed",
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": port } ] },
        "auth": {
            "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
            "config": { "secret_ref": "vendor-key", "header_name": "x-api-key" }
        }
    });
    let created = create_upstream(&harness, body).await;
    create_route(
        &harness,
        route_body(created["id"].as_str().unwrap(), "/echo"),
    )
    .await;

    let response = harness
        .send(Method::GET, &proxy_path("keyed", "/echo"), None)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(mock.calls(), 1);
}

/// The upstream response body is relayed untouched.
#[tokio::test]
async fn upstream_bodies_are_relayed_untouched() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/echo");
        then.status(201)
            .header("x-from-upstream", "1")
            .body("{\"value\":42}");
    });
    let harness = Harness::with_config(permissive_config(), None);
    let port = server.address().port();
    let created = create_upstream(
        &harness,
        json!({
            "alias": "raw",
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": port } ] }
        }),
    )
    .await;
    create_route(
        &harness,
        route_body(created["id"].as_str().unwrap(), "/echo"),
    )
    .await;

    let response = harness
        .send(Method::GET, &proxy_path("raw", "/echo"), None)
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(response.headers()["x-from-upstream"], "1");
    let body = harness.json(response).await;
    assert_eq!(body, serde_json::json!({ "value": 42 }));
}

/// The upstream status code is preserved, including 4xx and 5xx.
#[tokio::test]
async fn upstream_status_codes_are_preserved() {
    for (status, expected) in [
        (StatusCode::BAD_REQUEST, StatusCode::BAD_REQUEST),
        (StatusCode::NOT_FOUND, StatusCode::NOT_FOUND),
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    ] {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/echo");
            then.status(status.as_u16()).body("{}");
        });
        let harness = Harness::with_config(permissive_config(), None);
        let port = server.address().port();
        let created = create_upstream(
            &harness,
            json!({
                "alias": "codes",
                "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
                "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": port } ] }
            }),
        )
        .await;
        create_route(
            &harness,
            route_body(created["id"].as_str().unwrap(), "/echo"),
        )
        .await;

        let response = harness
            .send(Method::GET, &proxy_path("codes", "/echo"), None)
            .await;
        assert_eq!(response.status(), expected);
        assert_eq!(response.headers()["x-oagw-error-source"], "upstream");
    }
}

/// Hop-by-hop headers never reach the upstream.
#[tokio::test]
async fn hop_by_hop_headers_are_stripped() {
    let fixture = ProxyFixture::new().await;
    let headers = [
        ("connection", "keep-alive"),
        ("te", "trailers"),
        ("keep-alive", "timeout=5"),
        ("proxy-authorization", "Basic zzz"),
    ];
    let response = fixture
        .harness
        .send_raw(
            Method::GET,
            &proxy_path(fixture.alias, "/echo"),
            &headers,
            None,
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = fixture.harness.json(response).await;
    for name in ["connection", "te", "keep-alive", "proxy-authorization"] {
        assert!(
            body["headers"].get(name).is_none(),
            "{name} must be stripped, saw: {body}"
        );
    }
}

/// `path_suffix_mode` decides whether a suffix is forwarded or rejected.
#[tokio::test]
async fn path_suffix_rules_are_enforced() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/v1/models/extra");
        then.status(200).body("{}");
    });
    let harness = Harness::with_config(permissive_config(), None);
    let created = create_upstream(
        &harness,
        json!({
            "alias": "shaped",
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1",
                "port": server.address().port() } ] }
        }),
    )
    .await;
    let id = created["id"].as_str().unwrap().to_owned();
    let mut exact = route_body_with(id.as_str(), &["GET"], "/exact");
    exact["match"]["http"]["path_suffix_mode"] = json!("disabled");
    create_route(&harness, exact).await;
    create_route(&harness, route_body_with(id.as_str(), &["GET"], "/v1")).await;

    let rejected = harness
        .send(Method::GET, &proxy_path("shaped", "/exact/extra"), None)
        .await;
    harness
        .expect_problem(rejected, StatusCode::BAD_REQUEST)
        .await;

    let forwarded = harness
        .send(Method::GET, &proxy_path("shaped", "/v1/models/extra"), None)
        .await;
    assert_eq!(forwarded.status(), StatusCode::OK, "{forwarded:?}");
}

/// A failing upstream is attempted exactly once: no silent retries.
#[tokio::test]
async fn a_failing_upstream_is_called_once() {
    let (addr, counter) = accept_and_close();
    let harness = Harness::with_config(permissive_config(), None);
    let created = create_upstream(
        &harness,
        json!({
            "alias": "flaky",
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": addr.port() } ] }
        }),
    )
    .await;
    create_route(
        &harness,
        route_body(created["id"].as_str().unwrap(), "/echo"),
    )
    .await;

    let response = harness
        .send(Method::GET, &proxy_path("flaky", "/echo"), None)
        .await;
    assert!(
        response.status().is_server_error() || response.status().is_client_error(),
        "the failure surfaces as a problem document: {response:?}"
    );
    assert_eq!(
        counter.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the upstream saw exactly one attempt"
    );
}

/// An upstream that accepts the connection and never answers is a 504.
#[tokio::test]
async fn upstream_timeouts_are_gateway_errors() {
    let harness = Harness::with_config(
        oagw::config::OagwConfig {
            proxy_timeout_secs: 1,
            ..permissive_config()
        },
        None,
    );
    let addr = never_answer();
    let created = create_upstream(
        &harness,
        json!({
            "alias": "slow",
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1",
                "port": addr.port() } ] }
        }),
    )
    .await;
    create_route(
        &harness,
        route_body(created["id"].as_str().unwrap(), "/echo"),
    )
    .await;

    let response = harness
        .send(Method::GET, &proxy_path("slow", "/echo"), None)
        .await;
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(response.headers()["x-oagw-error-source"], "gateway");
    let problem = harness.json(response).await;
    assert!(
        problem["type"]
            .as_str()
            .unwrap()
            .ends_with("timeout.request.v1")
    );
}

/// A listener that accepts a connection and closes it immediately, counting
/// every accept.
fn accept_and_close() -> (
    std::net::SocketAddr,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    use std::io::Write;
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    let listener = TcpListener::bind("127.0.0.1:0").expect("listener binds");
    let addr = listener.local_addr().expect("address resolves");
    let counter = Arc::new(AtomicUsize::new(0));
    let tally = Arc::clone(&counter);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            tally.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            // Answer nothing: drop the socket, which hyper reads as a failure.
            let mut stream = stream;
            let _ignored = stream.write_all(&[]);
        }
    });
    (addr, counter)
}

/// A listener that accepts connections and answers nothing, forever.
fn never_answer() -> std::net::SocketAddr {
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").expect("listener binds");
    let addr = listener.local_addr().expect("address resolves");
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            // Hold the socket open without writing a byte: the gateway must
            // give up on its own.
            std::thread::sleep(std::time::Duration::from_secs(30));
            drop(stream);
        }
    });
    addr
}

/// An auth plugin whose secret cannot be resolved rejects the request.
#[tokio::test]
async fn an_unresolvable_secret_is_unauthorized() {
    let harness = Harness::with_config(permissive_config(), None);
    let upstream = MockUpstream::start();
    let body = json!({
        "alias": "keyed",
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "server": { "endpoints": [ upstream.endpoint() ] },
        "auth": {
            "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
            "config": { "secret_ref": "absent", "header_name": "x-api-key" }
        }
    });
    let created = create_upstream(&harness, body).await;
    create_route(
        &harness,
        route_body(created["id"].as_str().unwrap(), "/echo"),
    )
    .await;

    let response = harness
        .send(Method::GET, &proxy_path("keyed", "/echo"), None)
        .await;
    assert_eq!(response.headers()["x-oagw-error-source"], "gateway");
    let problem = harness
        .expect_problem(response, StatusCode::UNAUTHORIZED)
        .await;
    assert!(
        problem["type"]
            .as_str()
            .unwrap()
            .ends_with("auth.failed.v1"),
        "{problem}"
    );
}
