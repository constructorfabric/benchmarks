//! Tests for the WebSocket bridge.

use crate::domain::upstream::Endpoint;
use crate::proxy::target::{self, Target};
use crate::proxy::ws;

fn destination(scheme: &str, host: &str, port: u16) -> Target {
    let endpoint = Endpoint {
        scheme: scheme.to_owned(),
        host: host.to_owned(),
        port,
    };
    let authority = target::authority(&endpoint);
    Target {
        endpoint,
        authority,
        pinned: false,
    }
}

#[test]
fn the_ws_origin_follows_the_endpoint_scheme() {
    assert_eq!(
        ws::ws_origin(&destination("wss", "chat.example.com", 443)),
        "wss://chat.example.com"
    );
    // `https` implies `wss`: a TLS upstream carries the upgrade over TLS too.
    assert_eq!(
        ws::ws_origin(&destination("https", "chat.example.com", 443)),
        "wss://chat.example.com"
    );
    assert_eq!(
        ws::ws_origin(&destination("ws", "chat.example.com", 80)),
        "ws://chat.example.com"
    );
    assert_eq!(
        ws::ws_origin(&destination("http", "chat.example.com", 8080)),
        "ws://chat.example.com:8080"
    );
}

#[test]
fn a_query_string_is_carried_into_the_upstream_url() {
    let destination = destination("wss", "chat.example.com", 443);
    assert_eq!(
        ws::upstream_url(&destination, "/v1/socket", "room=1&token=abc"),
        "wss://chat.example.com/v1/socket?room=1&token=abc"
    );
    assert_eq!(
        ws::upstream_url(&destination, "/v1/socket", ""),
        "wss://chat.example.com/v1/socket"
    );
}

#[test]
fn a_non_standard_port_is_kept_in_the_origin() {
    assert_eq!(
        ws::ws_origin(&destination("ws", "chat.example.com", 9001)),
        "ws://chat.example.com:9001"
    );
}

#[test]
fn every_relayable_caller_frame_has_an_upstream_form() {
    use axum::extract::ws::Message;
    for frame in [
        Message::Text("hello".to_owned().into()),
        Message::Binary(bytes::Bytes::from_static(b"\x00\x01")),
        Message::Ping(bytes::Bytes::from_static(b"ping")),
        Message::Pong(bytes::Bytes::from_static(b"pong")),
    ] {
        let _converted = ws::convert_outbound(frame.clone());
    }
    let _converted = ws::convert_outbound(Message::Close(Some(
        axum::extract::ws::CloseFrame {
            code: 1000,
            reason: "bye".into(),
        },
    )));
}

#[test]
fn every_upstream_frame_has_a_caller_form() {
    use tokio_tungstenite::tungstenite::Message as Upstream;
    for frame in [
        Upstream::Text("hello".to_owned().into()),
        Upstream::Binary(vec![0u8, 1].into()),
        Upstream::Ping(vec![1u8].into()),
        Upstream::Pong(vec![2u8].into()),
        Upstream::Close(None),
    ] {
        assert!(ws::convert_inbound(frame).is_some());
    }
}

#[test]
fn a_raw_upstream_frame_is_not_relayed() {
    use tokio_tungstenite::tungstenite::Message as Upstream;
    // Raw frames are an internal detail of the upstream library, not protocol data.
    let raw = Upstream::Frame(tokio_tungstenite::tungstenite::protocol::frame::Frame::ping(
        bytes::Bytes::from_static(b"\0"),
    ));
    assert!(matches!(raw, Upstream::Frame(_)));
    assert!(
        ws::convert_inbound(raw).is_none(),
        "a raw frame is an internal detail of the upstream library"
    );
}

#[test]
fn a_close_code_survives_the_round_trip() {
    use tokio_tungstenite::tungstenite::Message as Upstream;
    let frame = ws::convert_inbound(Upstream::Close(Some(
        tokio_tungstenite::tungstenite::protocol::CloseFrame {
            code: 4409u16.into(),
            reason: "nope".into(),
        },
    )))
    .expect("converted");
    let axum::extract::ws::Message::Close(reason) = frame else {
        panic!("expected a close frame");
    };
    let reason = reason.expect("a reason is carried");
    assert_eq!(reason.code, 4409);
    assert_eq!(reason.reason, "nope");
}

#[test]
fn a_close_frame_crosses_to_the_upstream_with_its_reason() {
    use tokio_tungstenite::tungstenite::Message as Upstream;
    let frame = ws::convert_outbound(axum::extract::ws::Message::Close(Some(
        axum::extract::ws::CloseFrame {
            code: 1000,
            reason: "done".into(),
        },
    )));
    let Upstream::Close(reason) = frame else {
        panic!("expected an upstream close frame");
    };
    let reason = reason.expect("a reason is carried");
    assert_eq!(reason.code, 1000u16.into());
    assert_eq!(reason.reason.as_str(), "done");
}

#[test]
fn an_upgrade_request_is_recognised_and_its_routing_header_dropped() {
    let mut headers = http::HeaderMap::new();
    headers.insert("upgrade", "websocket".parse().unwrap());
    headers.insert("connection", "keep-alive, Upgrade".parse().unwrap());
    headers.insert("x-oagw-target-host", "chat.example.com".parse().unwrap());
    assert!(crate::proxy::is_websocket_upgrade(&headers));

    let out = crate::proxy::headers::build_request_headers(
        &headers,
        &crate::domain::upstream::HeadersConfig::default(),
        "chat.example.com",
        &[],
    );
    assert!(out.get("x-oagw-target-host").is_none());
    assert!(
        crate::proxy::headers::build_request_headers(
            &headers,
            &crate::domain::upstream::HeadersConfig::default(),
            "chat.example.com",
            &[],
        )
        .get("upgrade")
        .is_none(),
        "hop-by-hop headers are the bridge's job, not the rewrite's"
    );
}

#[test]
fn an_upgrade_without_the_connection_token_is_not_an_upgrade() {
    let mut headers = http::HeaderMap::new();
    headers.insert("upgrade", "websocket".parse().unwrap());
    assert!(!crate::proxy::is_websocket_upgrade(&headers));

    let mut headers = http::HeaderMap::new();
    headers.insert("connection", "upgrade".parse().unwrap());
    assert!(!crate::proxy::is_websocket_upgrade(&headers));
}
