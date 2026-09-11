//! WebSocket upgrades are dialled through the gateway and bridged both ways.
//!
//! An upgrade is owned by the HTTP server, so these tests mount the gear on a
//! loopback port and dial it with a real client rather than driving the router
//! in-process. The upstream is a local echo server: every frame the test sends
//! must come back, which is what "bidirectional" means here.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{LocalUpstream, app};
use futures_util::{SinkExt, StreamExt};

/// Wire an upstream to `/ws` and return the address the gateway listens on.
///
/// The upstream's echo endpoint is `/ws`, and the route forwards the path
/// verbatim, so the caller dials the same path through the gateway.
async fn wired(app: &common::TestApp, upstream: &LocalUpstream) -> std::net::SocketAddr {
    let upstream_doc = app.create_upstream(upstream.upstream_spec("local")).await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();
    app.create_route(serde_json::json!({
        "path": "/ws",
        "methods": ["GET"],
        "target_alias": alias,
        "strip_prefix": false
    }))
    .await;
    app.serve_with_subject(app.tenant).await
}

fn url(addr: std::net::SocketAddr) -> String {
    format!("ws://{addr}/oagw/v1/proxy/local/ws")
}

/// Connect through the gateway, exchanging `messages` with the echo upstream.
async fn echo_round_trip(
    addr: std::net::SocketAddr,
    messages: &[&str],
) -> Vec<tokio_tungstenite::tungstenite::Message> {
    let (mut socket, response) = tokio_tungstenite::connect_async(url(addr))
        .await
        .expect("the gateway completes the upgrade");
    assert_eq!(
        response.status(),
        http::StatusCode::SWITCHING_PROTOCOLS,
        "the caller is answered with 101"
    );

    let mut echoes = Vec::new();
    for message in messages {
        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                (*message).into(),
            ))
            .await
            .expect("the caller can send");
        let back = socket
            .next()
            .await
            .expect("the upstream echoes")
            .expect("the echo is readable");
        echoes.push(back);
    }
    socket.close(None).await.ok();
    echoes
}

#[tokio::test]
async fn an_upgrade_is_completed_and_frames_travel_both_ways() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let addr = wired(&app, &upstream).await;

    let echoes = echo_round_trip(addr, &["hello", "gateway", "echo"]).await;
    let texts: Vec<String> = echoes
        .iter()
        .map(|message| message.to_text().expect("a text frame").to_owned())
        .collect();
    assert_eq!(
        texts,
        vec!["hello", "gateway", "echo"],
        "each frame is echoed"
    );
    assert_eq!(upstream.count(), 1, "one upgrade reached the upstream");
}

#[tokio::test]
async fn a_longer_exchange_stays_bidirectional() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let addr = wired(&app, &upstream).await;

    let messages: Vec<String> = (0..20).map(|index| format!("frame-{index}")).collect();
    let echoes = echo_round_trip(
        addr,
        &messages.iter().map(String::as_str).collect::<Vec<_>>(),
    )
    .await;
    let texts: Vec<String> = echoes
        .iter()
        .map(|message| message.to_text().expect("a text frame").to_owned())
        .collect();
    assert_eq!(texts, messages, "frames come back in order");
}

/// A binary frame survives the bridge untouched, not just text.
#[tokio::test]
async fn a_binary_frame_survives_the_tunnel() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let addr = wired(&app, &upstream).await;

    let payload: Vec<u8> = (0u8..=15).collect();
    let (mut socket, response) = tokio_tungstenite::connect_async(url(addr))
        .await
        .expect("the upgrade completes");
    assert_eq!(response.status(), http::StatusCode::SWITCHING_PROTOCOLS);

    socket
        .send(tokio_tungstenite::tungstenite::Message::Binary(
            payload.clone().into(),
        ))
        .await
        .expect("the frame is sent");
    let back = socket
        .next()
        .await
        .expect("the echo arrives")
        .expect("the echo is readable");
    assert_eq!(back.into_data(), payload, "binary data is not mangled");
    socket.close(None).await.ok();
}

/// The upstream's refusal is passed back instead of a 101 nobody can speak on.
#[tokio::test]
async fn a_refused_upgrade_is_reported() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let upstream_doc = app.create_upstream(upstream.upstream_spec("local")).await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();
    // A route whose target serves plain HTTP only: it answers the upgrade with
    // its own status, and the gateway must not answer 101 over it.
    app.create_route(serde_json::json!({
        "path": "/ws",
        "methods": ["GET"],
        "target_alias": alias,
        "strip_prefix": false
    }))
    .await;
    let addr = app.serve_with_subject(app.tenant).await;

    // The echo upstream answers an upgrade on a path it does not serve with
    // 404; the gateway relays that rather than switching protocols.
    let path = format!("/oagw/v1/proxy/{alias}/not-the-websocket-endpoint");
    let dialled = format!("ws://{addr}{path}");
    let error = tokio_tungstenite::connect_async(dialled)
        .await
        .expect_err("the upgrade is refused");
    let status = match &error {
        tokio_tungstenite::tungstenite::Error::Http(response) => response.status(),
        other => panic!("an HTTP refusal, not {other:?}"),
    };
    assert_eq!(
        status,
        http::StatusCode::NOT_FOUND,
        "the upstream's own refusal stands"
    );
}
