//! Tests of the request and response header pipeline
//! (`cpt-cf-oagw-algo-request-proxy-header-transform`).

use crate::domain::headers::*;
use crate::domain::dto::{Endpoint, EndpointScheme, HeaderPassthrough, RequestHeaders, ResponseHeaders, HeadersConfig};
use std::collections::BTreeMap;

fn rules<const N: usize>(pairs: [(&str, &str); N]) -> BTreeMap<String, String> {
    pairs.iter().map(|(name, value)| ((*name).to_owned(), (*value).to_owned())).collect()
}

fn endpoint() -> Endpoint {
    Endpoint { scheme: EndpointScheme::Https, host: "api.vendor.com".to_owned(), port: 443 }
}

fn headers(config: HeadersConfig) -> Option<&'static HeadersConfig> {
    Some(Box::leak(Box::new(config)))
}

fn inbound(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs.iter().map(|(n, v)| ((*n).to_owned(), (*v).to_owned())).collect()
}

#[test]
fn the_hop_by_hop_set_is_the_rfc_9110_eight() {
    assert_eq!(HOP_BY_HOP.len(), 8);
    for name in ["connection", "keep-alive", "proxy-authenticate", "proxy-authorization", "te", "trailer", "transfer-encoding", "upgrade"] {
        assert!(is_hop_by_hop(name), "{name} is hop-by-hop");
        assert!(is_stripped_from_request(name));
        assert!(is_stripped_from_response(name));
    }
}

#[test]
fn the_routing_forwarding_host_and_length_headers_never_reach_the_upstream() {
    assert!(is_routing("x-oagw-target-host"));
    assert!(is_forwarded("x-forwarded-for"));
    assert!(is_stripped_from_request("host"));
    assert!(is_stripped_from_request("content-length"));
    assert!(is_stripped_from_request("x-oagw-target-host"));
}

#[test]
fn a_length_and_an_encoding_are_stripped_from_the_response_but_kept_from_the_request_set() {
    assert!(is_stripped_from_response("content-length"));
    assert!(is_stripped_from_response("content-encoding"));
    assert!(!is_stripped_from_response("x-request-id"));
}

#[test]
fn an_upgrade_is_detected_from_the_request_alone() {
    let upgrade = inbound(&[("connection", "keep-alive, Upgrade"), ("upgrade", "WebSocket")]);
    assert!(is_websocket_upgrade(&upgrade));
    let plain = inbound(&[("connection", "keep-alive"), ("upgrade", "h2c")]);
    assert!(!is_websocket_upgrade(&plain));
    let only_upgrade = inbound(&[("upgrade", "websocket")]);
    assert!(!is_websocket_upgrade(&only_upgrade));
}

#[test]
fn the_authority_carries_a_non_standard_port_only() {
    assert_eq!(authority_for(&endpoint()), "api.vendor.com");
    let mut custom = endpoint();
    custom.port = 8443;
    assert_eq!(authority_for(&custom), "api.vendor.com:8443");
}

#[test]
fn the_upstream_url_carries_the_query() {
    assert_eq!(
        upstream_url(&endpoint(), "/v1/orders", Some("limit=1")).expect("a url"),
        "https://api.vendor.com/v1/orders?limit=1"
    );
    assert_eq!(
        upstream_url(&endpoint(), "/v1/orders", None).expect("a url"),
        "https://api.vendor.com/v1/orders"
    );
}

#[test]
fn a_target_that_is_not_a_uri_is_rejected_before_any_connection() {
    let error = upstream_url(&endpoint(), "/v1/\r\nx", None).expect_err("a control character");
    assert!(format!("{error}").contains("path"));
}

#[test]
fn an_absent_passthrough_forwards_every_surviving_header() {
    let outbound =
        build_request_headers(&inbound(&[("x-a", "1"), ("connection", "keep-alive"), ("host", "client.dev")]), None, &endpoint(), false)
            .expect("the header set builds");
    assert_eq!(outbound.forwarded, vec![("x-a".to_owned(), "1".to_owned())]);
    assert_eq!(outbound.host, "api.vendor.com");
}

#[test]
fn the_none_passthrough_forwards_no_inbound_header() {
    let mut config = HeadersConfig::default();
    config.request = Some(RequestHeaders { passthrough: Some(HeaderPassthrough::None), ..Default::default() });
    let outbound = build_request_headers(&inbound(&[("x-a", "1"), ("x-b", "2")]), headers(config), &endpoint(), false)
        .expect("the header set builds");
    assert!(outbound.forwarded.is_empty());
}

#[test]
fn the_allowlist_passthrough_forwards_exactly_the_named_headers() {
    let mut config = HeadersConfig::default();
    config.request = Some(RequestHeaders {
        passthrough: Some(HeaderPassthrough::Allowlist),
        passthrough_allowlist: Some(vec!["X-A".to_owned()]),
        ..Default::default()
    });
    let outbound = build_request_headers(&inbound(&[("x-a", "1"), ("x-b", "2")]), headers(config), &endpoint(), false)
        .expect("the header set builds");
    assert_eq!(outbound.forwarded, vec![("x-a".to_owned(), "1".to_owned())]);
}

#[test]
fn the_ws_handshake_headers_are_replaced_not_stripped() {
    let request = inbound(&[("connection", "Upgrade"), ("upgrade", "websocket"), ("x-a", "1"), ("te", "trailers")]);
    let outbound = build_request_headers(&request, None, &endpoint(), true).expect("the header set builds");
    assert!(outbound.forwarded.contains(&("connection".to_owned(), "Upgrade".to_owned())));
    assert!(outbound.forwarded.contains(&("upgrade".to_owned(), "websocket".to_owned())));
    // The other hop-by-hop headers stay stripped on an upgrade request too.
    assert!(!outbound.forwarded.iter().any(|(name, _)| name == "te"));
}

#[test]
fn the_buffered_request_strips_the_handshake_headers() {
    let request = inbound(&[("connection", "Upgrade"), ("upgrade", "websocket")]);
    let outbound = build_request_headers(&request, None, &endpoint(), false).expect("the header set builds");
    assert!(outbound.forwarded.is_empty());
}

#[test]
fn set_add_and_remove_are_applied_in_that_order() {
    let mut config = HeadersConfig::default();
    config.request = Some(RequestHeaders {
        set: Some(rules([("x-set", "set")])),
        add: Some(rules([("x-add", "1")])),
        remove: Some(vec!["x-drop".to_owned()]),
        ..Default::default()
    });
    let outbound = build_request_headers(
        &inbound(&[("x-set", "inbound"), ("x-drop", "1")]),
        headers(config),
        &endpoint(),
        false,
    )
    .expect("the header set builds");
    assert!(outbound.forwarded.contains(&("x-set".to_owned(), "set".to_owned())));
    assert!(outbound.forwarded.contains(&("x-add".to_owned(), "1".to_owned())));
    assert!(!outbound.forwarded.iter().any(|(name, _)| name == "x-drop"));
}

#[test]
fn a_header_value_carrying_a_control_character_is_rejected() {
    let error = build_request_headers(&inbound(&[("x-a", "bad\rvalue")]), None, &endpoint(), false)
        .expect_err("a control character is rejected");
    assert!(format!("{error}").contains("x-a"));
}

#[test]
fn the_response_pipeline_strips_and_applies_its_rules() {
    let mut config = HeadersConfig::default();
    config.response = Some(ResponseHeaders {
        set: None,
        add: Some(rules([("x-add", "1")])),
        remove: Some(vec!["x-drop".to_owned()]),
    });
    let upstream = inbound(&[("content-type", "text/plain"), ("content-length", "3"), ("content-encoding", "gzip"), ("x-keep", "1"), ("connection", "close"), ("x-drop", "1")]);
    let out = build_response_headers(&upstream, headers(config));
    assert!(out.contains(&("x-keep".to_owned(), "1".to_owned())));
    assert!(out.contains(&("x-add".to_owned(), "1".to_owned())));
    assert!(!out.iter().any(|(name, _)| name == "content-length" || name == "content-encoding" || name == "connection" || name == "x-drop"));
}

#[test]
fn a_response_head_is_stripped_even_when_the_body_is_streamed() {
    let upstream = inbound(&[("transfer-encoding", "chunked"), ("trailer", "x-sum"), ("content-type", "text/event-stream")]);
    let out = build_response_headers(&upstream, None);
    assert_eq!(out, vec![("content-type".to_owned(), "text/event-stream".to_owned())]);
}
