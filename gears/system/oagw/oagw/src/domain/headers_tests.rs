//! Unit tests for header transformation.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::domain::model::{RequestHeaderRules, ResponseHeaderRules};
use std::collections::BTreeMap;

fn map(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
    entries
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

fn headers(entries: &[(&str, &str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in entries {
        map.insert(
            http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            http::HeaderValue::from_str(value).unwrap(),
        );
    }
    map
}

#[test]
fn hop_by_hop_set_is_complete() {
    assert_eq!(HOP_BY_HOP.len(), 8);
    for name in [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ] {
        assert!(is_hop_by_hop(name), "{name} should be hop-by-hop");
        assert!(is_hop_by_hop(&name.to_ascii_uppercase()));
    }
    assert!(!is_hop_by_hop("authorization"));
    assert_eq!(HopHeader::TransferEncoding.as_str(), "transfer-encoding");
}

#[test]
fn strip_removes_hop_by_hop_and_host() {
    let mut map = headers(&[
        ("host", "api.example.com"),
        ("connection", "keep-alive"),
        ("upgrade", "websocket"),
        ("authorization", "Bearer x"),
    ]);
    strip_hop_by_hop(&mut map);
    assert!(map.get("host").is_none());
    assert!(map.get("connection").is_none());
    assert!(map.get("upgrade").is_none());
    assert!(map.get("authorization").is_some());
}

#[test]
fn passthrough_none_drops_every_inbound_header() {
    let inbound = headers(&[("x-client", "1"), ("authorization", "Bearer x")]);
    let rules = RequestHeaderRules::default();
    let outbound = build_request_headers(&inbound, &HeadersConfig::default(), &rules).unwrap();
    assert!(outbound.is_empty());
}

#[test]
fn passthrough_allowlist_forwards_only_listed_headers() {
    let inbound = headers(&[
        ("x-forwarded-for", "10.0.0.1"),
        ("x-secret", "no"),
        ("connection", "keep-alive"),
    ]);
    let rules = RequestHeaderRules {
        passthrough: PassthroughMode::Allowlist,
        passthrough_allowlist: vec!["X-Forwarded-For".to_owned()],
        ..RequestHeaderRules::default()
    };
    let outbound = build_request_headers(&inbound, &HeadersConfig::default(), &rules).unwrap();
    assert_eq!(outbound.get("x-forwarded-for").unwrap(), "10.0.0.1");
    assert!(outbound.get("x-secret").is_none());
    assert!(outbound.get("connection").is_none());
}

#[test]
fn passthrough_all_forwards_everything_but_reserved_headers() {
    let inbound = headers(&[
        ("x-client", "1"),
        ("host", "caller.example"),
        ("x-oagw-target-host", "b.example.com"),
        ("content-length", "3"),
        ("x-request-id", "rid"),
    ]);
    let rules = RequestHeaderRules {
        passthrough: PassthroughMode::All,
        ..RequestHeaderRules::default()
    };
    let outbound = build_request_headers(&inbound, &HeadersConfig::default(), &rules).unwrap();
    assert_eq!(outbound.get("x-client").unwrap(), "1");
    assert!(outbound.get("host").is_none());
    assert!(outbound.get("x-oagw-target-host").is_none());
    assert!(outbound.get("content-length").is_none());
}

#[test]
fn request_rules_are_applied_in_order() {
    let inbound = headers(&[("x-drop-me", "1"), ("x-keep", "1")]);
    let rules = RequestHeaderRules {
        set: map(&[("x-set", "a")]),
        add: map(&[("x-add", "b")]),
        remove: vec!["x-drop-me".to_owned()],
        passthrough: PassthroughMode::All,
        ..RequestHeaderRules::default()
    };
    let outbound = build_request_headers(&inbound, &HeadersConfig::default(), &rules).unwrap();
    assert_eq!(outbound.get("x-set").unwrap(), "a");
    assert_eq!(outbound.get("x-add").unwrap(), "b");
    assert!(outbound.get("x-drop-me").is_none());
    assert_eq!(outbound.get("x-keep").unwrap(), "1");
}

#[test]
fn invalid_rules_are_rejected() {
    let bad_name = RequestHeaderRules {
        set: map(&[("bad name", "1")]),
        ..RequestHeaderRules::default()
    };
    assert!(
        build_request_headers(&HeaderMap::new(), &HeadersConfig::default(), &bad_name).is_err()
    );

    let bad_value = RequestHeaderRules {
        add: map(&[("x-ok", "bad\nvalue")]),
        ..RequestHeaderRules::default()
    };
    assert!(
        build_request_headers(&HeaderMap::new(), &HeadersConfig::default(), &bad_value).is_err()
    );
}

#[test]
fn response_rules_apply_and_content_length_is_dropped() {
    let upstream = headers(&[
        ("content-type", "application/json"),
        ("content-length", "12"),
        ("server", "unit"),
        ("connection", "close"),
    ]);
    let rules = ResponseHeaderRules {
        set: map(&[("x-resp", "s")]),
        add: map(&[("x-extra", "e")]),
        remove: vec!["server".to_owned()],
    };
    let outbound = build_response_headers(&upstream, &rules).unwrap();
    assert_eq!(outbound.get("x-resp").unwrap(), "s");
    assert_eq!(outbound.get("x-extra").unwrap(), "e");
    assert!(outbound.get("server").is_none());
    assert!(outbound.get("content-length").is_none());
    assert!(outbound.get("connection").is_none());
    assert_eq!(outbound.get("content-type").unwrap(), "application/json");
}
