//! Streaming proxy behaviour: server-sent events and WebSocket upgrades.
//!
//! `cpt-cf-oagw-fr-streaming` and `cpt-cf-oagw-usecase-sse-streaming`. Both
//! run over a real socket, because neither is observable through a buffered
//! request/response pair.

mod common;

use common::{Fixture, get, ws};
use http::StatusCode;
use oagw::domain::model::{Endpoint, Scheme, ServerConfig};
use serde_json::json;

#[tokio::test]
async fn sse_events_are_forwarded_as_they_are_produced() {
    let fx = Fixture::start().await;
    fx.simple("mock").await;

    let (status, headers, text) =
        common::stream_text(&fx.proxy_url("mock", "v1/sse"), |acc| {
            acc.contains("event-2")
        })
        .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.starts_with("text/event-stream")),
        Some(true),
        "the SSE content type survives the proxy"
    );
    assert_eq!(
        headers
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some("upstream")
    );
    for index in 0..3 {
        assert!(
            text.contains(&format!("event-{index}")),
            "event-{index} should have been forwarded; got: {text}"
        );
    }
}

#[tokio::test]
async fn an_sse_stream_is_not_buffered_to_completion_before_the_first_event() {
    let fx = Fixture::start().await;
    fx.simple("mock").await;

    // The mock spaces its three events 20 ms apart, so a buffering proxy
    // could not surface the first one in under ~60 ms.
    let started = std::time::Instant::now();
    let (_, _, text) = common::stream_text(&fx.proxy_url("mock", "v1/sse"), |acc| {
        acc.contains("event-0")
    })
    .await;
    let elapsed = started.elapsed();

    assert!(text.contains("event-0"));
    assert!(
        elapsed < std::time::Duration::from_millis(60),
        "the first event arrived after {elapsed:?}, which suggests the body was buffered"
    );
}

#[tokio::test]
async fn a_websocket_upgrade_is_relayed_end_to_end() {
    let fx = Fixture::start().await;
    fx.simple("mock").await;

    let mut handshake = ws::connect(fx.gateway.addr, "/oagw/v1/proxy/mock/v1/ws").await;
    assert!(
        handshake.status_line.contains("101"),
        "expected a protocol upgrade, got: {}",
        handshake.status_line
    );
    assert_eq!(handshake.header("upgrade"), Some("websocket"));
    assert!(
        handshake.header("sec-websocket-accept").is_some(),
        "the upstream's own handshake response is passed back"
    );

    ws::send_text(&mut handshake.socket, "hello").await;
    let echoed = ws::recv_text(&mut handshake.socket, &mut handshake.leftover).await;
    assert_eq!(echoed, "echo:hello");

    ws::send_text(&mut handshake.socket, "again").await;
    let echoed = ws::recv_text(&mut handshake.socket, &mut handshake.leftover).await;
    assert_eq!(
        echoed, "echo:again",
        "the relay stays open across multiple frames"
    );
}

#[tokio::test]
async fn an_upgrade_to_an_unmatched_route_fails_before_the_handshake() {
    let fx = Fixture::start().await;
    let upstream = fx.upstream("mock", |_| {}).await;
    // Only `/v1/echo` is routed; `/v1/ws` is not.
    fx.route(&upstream, &["GET"], "/v1/echo", |_| {}).await;

    let handshake = ws::connect(fx.gateway.addr, "/oagw/v1/proxy/mock/v1/ws").await;
    assert!(
        handshake.status_line.contains("404"),
        "expected a gateway 404, got: {}",
        handshake.status_line
    );
}

#[tokio::test]
async fn an_upgrade_to_a_non_upgrading_upstream_path_returns_the_upstream_response() {
    let fx = Fixture::start().await;
    fx.simple("mock").await;

    // `/v1/echo` is a plain handler: it answers 200 rather than 101, and the
    // gateway must relay that instead of hanging on an upgrade that never
    // happens.
    let handshake = ws::connect(fx.gateway.addr, "/oagw/v1/proxy/mock/v1/echo").await;
    assert!(
        handshake.status_line.contains("200"),
        "expected the upstream's plain response, got: {}",
        handshake.status_line
    );
}

#[tokio::test]
async fn a_websocket_scheme_endpoint_proxies_over_the_same_path() {
    let fx = Fixture::start().await;
    // `ws` is the plaintext member of the WebSocket family; the connection
    // itself is plain TCP either way, so the mock serves it unchanged.
    let upstream = fx
        .upstream("wsmock", |spec| {
            spec.server = ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: Scheme::Ws,
                    host: fx.upstream.host(),
                    port: Some(fx.upstream.port()),
                }],
            };
        })
        .await;
    fx.route(&upstream, &["GET"], "/v1/ws", |_| {}).await;

    let mut handshake = ws::connect(fx.gateway.addr, "/oagw/v1/proxy/wsmock/v1/ws").await;
    assert!(
        handshake.status_line.contains("101"),
        "expected a protocol upgrade, got: {}",
        handshake.status_line
    );
    ws::send_text(&mut handshake.socket, "ping").await;
    assert_eq!(
        ws::recv_text(&mut handshake.socket, &mut handshake.leftover).await,
        "echo:ping"
    );
}

#[tokio::test]
async fn a_plain_request_still_works_alongside_streaming_routes() {
    let fx = Fixture::start().await;
    fx.simple("mock").await;

    // Interleave the three shapes on one gateway to catch any state the
    // upgrade path might leave behind.
    assert_eq!(get(&fx.proxy_url("mock", "v1/echo")).await.status, StatusCode::OK);

    let mut handshake = ws::connect(fx.gateway.addr, "/oagw/v1/proxy/mock/v1/ws").await;
    assert!(handshake.status_line.contains("101"));
    ws::send_text(&mut handshake.socket, "x").await;
    assert_eq!(
        ws::recv_text(&mut handshake.socket, &mut handshake.leftover).await,
        "echo:x"
    );

    let (status, _, text) = common::stream_text(&fx.proxy_url("mock", "v1/sse"), |acc| {
        acc.contains("event-2")
    })
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(text.contains("event-2"));

    let res = get(&fx.proxy_url("mock", "v1/echo")).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.json()["path"], json!("/v1/echo"));
}
