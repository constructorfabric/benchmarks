//! Upgrade detection and handshake parsing.

use super::*;

fn upgrade_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
    headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    headers
}

#[test]
fn a_get_with_connection_upgrade_is_an_upgrade_request() {
    assert!(is_upgrade_request(&Method::GET, &upgrade_headers()));
}

#[test]
fn the_connection_token_may_sit_in_a_list() {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONNECTION,
        HeaderValue::from_static("keep-alive, Upgrade"),
    );
    headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    assert!(is_upgrade_request(&Method::GET, &headers));
}

#[test]
fn a_non_get_method_is_never_an_upgrade() {
    assert!(!is_upgrade_request(&Method::POST, &upgrade_headers()));
}

#[test]
fn an_upgrade_header_without_the_connection_token_does_not_count() {
    let mut headers = HeaderMap::new();
    headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    assert!(!is_upgrade_request(&Method::GET, &headers));
}

#[test]
fn an_ordinary_request_is_not_an_upgrade() {
    assert!(!is_upgrade_request(&Method::GET, &HeaderMap::new()));
}

#[test]
fn the_offered_protocol_is_reported_lowercased() {
    let mut headers = HeaderMap::new();
    headers.insert(header::UPGRADE, HeaderValue::from_static("WebSocket"));
    assert_eq!(upgrade_protocol(&headers).as_deref(), Some("websocket"));
    assert_eq!(upgrade_protocol(&HeaderMap::new()), None);
}

#[test]
fn the_head_terminator_is_found_at_the_right_offset() {
    let buffer = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\r\nPAYLOAD";
    let offset = find_head_end(buffer, 0).unwrap();
    assert_eq!(&buffer[offset..], b"PAYLOAD");
}

#[test]
fn an_incomplete_head_has_no_terminator_yet() {
    assert_eq!(find_head_end(b"HTTP/1.1 101 Switching\r\n", 0), None);
}

#[test]
fn scanning_can_resume_from_an_offset_without_missing_a_split_terminator() {
    let buffer = b"a\r\n\r\nb";
    // Resuming three bytes back from the end still catches the terminator.
    assert_eq!(find_head_end(buffer, buffer.len() - 6), Some(5));
}

#[test]
fn the_serialized_request_carries_the_upstream_host_and_the_handshake_headers() {
    let endpoint = Endpoint {
        scheme: crate::domain::model::Scheme::Ws,
        host: "chat.example.com".to_owned(),
        port: 8080,
    };
    let mut headers = upgrade_headers();
    headers.insert(
        HeaderName::from_static("sec-websocket-key"),
        HeaderValue::from_static("dGhlIHNhbXBsZSBub25jZQ=="),
    );
    headers.insert(header::HOST, HeaderValue::from_static("gateway.local"));

    let request = ProxyRequest {
        method: Method::GET,
        path: "/socket".to_owned(),
        query: vec![("room".to_owned(), "42".to_owned())],
        headers,
        body: Bytes::new(),
    };
    let wire = String::from_utf8(serialize_request(&endpoint, &request)).unwrap();

    assert!(wire.starts_with("GET /socket?room=42 HTTP/1.1\r\n"), "{wire}");
    // The inbound Host is replaced by the upstream authority, never forwarded.
    assert!(wire.contains("Host: chat.example.com:8080\r\n"), "{wire}");
    assert!(!wire.contains("gateway.local"), "{wire}");
    assert!(wire.contains("sec-websocket-key: dGhlIHNhbXBsZSBub25jZQ==\r\n"), "{wire}");
    assert!(wire.ends_with("\r\n\r\n"), "{wire}");
}
