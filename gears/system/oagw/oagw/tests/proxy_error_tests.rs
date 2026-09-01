// Data Plane integration tests: the error semantics of the proxy surface.
//
// 429 (rate limit), 502 (broken upstream), 503 (unreachable upstream / open
// circuit breaker), 413 (payload too large) and 400 (unsupported transfer
// encoding) are asserted through the real transport so the mapping from
// `DomainError` to an RFC 9457 problem response happens end to end.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{
    Harness, ProxyOptions, TestUpstream, context_for, gateway_request, http_route, text,
    upstream_shell,
};
use oagw::domain::model::{
    BurstCapacity, RateAlgorithm, RateLimitConfig, RateScope, RateStrategy, SharingMode,
    SustainedRate,
};
use tenant_resolver_sdk::TenantId;

fn options() -> ProxyOptions {
    ProxyOptions {
        proxy_timeout_secs: 5,
        allow_http_upstream: true,
        ssrf_enabled: false,
    }
}

/// A private token bucket of `capacity` tokens per second.
fn rate_limit(capacity: u32) -> RateLimitConfig {
    RateLimitConfig {
        sharing: SharingMode::Private,
        algorithm: RateAlgorithm::TokenBucket,
        sustained: SustainedRate {
            rate: capacity,
            window: oagw::domain::model::RateWindow::Second,
        },
        burst: Some(BurstCapacity { capacity }),
        scope: RateScope::Tenant,
        strategy: RateStrategy::Reject,
        cost: 1,
        response_headers: true,
    }
}

/// A TCP listener that accepts and closes each connection without answering.
///
/// The upstream accepts the socket and then vanishes, which is the transport
/// level failure the gateway must report as `502 Downstream Error`.
async fn silent_upstream() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                break;
            };
            drop(socket);
        }
    });
    port
}

#[tokio::test]
async fn an_exhausted_rate_limit_is_a_429_with_retry_after() {
    let upstream = common::echo_upstream().await;
    let harness = Harness::new(options());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
    let created = harness
        .control_plane()
        .create_upstream(&ctx, upstream_shell(upstream.port()))
        .await
        .expect("upstream");
    let mut route = http_route(created.id, "/", &["GET"]);
    route.rate_limit = Some(rate_limit(1));
    harness
        .control_plane()
        .create_route(&ctx, route)
        .await
        .expect("route");

    let first = harness
        .send(
            &ctx,
            gateway_request("GET", "/oagw/v1/proxy/local/v1/items"),
        )
        .await;
    assert_eq!(first.status(), StatusCode::OK);

    let mut second = harness
        .send(
            &ctx,
            gateway_request("GET", "/oagw/v1/proxy/local/v1/items"),
        )
        .await;
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(second.headers().get("retry-after").is_some());
    assert_eq!(
        second
            .headers()
            .get("x-oagw-error-source")
            .and_then(|value| value.to_str().ok()),
        Some("gateway")
    );
    let body = text(&mut second).await;
    assert!(body.contains("cf.oagw.rate_limit.exceeded.v1"), "{body}");
    assert!(body.contains("RATE_LIMIT_EXCEEDED"), "{body}");
}

#[tokio::test]
async fn a_connection_that_closes_without_a_response_is_a_502() {
    let port = silent_upstream().await;
    let harness = Harness::new(options());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
    harness
        .local_upstream(&ctx, port, "/", &["GET"])
        .await
        .expect("route");

    let mut response = harness
        .send(
            &ctx,
            gateway_request("GET", "/oagw/v1/proxy/local/v1/items"),
        )
        .await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY, "{response:?}");
    assert_eq!(
        response
            .headers()
            .get("x-oagw-error-source")
            .and_then(|value| value.to_str().ok()),
        Some("gateway")
    );
    let body = text(&mut response).await;
    assert!(body.contains("cf.oagw.downstream.error.v1"), "{body}");
}

#[tokio::test]
async fn an_unreachable_upstream_is_a_503_link_unavailable() {
    let harness = Harness::new(options());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
    harness
        .local_upstream(&ctx, 9, "/", &["GET"])
        .await
        .expect("route");

    let mut response = harness
        .send(
            &ctx,
            gateway_request("GET", "/oagw/v1/proxy/local/v1/items"),
        )
        .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response
            .headers()
            .get("x-oagw-error-source")
            .and_then(|value| value.to_str().ok()),
        Some("gateway")
    );
    let body = text(&mut response).await;
    assert!(body.contains("cf.oagw.link.unavailable.v1"), "{body}");
    assert!(body.contains("LINK_UNAVAILABLE"), "{body}");
}

#[tokio::test]
async fn an_open_circuit_breaker_is_a_503_with_a_retry_after() {
    let harness = Harness::new(options());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
    harness
        .local_upstream(&ctx, 9, "/", &["GET"])
        .await
        .expect("route");

    // Repeated failures trip the breaker: the first responses are 503 link
    // unavailable, later ones are reported as 503 circuit open.
    let mut saw_circuit_open = false;
    for _ in 0..8 {
        let mut response = harness
            .send(
                &ctx,
                gateway_request("GET", "/oagw/v1/proxy/local/v1/items"),
            )
            .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = text(&mut response).await;
        if body.contains("cf.oagw.circuit_breaker.open.v1") {
            saw_circuit_open = true;
            assert!(response.headers().get("retry-after").is_some());
        }
    }
    assert!(saw_circuit_open, "the breaker never opened");
}

#[tokio::test]
async fn a_content_length_above_the_hard_limit_is_a_413() {
    let upstream = common::echo_upstream().await;
    let harness = Harness::new(options());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
    harness
        .local_upstream(&ctx, upstream.port(), "/", &["POST"])
        .await
        .expect("route");

    // The declared length is rejected before a single body byte is read.
    let request = Request::builder()
        .method("POST")
        .uri("/oagw/v1/proxy/local/v1/upload")
        .header("content-length", "104857601")
        .body(Body::empty())
        .unwrap();
    let mut response = harness.send(&ctx, request).await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        response
            .headers()
            .get("x-oagw-error-source")
            .and_then(|value| value.to_str().ok()),
        Some("gateway")
    );
    let body = text(&mut response).await;
    assert!(body.contains("cf.oagw.payload.too_large.v1"), "{body}");
    assert!(body.contains("PAYLOAD_TOO_LARGE"), "{body}");
}

#[tokio::test]
async fn an_unsupported_transfer_encoding_is_a_400() {
    let upstream = common::echo_upstream().await;
    let harness = Harness::new(options());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
    harness
        .local_upstream(&ctx, upstream.port(), "/", &["POST"])
        .await
        .expect("route");

    let request = Request::builder()
        .method("POST")
        .uri("/oagw/v1/proxy/local/v1/upload")
        .header("transfer-encoding", "gzip")
        .body(Body::from("payload"))
        .unwrap();
    let mut response = harness.send(&ctx, request).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = text(&mut response).await;
    assert!(body.contains("cf.oagw.validation.error.v1"), "{body}");
}

#[tokio::test]
async fn a_malformed_content_length_is_a_400() {
    let upstream = TestUpstream::start(|_| async { common::respond(StatusCode::OK, "ok") }).await;
    let harness = Harness::new(options());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
    harness
        .local_upstream(&ctx, upstream.port(), "/", &["POST"])
        .await
        .expect("route");

    let request = Request::builder()
        .method("POST")
        .uri("/oagw/v1/proxy/local/v1/upload")
        .header("content-length", "twelve")
        .body(Body::from("payload"))
        .unwrap();
    let mut response = harness.send(&ctx, request).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = text(&mut response).await;
    assert!(body.contains("cf.oagw.validation.error.v1"), "{body}");
}
