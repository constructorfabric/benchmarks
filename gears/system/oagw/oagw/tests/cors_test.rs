//! CORS (T045, ADR 0004): a permissive preflight answered without resolving
//! the upstream, policy enforcement on the actual request, the exact /
//! port- / protocol-sensitive origin match, and the CORS headers an allowed
//! actual response carries.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use axum::http::StatusCode;
use common::*;
use httpmock::prelude::*;
use serde_json::json;

const ALLOWED: &str = "https://app.example.com";

/// The `GET /v1/models` mock a proxied request is expected to reach.
fn models_mock(stub: &MockServer) -> httpmock::Mock {
    stub.mock(|when, then| {
        when.method(GET).path("/v1/models");
        then.status(200).header("content-type", "application/json").body("ok");
    })
}

/// A gear whose upstream dials `stub` and carries the given CORS policy.
async fn gear_with_policy(stub: &MockServer, policy: serde_json::Value) -> Harness {
    models_mock(stub);
    let harness = Harness::plaintext_gear();
    let mut body = upstream_body("web", "127.0.0.1", stub.port(), "http");
    body["cors"] = policy;
    let response = harness
        .send(harness.request("POST", "/oagw/v1/upstreams", Some(body)))
        .await;
    assert_eq!(response.status(), StatusCode::CREATED, "upstream created");
    let upstream_id = read_json(response).await["id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    create_route(&harness, &upstream_id, "/v1/models", &["GET", "DELETE"]).await;
    harness
}

/// The mock to watch for hits in a test that has not captured one itself.
fn watched(stub: &MockServer) -> httpmock::Mock {
    models_mock(stub)
}

fn policy() -> serde_json::Value {
    json!({
        "enabled": true,
        "allowed_origins": [ALLOWED],
        "allowed_methods": ["GET", "POST"],
        "expose_headers": ["X-Request-ID"],
        "allow_credentials": true
    })
}

async fn preflight(harness: &Harness, origin: &str, method: &str) -> axum::http::Response<axum::body::Body> {
    harness
        .send(harness.proxy_request(
            "OPTIONS",
            "/oagw/v1/proxy/web/v1/models",
            &[
                ("origin", origin),
                ("access-control-request-method", method),
                ("access-control-request-headers", "Content-Type, Authorization"),
            ],
            None,
        ))
        .await
}

async fn actual(harness: &Harness, origin: Option<&str>, method: &str) -> axum::http::Response<axum::body::Body> {
    let mut headers: Vec<(&str, &str)> = Vec::new();
    if let Some(origin) = origin {
        headers.push(("origin", origin));
    }
    harness
        .send(harness.proxy_request(method, "/oagw/v1/proxy/web/v1/models", &headers, None))
        .await
}

fn header(response: &axum::http::Response<axum::body::Body>, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

#[tokio::test]
async fn a_preflight_is_answered_permissively_from_the_request_alone() {
    let stub = MockServer::start();
    stub.mock(|when, then| {
        when.method(GET).path("/v1/models");
        then.status(200).body("ok");
    });
    let harness = gear_with_policy(&stub, policy()).await;

    let response = preflight(&harness, ALLOWED, "POST").await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let headers = response.headers().clone();
    assert_eq!(headers.get("access-control-allow-origin").and_then(|v| v.to_str().ok()), Some(ALLOWED));
    assert_eq!(headers.get("access-control-allow-methods").and_then(|v| v.to_str().ok()), Some("POST"));
    assert_eq!(
        headers.get("access-control-allow-headers").and_then(|v| v.to_str().ok()),
        Some("Content-Type, Authorization")
    );
    assert_eq!(headers.get("access-control-max-age").and_then(|v| v.to_str().ok()), Some("86400"));
    assert_eq!(
        headers.get("vary").and_then(|v| v.to_str().ok()),
        Some("Origin, Access-Control-Request-Method, Access-Control-Request-Headers")
    );
    assert_eq!(watched(&stub).calls(), 0, "nothing was dialled to answer a preflight");
}

#[tokio::test]
async fn a_preflight_is_permissive_even_for_an_origin_the_policy_refuses() {
    let stub = MockServer::start();
    stub.mock(|when, then| {
        when.method(GET).path("/v1/models");
        then.status(200).body("ok");
    });
    let harness = gear_with_policy(&stub, policy()).await;

    // ADR 0004: enforcement is deferred to the actual request, so the
    // preflight is never the place a browser is told "no".
    let response = preflight(&harness, "https://evil.com", "DELETE").await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("https://evil.com")
    );
}

#[tokio::test]
async fn a_preflight_is_answered_without_an_upstream_to_resolve() {
    // No upstream is registered under the alias at all: the preflight is
    // still a 204, because answering it needs no upstream and no tenant.
    let harness = Harness::plaintext_gear();
    let response = preflight(&harness, ALLOWED, "GET").await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some(ALLOWED)
    );
    assert_eq!(response.headers().get("access-control-max-age").and_then(|v| v.to_str().ok()), Some("86400"));
}

#[tokio::test]
async fn a_preflight_is_answered_without_a_tenant_context() {
    // A browser sends no credentials on a preflight (WHATWG Fetch), so the
    // request arrives with no tenant to resolve (ADR 0004 §"Preflight Request
    // Handling"). Answering it permissively must not depend on one: resolving
    // a tenant chain first turns every credential-less preflight into a 403.
    let harness = Harness::plaintext_gear();
    let response = harness
        .send(harness.anonymous_proxy_request(
            "OPTIONS",
            "/oagw/v1/proxy/api.example.com/v1/models",
            &[
                ("origin", ALLOWED),
                ("access-control-request-method", "GET"),
            ],
        ))
        .await;
    assert_eq!(
        response.status(),
        StatusCode::NO_CONTENT,
        "a preflight is answered even when no tenant can be resolved"
    );
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some(ALLOWED)
    );
}

#[tokio::test]
async fn an_allowed_actual_request_carries_the_cors_headers() {
    let stub = MockServer::start();
    stub.mock(|when, then| {
        when.method(GET).path("/v1/models");
        then.status(200)
            .header("content-type", "application/json")
            .body("ok");
    });
    let harness = gear_with_policy(&stub, policy()).await;

    let response = actual(&harness, Some(ALLOWED), "GET").await;
    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers().clone();
    assert_eq!(
        headers.get("access-control-allow-origin").and_then(|v| v.to_str().ok()),
        Some(ALLOWED),
        "the request's origin is echoed"
    );
    assert_eq!(
        headers.get("access-control-expose-headers").and_then(|v| v.to_str().ok()),
        Some("X-Request-ID")
    );
    assert_eq!(
        headers.get("access-control-allow-credentials").and_then(|v| v.to_str().ok()),
        Some("true")
    );
    assert_eq!(headers.get("vary").and_then(|v| v.to_str().ok()), Some("Origin"));
}

#[tokio::test]
async fn a_disallowed_origin_is_refused_before_the_upstream_is_dialled() {
    let stub = MockServer::start();
    stub.mock(|when, then| {
        when.method(GET).path("/v1/models");
        then.status(200).body("ok");
    });
    let harness = gear_with_policy(&stub, policy()).await;

    let response = actual(&harness, Some("https://evil.com"), "GET").await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(watched(&stub).calls(), 0, "a refused request never reaches the upstream");

    let problem = read_json(response).await;
    assert_eq!(problem["type"], "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1");
    assert_eq!(problem["status"], 403);
    assert_eq!(problem["title"], "CORS Origin Not Allowed");
    assert_eq!(problem["invalid_value"], "https://evil.com");
    assert_eq!(problem["instance"], "/oagw/v1/proxy/web");
}

#[tokio::test]
async fn a_disallowed_method_is_refused_with_its_own_problem() {
    let stub = MockServer::start();
    stub.mock(|when, then| {
        when.method(GET).path("/v1/models");
        then.status(200).body("ok");
    });
    let harness = gear_with_policy(&stub, policy()).await;

    let response = actual(&harness, Some(ALLOWED), "DELETE").await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(watched(&stub).calls(), 0);

    let problem = read_json(response).await;
    assert_eq!(problem["type"], "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1");
    assert_eq!(problem["status"], 403);
    assert_eq!(problem["title"], "CORS Method Not Allowed");
    assert_eq!(problem["invalid_value"], "DELETE");
}

#[tokio::test]
async fn origin_matching_is_port_and_protocol_sensitive() {
    for refused in ["https://app.example.com:8443", "http://app.example.com", "https://other.example.com"] {
        let stub = MockServer::start();
        stub.mock(|when, then| {
            when.method(GET).path("/v1/models");
            then.status(200).body("ok");
        });
        let harness = gear_with_policy(&stub, policy()).await;
        let response = actual(&harness, Some(refused), "GET").await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "`{refused}` is a different origin");
    }
}

#[tokio::test]
async fn the_wildcard_admits_every_origin() {
    let stub = MockServer::start();
    stub.mock(|when, then| {
        when.method(GET).path("/v1/models");
        then.status(200).body("ok");
    });
    let harness = gear_with_policy(
        &stub,
        json!({ "enabled": true, "allowed_origins": ["*"], "allowed_methods": ["GET"] }),
    )
    .await;

    for origin in ["https://one.example", "http://other.example:8080"] {
        let response = actual(&harness, Some(origin), "GET").await;
        assert_eq!(response.status(), StatusCode::OK, "`{origin}` matches `*`");
        assert_eq!(
            response
                .headers()
                .get("access-control-allow-origin")
                .and_then(|v| v.to_str().ok()),
            Some(origin)
        );
    }
}

#[tokio::test]
async fn an_upstream_without_a_policy_denies_cross_origin_requests() {
    // "Deny by default": CORS is off unless explicitly enabled, so a browser
    // request to an upstream that configured no policy is refused.
    let stub = MockServer::start();
    stub.mock(|when, then| {
        when.method(GET).path("/v1/models");
        then.status(200).body("ok");
    });
    let harness = gear_with_policy(&stub, json!({ "enabled": false, "allowed_origins": ["*"] })).await;
    let response = actual(&harness, Some(ALLOWED), "GET").await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(watched(&stub).calls(), 0);
}

#[tokio::test]
async fn a_request_without_an_origin_is_never_subject_to_cors() {
    // CORS governs browsers only; a same-origin or non-browser request to an
    // upstream that configured no policy is proxied as usual.
    let stub = MockServer::start();
    stub.mock(|when, then| {
        when.method(GET).path("/v1/models");
        then.status(200).body("ok");
    });
    let harness = gear_with_policy(&stub, json!({ "enabled": false, "allowed_origins": ["*"] })).await;
    let response = actual(&harness, None, "GET").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("access-control-allow-origin").and_then(|v| v.to_str().ok()),
        None,
        "no CORS headers on a same-origin request"
    );
}

#[tokio::test]
async fn credentials_cannot_be_combined_with_the_wildcard() {
    let stub = MockServer::start();
    stub.mock(|when, then| {
        when.method(GET).path("/v1/models");
        then.status(200).body("ok");
    });
    let harness = Harness::plaintext_gear();
    let mut body = upstream_body("wild", "127.0.0.1", stub.port(), "http");
    body["cors"] = json!({
        "enabled": true,
        "allowed_origins": ["*"],
        "allowed_methods": ["GET"],
        "allow_credentials": true
    });
    let response = harness
        .send(harness.request("POST", "/oagw/v1/upstreams", Some(body)))
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let problem = read_json(response).await;
    assert_eq!(problem["status"], 400);
    let detail = problem["detail"].as_str().unwrap_or_default();
    assert!(detail.contains("allow_credentials"), "the reason is named: `{detail}`");
}
