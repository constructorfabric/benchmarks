//! Router-level tests for the built-in CORS handler (ADR-0004).
//!
//! Preflight is answered locally and permissively; origin and method are
//! validated on the actual request against the resolved configuration.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use axum::http::StatusCode;
use common::{GATEWAY_SOURCE, Harness, JsonConfig, record, request, request_with};

/// A gateway with one upstream whose CORS policy is `cors`, if given.
async fn gateway_with_cors(cors: Option<serde_json::Value>) -> Harness {
    let harness = Harness::build(&JsonConfig::new(true, 1024 * 1024), Vec::new())
        .await
        .0;
    harness.simple_upstream("echo", cors).await;
    harness
}

#[tokio::test]
async fn a_preflight_is_answered_locally_even_for_an_unknown_alias() {
    let harness = gateway_with_cors(None).await;
    let response = record(
        harness
            .serve(request_with(
                "OPTIONS",
                "/oagw/v1/proxy/no-such-alias/anything",
                &[
                    ("origin", "https://anywhere.test"),
                    ("access-control-request-method", "POST"),
                    ("access-control-request-headers", "content-type,x-api-key"),
                ],
            ))
            .await,
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::NO_CONTENT,
        "a preflight needs no route: {}",
        response.raw
    );
    assert_eq!(response.header("access-control-allow-origin"), Some("https://anywhere.test"));
    assert_eq!(response.header("access-control-allow-methods"), Some("POST"));
    assert_eq!(
        response.header("access-control-allow-headers"),
        Some("content-type,x-api-key")
    );
    assert_eq!(response.header("access-control-max-age"), Some("86400"));
    assert_eq!(response.source(), None, "the gateway answered, not an upstream");
}

#[tokio::test]
async fn a_preflight_carries_the_full_vary_set() {
    let harness = gateway_with_cors(None).await;
    let response = record(
        harness
            .serve(request_with(
                "OPTIONS",
                "/oagw/v1/proxy/echo/echo",
                &[
                    ("origin", "https://anywhere.test"),
                    ("access-control-request-method", "GET"),
                ],
            ))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::NO_CONTENT);
    let vary = response.header("vary").unwrap_or_default();
    for token in [
        "Origin",
        "Access-Control-Request-Method",
        "Access-Control-Request-Headers",
    ] {
        assert!(vary.contains(token), "`vary` names `{token}`: {vary}");
    }
}

#[tokio::test]
async fn an_origin_must_match_exactly() {
    // A different port is a different origin, and so is a different scheme.
    let harness = gateway_with_cors(Some(serde_json::json!({
        "cors": {
            "enabled": true,
            "allowed_origins": ["https://app.example.com"],
            "allowed_methods": ["GET", "POST"],
        },
    })))
    .await;
    for origin in ["https://app.example.com:8080", "http://app.example.com"] {
        let response = record(
            harness
                .serve(request_with(
                    "GET",
                    "/oagw/v1/proxy/echo/echo",
                    &[("origin", origin)],
                ))
                .await,
        )
        .await;
        assert_eq!(response.status, StatusCode::FORBIDDEN, "`{origin}` is not allowed");
        assert_eq!(
            response.body["type"],
            "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1"
        );
    }
}

#[tokio::test]
async fn a_wildcard_origin_admits_every_caller() {
    let harness = gateway_with_cors(Some(serde_json::json!({
        "cors": { "enabled": true, "allowed_origins": ["*"] },
    })))
    .await;
    let response = record(
        harness
            .serve(request_with(
                "GET",
                "/oagw/v1/proxy/echo/echo",
                &[("origin", "https://whoever.test")],
            ))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.raw);
    assert_eq!(response.header("access-control-allow-origin"), Some("https://whoever.test"));
}

#[tokio::test]
async fn a_disallowed_method_is_a_403() {
    let harness = gateway_with_cors(Some(serde_json::json!({
        "cors": {
            "enabled": true,
            "allowed_origins": ["https://allowed.test"],
            "allowed_methods": ["GET"],
        },
    })))
    .await;
    let response = record(
        harness
            .serve(request_with(
                "DELETE",
                "/oagw/v1/proxy/echo/echo",
                &[("origin", "https://allowed.test")],
            ))
            .await,
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::FORBIDDEN,
        "the origin is fine, the method is not: {}",
        response.raw
    );
    assert_eq!(
        response.body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1"
    );
    assert_eq!(response.source(), Some(GATEWAY_SOURCE));
}

#[tokio::test]
async fn an_allowed_origin_and_method_are_proxied_with_the_cors_headers() {
    let harness = gateway_with_cors(Some(serde_json::json!({
        "cors": {
            "enabled": true,
            "allowed_origins": ["https://allowed.test"],
            "allowed_methods": ["GET", "POST"],
            "expose_headers": ["X-Request-ID"],
            "allow_credentials": true,
        },
    })))
    .await;
    let response = record(
        harness
            .serve(request_with(
                "POST",
                "/oagw/v1/proxy/echo/echo",
                &[("origin", "https://allowed.test")],
            ))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.raw);
    assert_eq!(response.header("access-control-allow-origin"), Some("https://allowed.test"));
    assert_eq!(response.header("access-control-expose-headers"), Some("X-Request-ID"));
    assert_eq!(response.header("access-control-allow-credentials"), Some("true"));
    assert_eq!(response.header("vary"), Some("Origin"));
    assert_eq!(response.source(), Some("upstream"), "the request was forwarded");
}

#[tokio::test]
async fn a_request_without_an_origin_is_not_a_cross_origin_request() {
    let harness = gateway_with_cors(Some(serde_json::json!({
        "cors": { "enabled": true, "allowed_origins": ["https://allowed.test"] },
    })))
    .await;
    let response = record(
        harness
            .serve(request("DELETE", "/oagw/v1/proxy/echo/echo", None))
            .await,
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "no `Origin`, no CORS check: {}",
        response.raw
    );
    assert_eq!(response.source(), Some("upstream"));
}

#[tokio::test]
async fn cors_is_off_unless_it_is_enabled() {
    let harness = gateway_with_cors(Some(serde_json::json!({
        "cors": { "enabled": false, "allowed_origins": [] },
    })))
    .await;
    let response = record(
        harness
            .serve(request_with(
                "GET",
                "/oagw/v1/proxy/echo/echo",
                &[("origin", "https://whoever.test")],
            ))
            .await,
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "a disabled policy lets the request through: {}",
        response.raw
    );
    assert_eq!(
        response.header("access-control-allow-origin"),
        None,
        "no CORS headers are added when the policy is off"
    );
}

#[tokio::test]
async fn an_upstream_with_no_cors_field_at_all_imposes_nothing() {
    let harness = gateway_with_cors(None).await;
    let response = record(
        harness
            .serve(request_with(
                "DELETE",
                "/oagw/v1/proxy/echo/echo",
                &[("origin", "https://whoever.test")],
            ))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.raw);
    assert_eq!(response.header("access-control-allow-origin"), None);
}

#[tokio::test]
async fn a_preflight_needs_no_security_context() {
    let harness = gateway_with_cors(None).await;
    let response = record(
        harness
            .serve_unauthenticated(request_with(
                "OPTIONS",
                "/oagw/v1/proxy/echo/echo",
                &[
                    ("origin", "https://browser.test"),
                    ("access-control-request-method", "POST"),
                ],
            ))
            .await,
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::NO_CONTENT,
        "a browser sends no credentials with a preflight: {}",
        response.raw
    );
    assert_eq!(response.header("access-control-allow-origin"), Some("https://browser.test"));
}

#[tokio::test]
async fn an_upgrade_is_refused_for_a_disallowed_origin() {
    let harness = gateway_with_cors(Some(serde_json::json!({
        "cors": { "enabled": true, "allowed_origins": ["https://allowed.test"] },
    })))
    .await;
    let response = record(
        harness
            .serve(request_with(
                "GET",
                "/oagw/v1/proxy/echo/ws",
                &[
                    ("origin", "https://disallowed.test"),
                    ("host", "127.0.0.1"),
                    ("connection", "Upgrade"),
                    ("upgrade", "websocket"),
                    ("sec-websocket-version", "13"),
                    ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
                ],
            ))
            .await,
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::FORBIDDEN,
        "an upgrade is a cross-origin request like any other: {}",
        response.raw
    );
    assert_eq!(
        response.body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1"
    );
}

#[tokio::test]
async fn an_upgrade_for_an_allowed_origin_carries_the_cors_headers() {
    let harness = gateway_with_cors(Some(serde_json::json!({
        "cors": { "enabled": true, "allowed_origins": ["https://allowed.test"] },
    })))
    .await;
    let response = record(
        harness
            .serve(request_with(
                "GET",
                "/oagw/v1/proxy/echo/ws",
                &[
                    ("origin", "https://allowed.test"),
                    ("host", "127.0.0.1"),
                    ("connection", "Upgrade"),
                    ("upgrade", "websocket"),
                    ("sec-websocket-version", "13"),
                    ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
                ],
            ))
            .await,
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::SWITCHING_PROTOCOLS,
        "the upgrade is allowed and negotiated: {}",
        response.raw
    );
    assert_eq!(
        response.header("access-control-allow-origin"),
        Some("https://allowed.test"),
        "the 101 carries the CORS answer for the caller"
    );
    assert_eq!(response.header("vary"), Some("Origin"));
}

#[tokio::test]
async fn a_route_level_cors_policy_is_applied() {
    let harness = Harness::build(&JsonConfig::new(true, 1024 * 1024), Vec::new())
        .await
        .0;
    // The upstream declares no CORS policy; the route does.
    harness
        .upstream_with_route(
            "echo",
            None,
            Some(serde_json::json!({
                "cors": { "enabled": true, "allowed_origins": ["https://route.test"] },
            })),
        )
        .await;
    let response = record(
        harness
            .serve(request_with(
                "GET",
                "/oagw/v1/proxy/echo/echo",
                &[("origin", "https://elsewhere.test")],
            ))
            .await,
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::FORBIDDEN,
        "the route's own policy is what governs: {}",
        response.raw
    );
    assert_eq!(
        response.body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1"
    );

    let response = record(
        harness
            .serve(request_with(
                "GET",
                "/oagw/v1/proxy/echo/echo",
                &[("origin", "https://route.test")],
            ))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.raw);
    assert_eq!(response.header("access-control-allow-origin"), Some("https://route.test"));
}
