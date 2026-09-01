// Data Plane integration tests: server-sent-events pass-through.
//
// The gateway must never buffer an SSE response: events are streamed chunk by
// chunk, so this test asserts on the full relayed event stream and on the
// `Content-Type` of the upstream response.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use axum::http::StatusCode;
use tenant_resolver_sdk::TenantId;

use common::{
    Harness, ProxyOptions, TestUpstream, context_for, gateway_request, sse_response, text,
};

fn options() -> ProxyOptions {
    common::ProxyOptions {
        proxy_timeout_secs: 5,
        allow_http_upstream: true,
        ssrf_enabled: false,
    }
}

#[tokio::test]
async fn server_sent_events_are_relayed_without_buffering() {
    let events = vec![
        "event: token\ndata: alpha",
        "event: token\ndata: beta",
        "event: done\ndata: [DONE]",
    ];
    let upstream = TestUpstream::start(move |_| {
        let events = events.clone();
        async move { sse_response(events) }
    })
    .await;
    let harness = Harness::new(options());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
    harness
        .local_upstream(&ctx, upstream.port(), "/", &["GET", "POST"])
        .await
        .expect("upstream");

    let request = gateway_request("POST", "/oagw/v1/proxy/local/v1/chat/completions");
    let mut response = harness.send(&ctx, request).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );
    assert_eq!(
        response
            .headers()
            .get("x-oagw-error-source")
            .and_then(|value| value.to_str().ok()),
        Some("upstream")
    );
    let body = text(&mut response).await;
    assert!(body.contains("event: token\ndata: alpha"), "{body}");
    assert!(body.contains("event: token\ndata: beta"), "{body}");
    assert!(body.contains("event: done\ndata: [DONE]"), "{body}");
    assert_eq!(body.matches("\n\n").count(), 3, "{body}");
}

#[tokio::test]
async fn a_long_stream_is_relayed_completely() {
    let count = 200_usize;
    let upstream = TestUpstream::start(move |_| {
        let stream = futures_util::stream::iter((0..count).map(|i| {
            Ok::<_, std::convert::Infallible>(bytes::Bytes::from(format!(
                "event: token\ndata: {i}\n\n"
            )))
        }));
        async move { common::sse_stream(StatusCode::OK, "text/event-stream", stream) }
    })
    .await;
    let harness = Harness::new(options());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
    harness
        .local_upstream(&ctx, upstream.port(), "/", &["GET"])
        .await
        .expect("upstream");

    let request = gateway_request("GET", "/oagw/v1/proxy/local/v1/stream");
    let mut response = harness.send(&ctx, request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = text(&mut response).await;
    assert_eq!(body.matches("event: token").count(), count, "{body}");
    assert!(body.contains("data: 199"), "{body}");
}

#[tokio::test]
async fn an_aborted_stream_surfaces_a_gateway_problem() {
    let upstream = TestUpstream::start(|_| async {
        http::Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(axum::body::Body::from_stream(futures_util::stream::iter(
                vec![Ok::<_, std::convert::Infallible>(bytes::Bytes::from(
                    "event: token\ndata: 1\n\n",
                ))],
            )))
            .expect("response")
    })
    .await;
    let harness = Harness::new(options());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
    harness
        .local_upstream(&ctx, upstream.port(), "/", &["GET"])
        .await
        .expect("upstream");

    let request = gateway_request("GET", "/oagw/v1/proxy/local/v1/stream");
    let mut response = harness.send(&ctx, request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = text(&mut response).await;
    assert!(body.contains("event: token"), "{body}");
}
