// Created: 2026-08-29 by Constructor Tech
//! CORS: preflight fast path, origin/method enforcement and config-time rules.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{json_body, post, tenant};
use httpmock::MockServer;
use serde_json::{Value, json};

const PROTOCOL: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

async fn register_upstream(harness: &common::Harness, server: &MockServer, cors: Value) -> String {
    let payload = json!({
        "alias": "cors-up.example.com",
        "protocol": PROTOCOL,
        "server": { "endpoints": [ { "scheme": "http", "host": server.host(), "port": server.port() } ] },
        "cors": cors,
    });
    let created =
        json_body(post(harness.router(), "/oagw/v1/upstreams", payload, tenant()).await).await;
    let id = created["id"].as_str().unwrap().to_owned();
    post(
        harness.router(),
        "/oagw/v1/routes",
        json!({ "upstream_id": id, "match": { "http": { "methods": ["GET", "POST"], "path": "/" } } }),
        tenant(),
    )
    .await;
    id
}

fn request(
    alias: &str,
    method: &str,
    origin: Option<&str>,
    extra: &[(&str, &str)],
) -> axum::http::Request<axum::body::Body> {
    let mut builder = axum::http::Request::builder()
        .method(method)
        .uri(format!("/oagw/v1/proxy/{alias}/api"));
    if let Some(origin) = origin {
        builder = builder.header("origin", origin);
    }
    for (name, value) in extra {
        builder = builder.header(*name, *value);
    }
    builder.body(axum::body::Body::empty()).unwrap()
}

#[tokio::test]
async fn preflight_is_answered_with_204_without_reaching_the_upstream() {
    let server = MockServer::start();
    // Any request the upstream sees means the preflight was not short-circuited.
    let target = server.mock(|when, then| {
        when.method(httpmock::Method::GET);
        then.status(200).body("ok");
    });

    let harness = common::Harness::new(common::test_config(), None);
    register_upstream(
        &harness,
        &server,
        json!({ "enabled": true, "allowed_origins": ["https://app.example.com"] }),
    )
    .await;

    let response = harness
        .send_request(
            request(
                "cors-up.example.com",
                "OPTIONS",
                Some("https://app.example.com"),
                &[("access-control-request-method", "GET")],
            ),
            tenant(),
        )
        .await;
    assert_eq!(response.status(), 204);
    assert_eq!(
        common::header(&response, "access-control-allow-origin").as_deref(),
        Some("https://app.example.com")
    );
    assert_eq!(
        common::header(&response, "access-control-allow-methods").as_deref(),
        Some("GET")
    );
    assert_eq!(
        common::header(&response, "access-control-max-age").as_deref(),
        Some("86400")
    );
    assert_eq!(
        common::header(&response, "vary").as_deref(),
        Some("Origin, Access-Control-Request-Method, Access-Control-Request-Headers")
    );
    assert_eq!(
        common::header(&response, "x-oagw-error-source").as_deref(),
        Some("gateway")
    );
    assert_eq!(target.calls(), 0, "preflight must not reach the upstream");
}

#[tokio::test]
async fn preflight_does_not_need_options_in_the_route_allowlist() {
    let server = MockServer::start();
    let target = server.mock(|when, then| {
        when.method(httpmock::Method::GET);
        then.status(200).body("ok");
    });

    let harness = common::Harness::new(common::test_config(), None);
    let created = json_body(
        post(
            harness.router(),
            "/oagw/v1/upstreams",
            json!({
                "alias": "no-options.example.com",
                "protocol": PROTOCOL,
                "server": { "endpoints": [ { "scheme": "http", "host": server.host(), "port": server.port() } ] },
                "cors": { "enabled": true, "allowed_origins": ["https://app.example.com"] },
            }),
            tenant(),
        )
        .await,
    )
    .await;
    let id = created["id"].as_str().unwrap().to_owned();
    // `OPTIONS` is deliberately absent from the allowlist.
    post(
        harness.router(),
        "/oagw/v1/routes",
        json!({ "upstream_id": id, "match": { "http": { "methods": ["GET"], "path": "/" } } }),
        tenant(),
    )
    .await;

    let response = harness
        .send_request(
            request(
                "no-options.example.com",
                "OPTIONS",
                Some("https://app.example.com"),
                &[("access-control-request-method", "GET")],
            ),
            tenant(),
        )
        .await;
    assert_eq!(response.status(), 204);
    assert_eq!(
        common::header(&response, "access-control-allow-origin").as_deref(),
        Some("https://app.example.com")
    );
    assert_eq!(target.calls(), 0);
}

#[tokio::test]
async fn a_preflight_for_an_unresolvable_alias_is_still_answered_permissively() {
    let harness = common::Harness::new(common::test_config(), None);
    let response = harness
        .send_request(
            request(
                "ghost.example.com",
                "OPTIONS",
                Some("https://app.example.com"),
                &[("access-control-request-method", "GET")],
            ),
            tenant(),
        )
        .await;
    // ADR-0004: a browser preflight carries no credentials, so the gateway has no
    // tenant context to resolve an alias with. It is answered permissively before
    // any resolution; the 404 surfaces on the actual request that follows it.
    assert_eq!(response.status(), 204);
    assert_eq!(
        common::header(&response, "access-control-allow-origin").as_deref(),
        Some("https://app.example.com")
    );
    assert_eq!(
        common::header(&response, "x-oagw-error-source").as_deref(),
        Some("gateway")
    );
}

#[tokio::test]
async fn a_disallowed_origin_is_rejected_before_forwarding() {
    let server = MockServer::start();
    let target = server.mock(|when, then| {
        when.method(httpmock::Method::GET);
        then.status(200).body("ok");
    });

    let harness = common::Harness::new(common::test_config(), None);
    register_upstream(
        &harness,
        &server,
        json!({ "enabled": true, "allowed_origins": ["https://app.example.com"] }),
    )
    .await;

    let response = harness
        .send_request(
            request(
                "cors-up.example.com",
                "GET",
                Some("https://evil.example.com"),
                &[],
            ),
            tenant(),
        )
        .await;
    assert_eq!(response.status(), 403);
    let body = json_body(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1"
    );
    assert_eq!(body["status"], 403);
    assert_eq!(body["title"], "CORS Origin Not Allowed");
    assert_eq!(target.calls(), 0, "a rejected origin must not be forwarded");
}

#[tokio::test]
async fn a_disallowed_method_is_enforced_on_the_actual_request() {
    let server = MockServer::start();
    let target = server.mock(|when, then| {
        when.method(httpmock::Method::GET);
        then.status(200).body("ok");
    });
    let post_target = server.mock(|when, then| {
        when.method(httpmock::Method::POST);
        then.status(200).body("posted");
    });

    let harness = common::Harness::new(common::test_config(), None);
    // `register_upstream` allows GET and POST on the route, so POST survives route
    // matching and is the method CORS itself has to reject.
    register_upstream(
        &harness,
        &server,
        json!({
            "enabled": true,
            "allowed_origins": ["https://app.example.com"],
            "allowed_methods": ["GET"],
        }),
    )
    .await;

    // ADR-0004: the preflight itself is permissive — method enforcement is
    // deferred to the actual request, where the origin is validated too.
    let preflight = harness
        .send_request(
            request(
                "cors-up.example.com",
                "OPTIONS",
                Some("https://app.example.com"),
                &[("access-control-request-method", "POST")],
            ),
            tenant(),
        )
        .await;
    assert_eq!(preflight.status(), 204);

    let response = harness
        .send_request(
            request(
                "cors-up.example.com",
                "POST",
                Some("https://app.example.com"),
                &[],
            ),
            tenant(),
        )
        .await;
    assert_eq!(response.status(), 403);
    let body = json_body(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1"
    );
    assert_eq!(body["status"], 403);
    assert_eq!(body["title"], "CORS Method Not Allowed");
    assert_eq!(
        post_target.calls(),
        0,
        "a rejected method must not be forwarded"
    );
    assert_eq!(target.calls(), 0);
}

#[tokio::test]
async fn actual_responses_carry_the_cors_headers() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::GET);
        then.status(200).body("ok");
    });

    let harness = common::Harness::new(common::test_config(), None);
    register_upstream(
        &harness,
        &server,
        json!({
            "enabled": true,
            "allowed_origins": ["https://app.example.com"],
            "allowed_methods": ["GET", "POST"],
            "expose_headers": ["x-request-id"],
            "allow_credentials": true,
        }),
    )
    .await;

    let response = harness
        .send_request(
            request(
                "cors-up.example.com",
                "GET",
                Some("https://app.example.com"),
                &[],
            ),
            tenant(),
        )
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        common::header(&response, "access-control-allow-origin").as_deref(),
        Some("https://app.example.com")
    );
    assert_eq!(
        common::header(&response, "access-control-allow-credentials").as_deref(),
        Some("true")
    );
    assert_eq!(
        common::header(&response, "access-control-expose-headers").as_deref(),
        Some("x-request-id")
    );
    assert_eq!(common::header(&response, "vary").as_deref(), Some("Origin"));
}

#[tokio::test]
async fn credentials_with_a_wildcard_origin_are_rejected_at_configuration_time() {
    let harness = common::Harness::new(common::test_config(), None);
    let response = post(
        harness.router(),
        "/oagw/v1/upstreams",
        json!({
            "alias": "wild.example.com",
            "protocol": PROTOCOL,
            "server": { "endpoints": [ { "scheme": "https", "host": "wild.example.com" } ] },
            "cors": { "enabled": true, "allowed_origins": ["*"], "allow_credentials": true },
        }),
        tenant(),
    )
    .await;
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
            .contains("allow_credentials"),
        "the problem document must name the rule: {}",
        body["detail"]
    );
}

#[tokio::test]
async fn an_unknown_cors_method_is_rejected_at_configuration_time() {
    let harness = common::Harness::new(common::test_config(), None);
    let response = post(
        harness.router(),
        "/oagw/v1/upstreams",
        json!({
            "alias": "methods.example.com",
            "protocol": PROTOCOL,
            "server": { "endpoints": [ { "scheme": "https", "host": "methods.example.com" } ] },
            "cors": { "enabled": true, "allowed_methods": ["TRACE"] },
        }),
        tenant(),
    )
    .await;
    assert_eq!(response.status(), 400);
    let body = json_body(response).await;
    assert!(
        body["detail"].as_str().unwrap().contains("allowed_methods"),
        "the problem document must name the rule: {}",
        body["detail"]
    );
}

#[tokio::test]
async fn a_relative_origin_is_rejected_at_configuration_time() {
    let harness = common::Harness::new(common::test_config(), None);
    let response = post(
        harness.router(),
        "/oagw/v1/upstreams",
        json!({
            "alias": "relative.example.com",
            "protocol": PROTOCOL,
            "server": { "endpoints": [ { "scheme": "https", "host": "relative.example.com" } ] },
            "cors": { "enabled": true, "allowed_origins": ["app.example.com"] },
        }),
        tenant(),
    )
    .await;
    assert_eq!(response.status(), 400);
    let body = json_body(response).await;
    assert!(
        body["detail"].as_str().unwrap().contains("absolute origin"),
        "the problem document must name the rule: {}",
        body["detail"]
    );
}

#[tokio::test]
async fn a_relative_origin_is_rejected_on_a_route_too() {
    let server = MockServer::start();
    let harness = common::Harness::new(common::test_config(), None);
    let created = json_body(
        post(
            harness.router(),
            "/oagw/v1/upstreams",
            json!({
                "alias": "route-cors.example.com",
                "protocol": PROTOCOL,
                "server": { "endpoints": [ { "scheme": "http", "host": server.host(), "port": server.port() } ] },
            }),
            tenant(),
        )
        .await,
    )
    .await;
    let id = created["id"].as_str().unwrap().to_owned();
    let response = post(
        harness.router(),
        "/oagw/v1/routes",
        json!({
            "upstream_id": id,
            "match": { "http": { "methods": ["GET"], "path": "/" } },
            "cors": { "enabled": true, "allowed_origins": ["app.example.com"] },
        }),
        tenant(),
    )
    .await;
    assert_eq!(response.status(), 400);
}

#[tokio::test]
async fn wildcard_origin_matches_every_caller() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::GET);
        then.status(200).body("ok");
    });

    let harness = common::Harness::new(common::test_config(), None);
    register_upstream(
        &harness,
        &server,
        json!({ "enabled": true, "allowed_origins": ["*"] }),
    )
    .await;

    for origin in ["https://a.example.com", "https://b.example.org"] {
        let response = harness
            .send_request(
                request("cors-up.example.com", "GET", Some(origin), &[]),
                tenant(),
            )
            .await;
        assert_eq!(response.status(), 200, "{origin} must be matched by '*'");
        assert_eq!(
            common::header(&response, "access-control-allow-origin").as_deref(),
            Some(origin),
            "the caller's origin is echoed, never '*' with credentials"
        );
    }
}
