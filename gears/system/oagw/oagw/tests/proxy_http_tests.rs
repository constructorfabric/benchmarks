// Data Plane integration tests: plain HTTP proxying against a real upstream.
//
// Every test drives the OAGW sub-router with `tower::ServiceExt::oneshot` and
// reaches a real TCP upstream, so status codes, headers, streamed bodies and
// the `X-OAGW-Error-Source` contract are exercised end to end.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use oagw::domain::model::PathSuffixMode;
use tenant_resolver_sdk::TenantId;

use common::{
    Harness, ProxyOptions, chunked_upstream, context_for, echo_upstream, gateway_request,
    gateway_request_with, http_route, text, upstream_shell,
};

fn options() -> ProxyOptions {
    ProxyOptions {
        proxy_timeout_secs: 5,
        allow_http_upstream: true,
        ssrf_enabled: false,
    }
}

#[tokio::test]
async fn proxy_round_trips_method_path_query_and_body() {
    let upstream = echo_upstream().await;
    let harness = Harness::new(options());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
    harness
        .local_upstream(&ctx, upstream.port(), "/", &["POST", "GET"])
        .await
        .expect("upstream");

    let request = gateway_request_with("POST", "/oagw/v1/proxy/local/v1/chat", "payload");
    let mut response = harness.send(&ctx, request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = text(&mut response).await;
    assert!(body.starts_with("POST /v1/chat? payload"), "{body}");
    assert_eq!(
        response
            .headers()
            .get("x-oagw-error-source")
            .and_then(|value| value.to_str().ok()),
        Some("upstream")
    );
}

#[tokio::test]
async fn proxy_streams_a_chunked_response() {
    let upstream = chunked_upstream(64).await;
    let harness = Harness::new(options());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
    harness
        .local_upstream(&ctx, upstream.port(), "/", &["GET"])
        .await
        .expect("upstream");

    let request = gateway_request("GET", "/oagw/v1/proxy/local/v1/feed");
    let mut response = harness.send(&ctx, request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = text(&mut response).await;
    assert_eq!(body.lines().count(), 64);
    assert!(body.starts_with("chunk-00"));
    assert!(body.ends_with("chunk-63\n"));
}

#[tokio::test]
async fn proxy_relays_a_request_body_to_the_upstream() {
    let upstream = echo_upstream().await;
    let harness = Harness::new(options());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
    harness
        .local_upstream(&ctx, upstream.port(), "/", &["PUT"])
        .await
        .expect("upstream");

    let payload = "x".repeat(200_000);
    let request = Request::builder()
        .method("PUT")
        .uri("/oagw/v1/proxy/local/v1/items")
        .header("content-type", "text/plain")
        .body(Body::from(payload.clone()))
        .expect("request");
    let mut response = harness.send(&ctx, request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = text(&mut response).await;
    assert_eq!(body, format!("PUT /v1/items? {payload}"));
}

#[tokio::test]
async fn unknown_alias_is_a_404_problem() {
    let harness = Harness::new(options());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));

    let mut response = harness
        .send(
            &ctx,
            gateway_request("GET", "/oagw/v1/proxy/does-not-exist/v1"),
        )
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        response
            .headers()
            .get("x-oagw-error-source")
            .and_then(|value| value.to_str().ok()),
        Some("gateway")
    );
    let body = text(&mut response).await;
    assert!(body.contains("cf.oagw.route.not_found.v1"), "{body}");
    assert!(body.contains("ROUTE_NOT_FOUND"), "{body}");
}

#[tokio::test]
async fn no_matching_route_is_a_404_and_an_unknown_method_too() {
    let upstream = echo_upstream().await;
    let harness = Harness::new(options());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
    harness
        .local_upstream(&ctx, upstream.port(), "/v1", &["GET"])
        .await
        .expect("upstream");

    let request = gateway_request("DELETE", "/oagw/v1/proxy/local/v1");
    let response = harness.send(&ctx, request).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let request = gateway_request("GET", "/oagw/v1/proxy/local/other");
    let response = harness.send(&ctx, request).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_route_path_is_prepended_to_the_path_suffix() {
    // DESIGN.md §"Header transformation rules": the path suffix captured after
    // the alias is *appended to* `match.http.path`, so a route rooted at
    // `/base` yields `/base/base/v1/feed` for the request below. A route that
    // wants the client path relayed verbatim configures `path: "/"`.
    let upstream = echo_upstream().await;
    let harness = Harness::new(options());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
    harness
        .local_upstream(&ctx, upstream.port(), "/base", &["GET"])
        .await
        .expect("upstream");

    let request = gateway_request("GET", "/oagw/v1/proxy/local/base/v1/feed");
    let mut response = harness.send(&ctx, request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = text(&mut response).await;
    assert!(body.starts_with("GET /base/base/v1/feed? "), "{body}");
}

#[tokio::test]
async fn a_root_route_relays_the_client_path_verbatim() {
    let upstream = echo_upstream().await;
    let harness = Harness::new(options());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
    harness
        .local_upstream(&ctx, upstream.port(), "/", &["GET"])
        .await
        .expect("upstream");

    let request = gateway_request("GET", "/oagw/v1/proxy/local/v1/chat/completions");
    let mut response = harness.send(&ctx, request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = text(&mut response).await;
    assert!(body.starts_with("GET /v1/chat/completions? "), "{body}");
}

#[tokio::test]
async fn a_disabled_path_suffix_mode_rejects_a_suffix() {
    let upstream = echo_upstream().await;
    let harness = Harness::new(options());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
    let created = harness
        .control_plane()
        .create_upstream(&ctx, upstream_shell(upstream.port()))
        .await
        .expect("upstream");
    let mut route = http_route(created.id, "/", &["GET"]);
    if let Some(http) = route.match_config.http.as_mut() {
        http.path_suffix_mode = PathSuffixMode::Disabled;
    }
    harness
        .control_plane()
        .create_route(&ctx, route)
        .await
        .expect("route");

    let request = gateway_request("GET", "/oagw/v1/proxy/local");
    assert_eq!(harness.send(&ctx, request).await.status(), StatusCode::OK);
    let request = gateway_request("GET", "/oagw/v1/proxy/local/extra");
    assert_eq!(
        harness.send(&ctx, request).await.status(),
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn disallowed_query_parameter_is_a_400() {
    let upstream = echo_upstream().await;
    let harness = Harness::new(options());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
    let created = harness
        .control_plane()
        .create_upstream(&ctx, upstream_shell(upstream.port()))
        .await
        .expect("upstream");
    let mut restricted = http_route(created.id, "/", &["GET"]);
    if let Some(http) = restricted.match_config.http.as_mut() {
        http.query_allowlist = vec!["api-version".to_owned()];
    }
    harness
        .control_plane()
        .create_route(&ctx, restricted)
        .await
        .expect("route");

    let request = gateway_request("GET", "/oagw/v1/proxy/local/v1?api-version=1");
    assert_eq!(harness.send(&ctx, request).await.status(), StatusCode::OK);
    let request = gateway_request("GET", "/oagw/v1/proxy/local/v1?other=1");
    assert_eq!(
        harness.send(&ctx, request).await.status(),
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn unreachable_upstream_is_a_503_with_the_gateway_source_header() {
    let harness = Harness::new(options());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
    harness
        .local_upstream(&ctx, 1, "/", &["GET"])
        .await
        .expect("upstream");

    let request = gateway_request("GET", "/oagw/v1/proxy/local/v1");
    let response = harness.send(&ctx, request).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response
            .headers()
            .get("x-oagw-error-source")
            .and_then(|value| value.to_str().ok()),
        Some("gateway")
    );
}

#[tokio::test]
async fn the_api_prefix_serves_the_same_proxy_surface() {
    let upstream = echo_upstream().await;
    let harness = Harness::new(options());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
    harness
        .local_upstream(&ctx, upstream.port(), "/", &["GET"])
        .await
        .expect("upstream");

    let request = gateway_request("GET", "/api/oagw/v1/proxy/local/v1");
    let mut response = harness.send_routed(&ctx, request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = text(&mut response).await;
    assert!(body.starts_with("GET /v1? "), "{body}");
}

#[tokio::test]
async fn head_is_served_by_a_get_route() {
    let upstream = echo_upstream().await;
    let harness = Harness::new(options());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
    harness
        .local_upstream(&ctx, upstream.port(), "/", &["GET"])
        .await
        .expect("upstream");

    let request = gateway_request("HEAD", "/oagw/v1/proxy/local/v1");
    let response = harness.send(&ctx, request).await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn an_anonymous_caller_is_rejected() {
    let harness = Harness::new(options());
    let ctx = common::anonymous();

    let request = gateway_request("GET", "/oagw/v1/proxy/local/v1");
    let mut response = harness.send(&ctx, request).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        text(&mut response)
            .await
            .contains("cf.oagw.validation.error.v1")
    );
}
